//! Small, uncached cover requests for native visualiser colour extraction.
use std::{io::Read, time::Duration};

use anyhow::{Context, Result, ensure};

pub(crate) const MAX_COVER_BYTES: usize = 2 * 1024 * 1024;

pub(crate) fn fetch_cover_art_bytes(url: &str) -> Result<Vec<u8>> {
    if let Some(raw_path) = url.strip_prefix("file://") {
        // Offline metadata currently contains literal paths, not encoded URLs.
        // Prefer that exact path, then also accept proper percent-encoded URLs.
        ensure!(
            raw_path.starts_with('/'),
            "Remote file hosts are not supported"
        );
        let literal = std::path::PathBuf::from(raw_path);
        let path = if literal.exists() {
            literal
        } else {
            reqwest::Url::parse(url)?
                .to_file_path()
                .ok()
                .context("Invalid local cover path")?
        };
        ensure!(
            std::fs::metadata(&path)?.is_file(),
            "Cover must be a regular file"
        );
        let file = std::fs::File::open(path)?;
        let length = file.metadata()?.len();
        return read_cover(file, Some(length));
    }
    let url = reqwest::Url::parse(url)?;
    ensure!(
        matches!(url.scheme(), "https" | "http"),
        "Invalid cover URL"
    );
    // Build and drop this blocking client on the dedicated native colour worker.
    // Avoid the UI thumbnail cache: only the twelve tiny palettes are retained.
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(6))
        .redirect(reqwest::redirect::Policy::limited(3))
        .build()?;
    let response = client.get(url).send()?.error_for_status()?;
    let length = response.content_length();
    read_cover(response, length)
}

fn read_cover(reader: impl Read, length: Option<u64>) -> Result<Vec<u8>> {
    ensure!(
        length.is_none_or(|size| size <= MAX_COVER_BYTES as u64),
        "Cover image is too large"
    );
    let mut bytes = Vec::new();
    reader
        .take((MAX_COVER_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= MAX_COVER_BYTES, "Cover image is too large");
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_bound_applies_to_known_and_unknown_body_lengths() {
        assert!(read_cover(std::io::empty(), Some(MAX_COVER_BYTES as u64 + 1)).is_err());
        assert!(read_cover(std::io::repeat(0), None).is_err());
        assert_eq!(
            read_cover(&b"small cover"[..], None).unwrap(),
            b"small cover"
        );
    }

    #[test]
    fn offline_cover_handles_literal_and_encoded_paths() {
        let dir = std::env::temp_dir().join(format!("static-cover-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("art # one.png");
        std::fs::write(&path, b"cover").unwrap();
        assert_eq!(
            fetch_cover_art_bytes(&format!("file://{}", path.display())).unwrap(),
            b"cover"
        );
        let encoded = reqwest::Url::from_file_path(&path).unwrap();
        assert_eq!(fetch_cover_art_bytes(encoded.as_str()).unwrap(), b"cover");
        assert!(fetch_cover_art_bytes("file://other-host/cover.png").is_err());
        assert!(fetch_cover_art_bytes(&format!("file://{}", dir.display())).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
