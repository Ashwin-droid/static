use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use reqwest::{
    StatusCode,
    blocking::{Client as BlockingClient, Response},
    header::{CONTENT_RANGE, CONTENT_TYPE, RANGE},
};

macro_rules! eprintln {
    ($($arg:tt)*) => {{
        crate::app_log!($($arg)*);
    }};
}

static DOWNLOAD_CLIENT: LazyLock<BlockingClient> = LazyLock::new(|| {
    BlockingClient::builder()
        .timeout(Duration::from_secs(120))
        .user_agent("Mozilla/5.0 (X11; Linux x86_64; rv:136.0) Gecko/20100101 Firefox/136.0")
        .http1_only()
        .build()
        .unwrap()
});

static THUMB_CLIENT: LazyLock<BlockingClient> = LazyLock::new(|| {
    BlockingClient::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap()
});

static THUMBNAIL_BYTES_CACHE: LazyLock<Mutex<HashMap<String, Vec<u8>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

const MEDIA_INITIAL_CHUNK_BYTES: u64 = 512 * 1024;
const MEDIA_MIN_CHUNK_BYTES: u64 = 64 * 1024;
const MEDIA_ALT_URL_FOLLOW_LIMIT: usize = 3;
const TEXT_MEDIA_ERROR_LIMIT_BYTES: usize = 16 * 1024;

pub fn download_media_bytes(url: &str, range_header: Option<&str>) -> Result<Vec<u8>> {
    download_media_bytes_inner(url, range_header, 0)
}

fn download_media_bytes_inner(
    url: &str,
    range_header: Option<&str>,
    follow_depth: usize,
) -> Result<Vec<u8>> {
    if range_header == Some("bytes=0-") && media_url_is_web_remix(url) {
        return download_media_bytes_chunked(url, follow_depth).with_context(|| {
            format!(
                "WEB_REMIX chunked media download failed for {}",
                media_url_for_log(url)
            )
        });
    }

    let response = send_media_request(url, range_header)?;
    let status = response.status();
    if !status.is_success() {
        if status == StatusCode::FORBIDDEN && range_header == Some("bytes=0-") {
            return download_media_bytes_chunked(url, follow_depth).with_context(|| {
                format!(
                    "media server rejected open-ended range for {}; chunked fallback failed",
                    media_url_for_log(url)
                )
            });
        }
        return Err(media_status_error(response, url));
    }

    let content_type = response_content_type(&response);
    let bytes = response
        .bytes()
        .with_context(|| format!("Failed to read media bytes from {}", media_url_for_log(url)))?
        .to_vec();

    if let Some(next_url) = alternate_media_url_from_bytes(&bytes) {
        ensure_can_follow_alternate_url(follow_depth, url)?;
        eprintln!(
            "[MEDIA] following alternate media URL from {}",
            media_url_for_log(url)
        );
        return download_media_bytes_chunked(&next_url, follow_depth + 1);
    }

    if text_media_response_without_alternate(&content_type, &bytes) {
        bail!(
            "media server returned text instead of audio for {}: {}",
            media_url_for_log(url),
            text_preview(&bytes)
        );
    }

    Ok(bytes)
}

fn send_media_request(url: &str, range_header: Option<&str>) -> Result<Response> {
    let mut request = DOWNLOAD_CLIENT.get(url);
    if let Some(range_header) = range_header {
        request = request.header(RANGE, range_header);
    }

    request.send().with_context(|| {
        format!(
            "Failed to request media bytes from {}",
            media_url_for_log(url)
        )
    })
}

fn download_media_bytes_chunked(url: &str, follow_depth: usize) -> Result<Vec<u8>> {
    let mut audio = Vec::new();
    let mut total_len = None;
    let mut chunk_bytes = MEDIA_INITIAL_CHUNK_BYTES;

    loop {
        let start = audio.len() as u64;
        let end = start.saturating_add(chunk_bytes - 1);
        let range_header = format!("bytes={start}-{end}");
        let response = send_media_request(url, Some(&range_header))?;
        let status = response.status();
        if !status.is_success() {
            if status == StatusCode::FORBIDDEN {
                if let Some(smaller_chunk_bytes) = smaller_media_chunk_size(chunk_bytes) {
                    eprintln!(
                        "[MEDIA] media server rejected {} KiB range for {}; retrying with {} KiB",
                        chunk_bytes / 1024,
                        media_url_for_log(url),
                        smaller_chunk_bytes / 1024
                    );
                    chunk_bytes = smaller_chunk_bytes;
                    continue;
                }
            }
            return Err(media_status_error(response, url));
        }

        let content_type = response_content_type(&response);
        if status == StatusCode::PARTIAL_CONTENT {
            total_len = response
                .headers()
                .get(CONTENT_RANGE)
                .and_then(|value| value.to_str().ok())
                .and_then(content_range_total);
        }
        let chunk = response
            .bytes()
            .with_context(|| {
                format!(
                    "Failed to read chunked media bytes from {}",
                    media_url_for_log(url)
                )
            })?
            .to_vec();

        if start == 0 {
            if let Some(next_url) = alternate_media_url_from_bytes(&chunk) {
                ensure_can_follow_alternate_url(follow_depth, url)?;
                eprintln!(
                    "[MEDIA] following alternate media URL from {}",
                    media_url_for_log(url)
                );
                return download_media_bytes_chunked(&next_url, follow_depth + 1);
            }

            if text_media_response_without_alternate(&content_type, &chunk) {
                bail!(
                    "media server returned text instead of audio for {}: {}",
                    media_url_for_log(url),
                    text_preview(&chunk)
                );
            }

            if status != StatusCode::PARTIAL_CONTENT {
                return Ok(chunk);
            }
        }

        if chunk.is_empty() {
            bail!(
                "media server returned an empty chunk for {} at byte {start}",
                media_url_for_log(url)
            );
        }
        audio.extend_from_slice(&chunk);

        if total_len.is_some_and(|len| audio.len() as u64 >= len) {
            return Ok(audio);
        }
        if total_len.is_none() && chunk.len() < chunk_bytes as usize {
            return Ok(audio);
        }
    }
}

fn smaller_media_chunk_size(current: u64) -> Option<u64> {
    (current > MEDIA_MIN_CHUNK_BYTES).then(|| (current / 2).max(MEDIA_MIN_CHUNK_BYTES))
}

fn media_status_error(response: Response, url: &str) -> anyhow::Error {
    let status = response.status();
    let body_preview = response
        .text()
        .unwrap_or_default()
        .trim()
        .chars()
        .take(160)
        .collect::<String>();
    if body_preview.is_empty() {
        return anyhow!(
            "media server returned {status} for {}",
            media_url_for_log(url)
        );
    }
    anyhow!(
        "media server returned {status} for {}: {body_preview}",
        media_url_for_log(url)
    )
}

fn ensure_can_follow_alternate_url(follow_depth: usize, url: &str) -> Result<()> {
    if follow_depth >= MEDIA_ALT_URL_FOLLOW_LIMIT {
        bail!(
            "media server returned too many alternate URLs for {}",
            media_url_for_log(url)
        );
    }
    Ok(())
}

fn response_content_type(response: &Response) -> Option<String> {
    response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_ascii_lowercase())
}

fn content_range_total(value: &str) -> Option<u64> {
    let (_, total) = value.rsplit_once('/')?;
    (total != "*").then(|| total.parse().ok()).flatten()
}

fn alternate_media_url_from_bytes(bytes: &[u8]) -> Option<String> {
    if bytes.len() > TEXT_MEDIA_ERROR_LIMIT_BYTES {
        return None;
    }
    let value = std::str::from_utf8(bytes).ok()?.trim();
    if !(value.starts_with("https://") || value.starts_with("http://")) {
        return None;
    }
    let parsed = reqwest::Url::parse(value).ok()?;
    let is_googlevideo = parsed
        .host_str()
        .is_some_and(|host| host.ends_with(".googlevideo.com") || host == "googlevideo.com");
    (is_googlevideo && parsed.path().contains("/videoplayback")).then(|| value.to_string())
}

fn media_url_is_web_remix(url: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    parsed
        .query_pairs()
        .any(|(key, value)| key == "c" && value.eq_ignore_ascii_case("WEB_REMIX"))
}

fn text_media_response_without_alternate(content_type: &Option<String>, bytes: &[u8]) -> bool {
    bytes.len() <= TEXT_MEDIA_ERROR_LIMIT_BYTES
        && content_type
            .as_deref()
            .is_some_and(|value| value.starts_with("text/"))
}

fn text_preview(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .trim()
        .chars()
        .take(160)
        .collect()
}

fn media_url_for_log(url: &str) -> String {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return "media URL".to_string();
    };
    let host = parsed.host_str().unwrap_or("media-host");
    let mut safe_query = Vec::new();
    for (key, value) in parsed.query_pairs() {
        if matches!(
            key.as_ref(),
            "itag" | "mime" | "dur" | "clen" | "source" | "c"
        ) {
            safe_query.push(format!("{key}={value}"));
        }
    }
    if safe_query.is_empty() {
        host.to_string()
    } else {
        format!("{host}?{}", safe_query.join("&"))
    }
}

pub fn fetch_thumbnail_bytes(url: &str) -> Option<Vec<u8>> {
    const MAX_ATTEMPTS: usize = 3;
    const BACKOFF_MS: [u64; MAX_ATTEMPTS] = [250, 700, 1500];

    if let Some(path) = url.strip_prefix("file://") {
        return std::fs::read(path).ok();
    }

    if let Some(bytes) = THUMBNAIL_BYTES_CACHE.lock().unwrap().get(url).cloned() {
        return Some(bytes);
    }

    for attempt in 0..MAX_ATTEMPTS {
        let response = THUMB_CLIENT.get(url).send();
        match response
            .and_then(|resp| resp.error_for_status())
            .and_then(|resp| resp.bytes())
        {
            Ok(bytes) => {
                let bytes = bytes.to_vec();
                THUMBNAIL_BYTES_CACHE
                    .lock()
                    .unwrap()
                    .insert(url.to_string(), bytes.clone());
                return Some(bytes);
            }
            Err(err) => {
                eprintln!(
                    "[ART] thumbnail fetch failed (attempt {}/{}): {} ({err})",
                    attempt + 1,
                    MAX_ATTEMPTS,
                    url
                );
                if attempt + 1 < MAX_ATTEMPTS {
                    std::thread::sleep(std::time::Duration::from_millis(BACKOFF_MS[attempt]));
                }
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::{
        alternate_media_url_from_bytes, content_range_total, media_url_for_log,
        media_url_is_web_remix, smaller_media_chunk_size,
    };

    #[test]
    fn media_log_url_keeps_shape_without_signed_params() {
        let rendered = media_url_for_log(
            "https://rr1---sn-ci5gup-2o9l.googlevideo.com/videoplayback?expire=1&ip=secret&itag=774&mime=audio%2Fwebm&sig=signed&clen=1234&c=WEB_REMIX",
        );

        assert_eq!(
            rendered,
            "rr1---sn-ci5gup-2o9l.googlevideo.com?itag=774&mime=audio/webm&clen=1234&c=WEB_REMIX"
        );
    }

    #[test]
    fn alternate_media_url_is_detected_from_text_body() {
        let url = "https://rr2---sn-ci5gup-cvhee.googlevideo.com/videoplayback?expire=1&itag=141&sig=signed";

        assert_eq!(
            alternate_media_url_from_bytes(url.as_bytes()),
            Some(url.to_string())
        );
        assert_eq!(
            alternate_media_url_from_bytes(b"https://example.com/videoplayback?itag=141"),
            None
        );
        assert_eq!(alternate_media_url_from_bytes(b"not a url"), None);
    }

    #[test]
    fn web_remix_media_urls_are_identified_from_client_query() {
        assert!(media_url_is_web_remix(
            "https://rr1---sn-ci5gup-2o9l.googlevideo.com/videoplayback?itag=251&c=WEB_REMIX"
        ));
        assert!(media_url_is_web_remix(
            "https://rr1---sn-ci5gup-2o9l.googlevideo.com/videoplayback?itag=251&c=web_remix"
        ));
        assert!(!media_url_is_web_remix(
            "https://rr1---sn-ci5gup-2o9l.googlevideo.com/videoplayback?itag=251&c=ANDROID_VR"
        ));
    }

    #[test]
    fn chunk_size_retry_stops_at_minimum() {
        assert_eq!(smaller_media_chunk_size(512 * 1024), Some(256 * 1024));
        assert_eq!(smaller_media_chunk_size(128 * 1024), Some(64 * 1024));
        assert_eq!(smaller_media_chunk_size(64 * 1024), None);
    }

    #[test]
    fn content_range_total_parses_known_total() {
        assert_eq!(content_range_total("bytes 0-1023/5027291"), Some(5027291));
        assert_eq!(content_range_total("bytes 0-1023/*"), None);
        assert_eq!(content_range_total("invalid"), None);
    }
}
