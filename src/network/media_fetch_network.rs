use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::blocking::Client as BlockingClient;

macro_rules! eprintln {
    ($($arg:tt)*) => {{
        crate::app_log!($($arg)*);
    }};
}

static DOWNLOAD_CLIENT: LazyLock<BlockingClient> = LazyLock::new(|| {
    BlockingClient::builder()
        .timeout(Duration::from_secs(120))
        .user_agent("Mozilla/5.0 (X11; Linux x86_64; rv:136.0) Gecko/20100101 Firefox/136.0")
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

pub fn download_media_bytes(url: &str, range_header: Option<&str>) -> Result<Vec<u8>> {
    let mut request = DOWNLOAD_CLIENT.get(url);
    if let Some(range_header) = range_header {
        request = request.header("Range", range_header);
    }

    request
        .send()
        .and_then(|response| response.error_for_status())
        .and_then(|response| response.bytes())
        .map(|bytes| bytes.to_vec())
        .with_context(|| format!("Failed to download media bytes from {url}"))
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
