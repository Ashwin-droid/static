use std::collections::HashMap;
use std::io::{Read, Write};
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

/// Stream complete media to a writer with bounded memory, including servers which
/// ignore Range. The caller must discard its output if this returns an error.
pub fn stream_media_to_writer(url: &str, writer: &mut dyn Write) -> Result<u64> {
    stream_media_inner(url, writer, 0)
}

fn stream_media_inner(url: &str, writer: &mut dyn Write, depth: usize) -> Result<u64> {
    let mut written = 0u64;
    let mut total = None;
    let mut chunk_size = MEDIA_INITIAL_CHUNK_BYTES;
    loop {
        let end = media_chunk_end(written, chunk_size, total);
        let mut response = send_media_request(url, Some(&format!("bytes={written}-{end}")))?;
        let status = response.status();
        if status == StatusCode::FORBIDDEN {
            if let Some(smaller) = smaller_media_chunk_size(chunk_size) {
                chunk_size = smaller;
                continue;
            }
        }
        if !status.is_success() {
            let mut preview = Vec::new();
            response.take(160).read_to_end(&mut preview)?;
            bail!("media server returned {status}: {}", text_preview(&preview));
        }
        let expected = if status == StatusCode::PARTIAL_CONTENT {
            let header = response
                .headers()
                .get(CONTENT_RANGE)
                .and_then(|value| value.to_str().ok())
                .context("partial media response missing Content-Range")?;
            let (start, finish, length) = parse_stream_content_range(header)?;
            if start != written || finish > end || total.is_some_and(|old| old != length) {
                bail!("inconsistent media Content-Range");
            }
            total = Some(length);
            Some(finish - start + 1)
        } else if status == StatusCode::OK && written == 0 {
            response.content_length()
        } else {
            bail!("media server ignored a continuation range");
        };

        // Some Google media endpoints return another URL as a short text body.
        // Inspect only a bounded prefix before committing any bytes to the file.
        let content_type = response_content_type(&response);
        let mut prefix = Vec::with_capacity(TEXT_MEDIA_ERROR_LIMIT_BYTES + 1);
        response
            .by_ref()
            .take((TEXT_MEDIA_ERROR_LIMIT_BYTES + 1) as u64)
            .read_to_end(&mut prefix)?;
        if written == 0 {
            if let Some(next_url) = alternate_media_url_from_bytes(&prefix) {
                ensure_can_follow_alternate_url(depth, url)?;
                return stream_media_inner(&next_url, writer, depth + 1);
            }
        }
        if content_type
            .as_deref()
            .is_some_and(|value| value.starts_with("text/"))
        {
            bail!(
                "media server returned text instead of audio: {}",
                text_preview(&prefix)
            );
        }
        if expected.is_some_and(|count| prefix.len() as u64 > count) {
            bail!("media response exceeds declared range");
        }
        writer.write_all(&prefix)?;
        let remaining = expected.map(|count| count - prefix.len() as u64);
        let copied = if let Some(remaining) = remaining {
            let copied = std::io::copy(&mut response.by_ref().take(remaining), writer)?;
            let mut extra = [0u8; 1];
            if copied != remaining || response.read(&mut extra)? != 0 {
                bail!("media response length differs from declared range");
            }
            copied
        } else {
            std::io::copy(&mut response, writer)?
        };
        let count = prefix.len() as u64 + copied;
        if count == 0 {
            bail!("media server returned empty audio");
        }
        written = written
            .checked_add(count)
            .context("media length overflow")?;
        if status == StatusCode::OK || total == Some(written) {
            return Ok(written);
        }
    }
}

fn parse_stream_content_range(header: &str) -> Result<(u64, u64, u64)> {
    let (range, total) = header
        .strip_prefix("bytes ")
        .and_then(|value| value.split_once('/'))
        .context("invalid media Content-Range")?;
    let (start, end) = range.split_once('-').context("invalid media byte range")?;
    let (start, end, total): (u64, u64, u64) = (start.parse()?, end.parse()?, total.parse()?);
    if start > end || end >= total {
        bail!("invalid media Content-Range bounds");
    }
    Ok((start, end, total))
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
        if total_len.is_some_and(|len| start >= len) {
            return Ok(audio);
        }
        let end = media_chunk_end(start, chunk_bytes, total_len);
        let range_header = format!("bytes={start}-{end}");
        let response = send_media_request(url, Some(&range_header))?;
        let status = response.status();
        if !status.is_success() {
            if status == StatusCode::FORBIDDEN {
                if let Some(smaller_chunk_bytes) = smaller_media_chunk_size(chunk_bytes) {
                    eprintln!(
                        "[MEDIA] media server rejected range {range_header} ({} KiB chunk) for {}; retrying with {} KiB",
                        chunk_bytes / 1024,
                        media_url_for_log(url),
                        smaller_chunk_bytes / 1024
                    );
                    chunk_bytes = smaller_chunk_bytes;
                    continue;
                }
                eprintln!(
                    "[MEDIA] media server rejected minimum range {range_header} ({} KiB chunk) for {}",
                    chunk_bytes / 1024,
                    media_url_for_log(url)
                );
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

fn media_chunk_end(start: u64, chunk_bytes: u64, total_len: Option<u64>) -> u64 {
    let uncapped_end = start.saturating_add(chunk_bytes - 1);
    match total_len {
        Some(len) => uncapped_end.min(len.saturating_sub(1)),
        None => uncapped_end,
    }
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
        alternate_media_url_from_bytes, content_range_total, media_chunk_end, media_url_for_log,
        media_url_is_web_remix, smaller_media_chunk_size,
    };

    fn serve_stream_responses(
        responses: Vec<(String, Vec<u8>)>,
    ) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let worker = std::thread::spawn(move || {
            for (headers, body) in responses {
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                let mut stream = loop {
                    match listener.accept() {
                        Ok((stream, _)) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(
                                std::time::Instant::now() < deadline,
                                "missing media request"
                            );
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                        Err(error) => panic!("{error}"),
                    }
                };
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(3)))
                    .unwrap();
                let mut request = Vec::new();
                let mut byte = [0u8; 1];
                while !request.ends_with(b"\r\n\r\n") {
                    assert_eq!(stream.read(&mut byte).unwrap(), 1);
                    request.push(byte[0]);
                }
                assert!(
                    String::from_utf8_lossy(&request)
                        .to_lowercase()
                        .contains("range: bytes=")
                );
                let _ = stream.write_all(headers.as_bytes());
                let _ = stream.write_all(&body);
            }
        });
        (
            format!("http://{address}/videoplayback?c=WEB_REMIX"),
            worker,
        )
    }

    #[test]
    fn streaming_media_handles_ignored_ranges_without_whole_body_buffering() {
        struct BoundedWriter {
            length: u64,
        }
        impl std::io::Write for BoundedWriter {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                assert!(bytes.len() <= super::TEXT_MEDIA_ERROR_LIMIT_BYTES + 1);
                assert!(bytes.iter().all(|value| *value == 0x71));
                self.length += bytes.len() as u64;
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let length = 2 * 1024 * 1024;
        let (url, server) = serve_stream_responses(vec![(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: audio/webm\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
            ),
            vec![0x71; length],
        )]);
        let mut writer = BoundedWriter { length: 0 };
        assert_eq!(
            super::stream_media_to_writer(&url, &mut writer).unwrap(),
            length as u64
        );
        assert_eq!(writer.length, length as u64);
        server.join().unwrap();
    }

    #[test]
    fn streaming_media_retries_forbidden_ranges_and_validates_continuations() {
        let first = 256 * 1024;
        let total = first + 7;
        let (url, server) = serve_stream_responses(vec![
            (
                "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into(),
                vec![],
            ),
            (
                format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-{}/{total}\r\nContent-Length: {first}\r\nConnection: close\r\n\r\n",
                    first - 1
                ),
                vec![0x41; first],
            ),
            (
                format!(
                    "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {first}-{}/{total}\r\nContent-Length: 7\r\nConnection: close\r\n\r\n",
                    total - 1
                ),
                vec![0x42; 7],
            ),
        ]);
        let mut audio = Vec::new();
        assert_eq!(
            super::stream_media_to_writer(&url, &mut audio).unwrap(),
            total as u64
        );
        assert_eq!(&audio[first..], &[0x42; 7]);
        server.join().unwrap();
    }

    #[test]
    fn streaming_media_rejects_bad_ranges_truncation_and_text() {
        for (headers, body) in [
            (
                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 1-4/5\r\nContent-Length: 4\r\nConnection: close\r\n\r\n",
                b"abcd".to_vec(),
            ),
            (
                "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-4/5\r\nContent-Length: 4\r\nConnection: close\r\n\r\n",
                b"abcd".to_vec(),
            ),
            (
                "HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\n",
                b"abcd".to_vec(),
            ),
            (
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 4\r\nConnection: close\r\n\r\n",
                b"oops".to_vec(),
            ),
        ] {
            let (url, server) = serve_stream_responses(vec![(headers.into(), body)]);
            assert!(super::stream_media_to_writer(&url, &mut Vec::new()).is_err());
            server.join().unwrap();
        }
        assert!(super::parse_stream_content_range("bytes 0-4/*").is_err());
        assert!(super::parse_stream_content_range("bytes 4-2/10").is_err());
        assert!(super::parse_stream_content_range("bytes 0-10/10").is_err());
    }

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
    fn media_chunk_end_caps_to_known_total_length() {
        assert_eq!(
            media_chunk_end(3_670_016, 512 * 1024, Some(3_901_969)),
            3_901_968
        );
        assert_eq!(
            media_chunk_end(524_288, 512 * 1024, Some(3_901_969)),
            1_048_575
        );
        assert_eq!(media_chunk_end(3_670_016, 512 * 1024, None), 4_194_303);
    }

    #[test]
    fn content_range_total_parses_known_total() {
        assert_eq!(content_range_total("bytes 0-1023/5027291"), Some(5027291));
        assert_eq!(content_range_total("bytes 0-1023/*"), None);
        assert_eq!(content_range_total("invalid"), None);
    }
}
