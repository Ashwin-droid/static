use super::*;

pub(super) fn load_thumbnail_into(picture: &Picture, url: &str, size: i32) {
    if let Some(texture) = cached_thumbnail_texture(url, size) {
        picture.set_paintable(Some(&texture));
        return;
    }

    let picture = picture.clone();
    let url = url.to_string();

    glib::spawn_future_local(async move {
        let fetch_url = url.clone();
        let result = gio::spawn_blocking(move || {
            if let Some(path) = fetch_url.strip_prefix("file://") {
                std::fs::read(path).map_err(|e| e.to_string())
            } else {
                fetch_thumbnail_bytes(&fetch_url)
                    .ok_or_else(|| format!("failed to load thumbnail after retries: {fetch_url}"))
            }
        })
        .await;

        if let Ok(Ok(bytes)) = result {
            if let Some(texture) = build_thumbnail_texture(&bytes, size) {
                store_thumbnail_texture(&url, size, &texture);
                picture.set_paintable(Some(&texture));
            }
        }
    });
}

pub(super) fn fetch_thumbnail_bytes(url: &str) -> Option<Vec<u8>> {
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

pub(super) fn build_thumbnail_texture(bytes: &[u8], size: i32) -> Option<gtk::gdk::Texture> {
    let gbytes = glib::Bytes::from(bytes);
    let stream = gio::MemoryInputStream::from_bytes(&gbytes);
    let pixbuf = gtk::gdk_pixbuf::Pixbuf::from_stream(&stream, gio::Cancellable::NONE).ok()?;
    let w = pixbuf.width();
    let h = pixbuf.height();
    let (sw, sh) = if w < h {
        (size, (h as f64 * size as f64 / w as f64) as i32)
    } else {
        ((w as f64 * size as f64 / h as f64) as i32, size)
    };
    let scaled = pixbuf
        .scale_simple(sw, sh, gtk::gdk_pixbuf::InterpType::Bilinear)
        .unwrap_or(pixbuf);
    let ox = (scaled.width() - size).max(0) / 2;
    let oy = (scaled.height() - size).max(0) / 2;
    let cropped = scaled.new_subpixbuf(ox, oy, size.min(scaled.width()), size.min(scaled.height()));
    Some(gtk::gdk::Texture::for_pixbuf(&cropped))
}

pub(super) fn cached_thumbnail_texture(url: &str, size: i32) -> Option<gtk::gdk::Texture> {
    THUMBNAIL_TEXTURE_CACHE.with(|cache| cache.borrow().get(&(url.to_string(), size)).cloned())
}

pub(super) fn store_thumbnail_texture(url: &str, size: i32, texture: &gtk::gdk::Texture) {
    THUMBNAIL_TEXTURE_CACHE.with(|cache| {
        cache
            .borrow_mut()
            .insert((url.to_string(), size), texture.clone());
    });
}

pub(super) fn history_thumbnail_candidate(url: Option<&str>) -> Option<&str> {
    url.filter(|value| !value.starts_with("file://"))
}

pub(super) fn note_playback_position(ps: &PlayerState, position_millis: u64) {
    if position_millis == 0 {
        return;
    }

    let mut now_playing = ps.now_playing_track.borrow_mut();
    let Some(track) = now_playing.as_mut() else {
        return;
    };
    track.last_observed_position_millis = position_millis;
}
