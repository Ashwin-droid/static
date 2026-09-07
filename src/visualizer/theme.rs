//! Material You's Celebi -> Score -> HCT pipeline, entirely on a native worker.
use std::{
    collections::VecDeque,
    io::Cursor,
    sync::{Arc, Condvar, Mutex},
    thread,
};

use anyhow::{Result, ensure};
use image::{ImageReader, imageops::FilterType};
use material_colors::{
    color::Argb,
    hct::Hct,
    palette::TonalPalette,
    quantize::{Quantizer, QuantizerCelebi},
    score::Score,
};
use serde::Serialize;
use serde_json::json;
use tokio::sync::watch;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Theme {
    name: &'static str,
    ui_theme: UiTheme,
    graph_theme: GraphTheme,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct UiTheme {
    primary: String,
    secondary: String,
    background: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
struct GraphTheme {
    line1: String,
    line2: String,
}

fn tone(hue: f64, chroma: f64, lightness: i32) -> String {
    TonalPalette::of(hue, chroma)
        .tone(lightness)
        .to_hex_with_pound()
}

fn hue_distance(a: f64, b: f64) -> f64 {
    let distance = (a - b).abs() % 360.0;
    distance.min(360.0 - distance)
}

fn theme_from_pixels(pixels: &[Argb]) -> Option<Theme> {
    if pixels.is_empty() {
        return None;
    }
    // Population and perceptual chroma select actual cover colours; hue averaging
    // would incorrectly turn a two-colour cover into an unrelated third colour.
    let quantized = QuantizerCelebi::quantize(pixels, 128);
    let colorful = quantized.color_to_count.iter().any(|(color, count)| {
        *count as usize * 100 >= pixels.len() && Hct::new(*color).get_chroma() >= 8.0
    });
    if !colorful {
        // Black-and-white artwork remains neutral instead of acquiring the
        // quantizer's arbitrary blue fallback. Graph traces differ by lightness.
        return Some(Theme {
            name: "Cover art",
            ui_theme: UiTheme {
                primary: tone(0.0, 0.0, 82),
                secondary: tone(0.0, 0.0, 70),
                background: tone(0.0, 0.0, 6),
            },
            graph_theme: GraphTheme {
                line1: tone(0.0, 0.0, 86),
                line2: tone(0.0, 0.0, 56),
            },
        });
    }
    let ranked = Score::score(&quantized.color_to_count, Some(4), None, Some(true));
    let primary = Hct::new(ranked[0]);
    let accent = ranked
        .iter()
        .skip(1)
        .map(|color| Hct::new(*color))
        .find(|hct| hue_distance(primary.get_hue(), hct.get_hue()) >= 40.0);
    // Like Material's tertiary palette, a monochromatic cover receives a
    // related accent. Multicolour covers preferentially use a second real hue.
    let accent_hue = accent.map_or((primary.get_hue() + 60.0) % 360.0, |hct| hct.get_hue());
    let accent_chroma = accent.map_or(40.0, |hct| hct.get_chroma().clamp(24.0, 56.0));
    Some(Theme {
        name: "Cover art",
        ui_theme: UiTheme {
            primary: tone(
                primary.get_hue(),
                primary.get_chroma().clamp(36.0, 64.0),
                80,
            ),
            secondary: tone(accent_hue, accent_chroma.min(40.0), 70),
            background: tone(primary.get_hue(), 8.0, 6),
        },
        graph_theme: GraphTheme {
            line1: tone(
                primary.get_hue(),
                primary.get_chroma().clamp(40.0, 72.0),
                78,
            ),
            line2: tone(accent_hue, accent_chroma, 62),
        },
    })
}

fn from_bytes(bytes: &[u8]) -> Result<Option<Theme>> {
    ensure!(
        bytes.len() <= crate::network::MAX_COVER_BYTES,
        "Cover image is too large"
    );
    let reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let (width, height) = reader.into_dimensions()?;
    ensure!(
        width > 0 && height > 0 && width <= 4096 && height <= 4096,
        "Invalid cover dimensions"
    );
    let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    let image = reader
        .decode()?
        .resize(128, 128, FilterType::Nearest)
        .to_rgba8();
    let pixels: Vec<_> = image
        .pixels()
        .filter(|pixel| pixel[3] >= 240)
        .map(|pixel| Argb::new(255, pixel[0], pixel[1], pixel[2]))
        .collect();
    Ok(theme_from_pixels(&pixels))
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Key {
    video_id: Option<String>,
    url: Option<String>,
}

#[derive(Clone)]
struct Request {
    key: Key,
    generation: u64,
}

#[derive(Default)]
struct Slot {
    current: Option<Key>,
    generation: u64,
    pending: Option<Request>,
    stopped: bool,
}

fn message(key: &Key, theme: Option<&Theme>) -> String {
    json!({"type":"theme", "video_id":key.video_id, "theme":theme}).to_string()
}

/// One in-flight request and one replaceable pending request. The audio polling
/// thread only updates this slot; it never decodes images or waits on a network.
pub(super) struct ThemeWorker {
    slot: Arc<(Mutex<Slot>, Condvar)>,
    output: watch::Sender<String>,
    thread: Option<thread::JoinHandle<()>>,
}

impl ThemeWorker {
    pub fn start(output: watch::Sender<String>) -> std::io::Result<Self> {
        Self::with_loader(output, |url| {
            let bytes = crate::network::fetch_cover_art_bytes(url)?;
            from_bytes(&bytes)
        })
    }

    fn with_loader<F>(output: watch::Sender<String>, mut load: F) -> std::io::Result<Self>
    where
        F: FnMut(&str) -> Result<Option<Theme>> + Send + 'static,
    {
        let slot = Arc::new((Mutex::new(Slot::default()), Condvar::new()));
        let worker_slot = slot.clone();
        let worker_output = output.clone();
        let thread = thread::Builder::new()
            .name("visualiser-cover".into())
            .spawn(move || {
                let (mutex, wake) = &*worker_slot;
                let mut cache: VecDeque<(String, Option<Theme>)> = VecDeque::new();
                loop {
                    let request = {
                        let mut slot = mutex.lock().unwrap_or_else(|e| e.into_inner());
                        while slot.pending.is_none() && !slot.stopped {
                            slot = wake.wait(slot).unwrap_or_else(|e| e.into_inner());
                        }
                        if slot.stopped {
                            break;
                        }
                        slot.pending.take().unwrap()
                    };
                    let theme = request.key.url.as_ref().and_then(|url| {
                        if let Some(index) = cache.iter().position(|(cached, _)| cached == url) {
                            let entry = cache.remove(index).unwrap();
                            let theme = entry.1.clone();
                            cache.push_back(entry);
                            return theme;
                        }
                        // Failed/transparent covers are cached too, avoiding retries on
                        // every state poll or when switching repeatedly between tracks.
                        let theme = load(url).unwrap_or_else(|_| {
                            tracing::debug!("Cover palette unavailable; using visualiser default");
                            None
                        });
                        if cache.len() == 12 {
                            cache.pop_front();
                        }
                        cache.push_back((url.clone(), theme.clone()));
                        theme
                    });
                    let slot = mutex.lock().unwrap_or_else(|e| e.into_inner());
                    // Publication and invalidation share a lock. Even A -> B -> A
                    // with changed art cannot publish an old request's result.
                    if !slot.stopped && request.generation == slot.generation {
                        worker_output.send_replace(message(&request.key, theme.as_ref()));
                    }
                }
            })?;
        Ok(Self {
            slot,
            output,
            thread: Some(thread),
        })
    }

    pub fn update(&self, track: Option<&crate::ipc::ActiveTrackSnapshot>) {
        self.request(Key {
            video_id: track.map(|track| track.video_id.clone()),
            url: track
                .and_then(|track| track.thumb_url.clone())
                .filter(|url| !url.is_empty()),
        });
    }

    fn request(&self, key: Key) {
        let (mutex, wake) = &*self.slot;
        let mut slot = mutex.lock().unwrap_or_else(|e| e.into_inner());
        if slot.current.as_ref() == Some(&key) {
            return;
        }
        slot.generation = slot.generation.wrapping_add(1);
        slot.current = Some(key.clone());
        self.output.send_replace(message(&key, None));
        slot.pending = Some(Request {
            key,
            generation: slot.generation,
        });
        wake.notify_one();
    }
}

impl Drop for ThemeWorker {
    fn drop(&mut self) {
        let (mutex, wake) = &*self.slot;
        {
            let mut slot = mutex.lock().unwrap_or_else(|e| e.into_inner());
            slot.stopped = true;
            slot.pending = None;
            wake.notify_one();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, ImageFormat, Rgba, RgbaImage};
    use material_colors::color::linearized;
    use std::{
        sync::{
            atomic::{AtomicUsize, Ordering},
            mpsc,
        },
        time::{Duration, Instant},
    };

    fn solid(color: Argb) -> Theme {
        theme_from_pixels(&vec![color; 1024]).unwrap()
    }

    fn png(image: RgbaImage) -> Vec<u8> {
        let mut bytes = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(image)
            .write_to(&mut bytes, ImageFormat::Png)
            .unwrap();
        bytes.into_inner()
    }

    fn luminance(hex: &str) -> f64 {
        let color: Argb = hex.parse().unwrap();
        (0.2126 * linearized(color.red)
            + 0.7152 * linearized(color.green)
            + 0.0722 * linearized(color.blue))
            / 100.0
    }

    #[test]
    fn coloured_covers_keep_their_actual_hues_and_legible_contrast() {
        for source in [0xffff0000, 0xff00cc44, 0xff1450ff, 0xffffcc00, 0xffaa00dd] {
            let color = Argb::from_u32(source);
            let theme = solid(color);
            let primary: Argb = theme.ui_theme.primary.parse().unwrap();
            assert!(
                hue_distance(Hct::new(color).get_hue(), Hct::new(primary).get_hue()) < 8.0,
                "{theme:?}"
            );
            let background = luminance(&theme.ui_theme.background);
            for hex in [&theme.ui_theme.primary, &theme.ui_theme.secondary] {
                assert!(
                    (luminance(hex) + 0.05) / (background + 0.05) >= 7.0,
                    "{theme:?}"
                );
            }
            assert_ne!(theme.graph_theme.line1, theme.graph_theme.line2);
            assert_eq!(theme, solid(color));
        }
    }

    #[test]
    fn two_colour_cover_selects_present_hues_instead_of_their_average() {
        let mut pixels = vec![Argb::from_u32(0xffff0000); 600];
        pixels.extend(vec![Argb::from_u32(0xff0000ff); 400]);
        let theme = theme_from_pixels(&pixels).unwrap();
        let selected: Argb = theme.ui_theme.primary.parse().unwrap();
        let selected = Hct::new(selected).get_hue();
        assert!(
            [0xffff0000, 0xff0000ff]
                .into_iter()
                .any(
                    |source| hue_distance(selected, Hct::new(Argb::from_u32(source)).get_hue())
                        < 8.0
                )
        );
        let first: Argb = theme.graph_theme.line1.parse().unwrap();
        let second: Argb = theme.graph_theme.line2.parse().unwrap();
        assert!(hue_distance(Hct::new(first).get_hue(), Hct::new(second).get_hue()) > 40.0);
    }

    #[test]
    fn grayscale_stays_neutral_and_transparent_art_uses_fallback() {
        let theme = solid(Argb::from_u32(0xff888888));
        let primary: Argb = theme.ui_theme.primary.parse().unwrap();
        assert!(
            primary.red.abs_diff(primary.green) <= 1 && primary.green.abs_diff(primary.blue) <= 1
        );
        assert!(
            from_bytes(&png(RgbaImage::from_pixel(32, 32, Rgba([255, 0, 0, 0]))))
                .unwrap()
                .is_none()
        );
        let mut image = RgbaImage::from_pixel(128, 128, Rgba([255, 0, 0, 0]));
        for y in 0..128 {
            for x in 0..64 {
                image.put_pixel(x, y, Rgba([0, 0, 255, 255]));
            }
        }
        assert_eq!(
            from_bytes(&png(image)).unwrap(),
            Some(solid(Argb::from_u32(0xff0000ff)))
        );
    }

    #[test]
    fn corrupt_and_oversized_images_are_rejected_before_unbounded_allocation() {
        assert!(from_bytes(b"not an image").is_err());
        assert!(from_bytes(&vec![0; crate::network::MAX_COVER_BYTES + 1]).is_err());
        let too_wide = png(RgbaImage::new(4097, 1));
        assert!(
            from_bytes(&too_wide)
                .unwrap_err()
                .to_string()
                .contains("dimensions")
        );
        let valid = png(RgbaImage::from_pixel(640, 480, Rgba([255, 0, 0, 255])));
        assert_eq!(
            from_bytes(&valid).unwrap(),
            Some(solid(Argb::from_u32(0xffff0000)))
        );
    }

    fn key(video: &str, url: &str) -> Key {
        Key {
            video_id: Some(video.into()),
            url: Some(url.into()),
        }
    }

    fn wait_palette(receiver: &mut watch::Receiver<String>, video: &str) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let data = receiver.borrow_and_update().clone();
            if let Ok(value) = serde_json::from_str::<serde_json::Value>(&data) {
                if value["video_id"] == video && !value["theme"].is_null() {
                    return value;
                }
            }
            assert!(
                Instant::now() < deadline,
                "palette worker timed out for {video}"
            );
            thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn slow_old_art_cannot_override_latest_and_pending_work_is_coalesced() {
        let (tx, mut rx) = watch::channel(String::new());
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let red = solid(Argb::from_u32(0xffff0000));
        let blue = solid(Argb::from_u32(0xff0000ff));
        let expected = blue.clone();
        let worker = ThemeWorker::with_loader(tx, move |url| {
            started_tx.send(url.to_owned()).unwrap();
            if url == "old" {
                release_rx.recv().unwrap();
                Ok(Some(red.clone()))
            } else {
                Ok(Some(blue.clone()))
            }
        })
        .unwrap();
        worker.request(key("track", "old"));
        let started = started_rx.recv_timeout(Duration::from_secs(2));
        worker.request(key("skipped", "middle"));
        worker.request(key("track", "new"));
        let invalidated: serde_json::Value = serde_json::from_str(&rx.borrow_and_update()).unwrap();
        release_tx.send(()).unwrap();
        assert_eq!(started.unwrap(), "old");
        assert!(invalidated["theme"].is_null());
        let result = wait_palette(&mut rx, "track");
        assert_eq!(result["theme"], serde_json::to_value(expected).unwrap());
        assert_eq!(
            started_rx.recv_timeout(Duration::from_secs(2)).unwrap(),
            "new"
        );
        assert!(started_rx.try_recv().is_err());
    }

    #[test]
    fn palette_cache_reuses_album_art_and_evicts_after_twelve_entries() {
        let (tx, mut rx) = watch::channel(String::new());
        let count = Arc::new(AtomicUsize::new(0));
        let loaded = count.clone();
        let palette = solid(Argb::from_u32(0xff00cc44));
        let worker = ThemeWorker::with_loader(tx, move |_| {
            loaded.fetch_add(1, Ordering::SeqCst);
            Ok(Some(palette.clone()))
        })
        .unwrap();
        worker.request(key("first", "album"));
        wait_palette(&mut rx, "first");
        worker.request(key("second", "album"));
        wait_palette(&mut rx, "second");
        assert_eq!(count.load(Ordering::SeqCst), 1);
        for i in 0..12 {
            let video = i.to_string();
            worker.request(key(&video, &video));
            wait_palette(&mut rx, &video);
        }
        worker.request(key("again", "album"));
        wait_palette(&mut rx, "again");
        assert_eq!(count.load(Ordering::SeqCst), 14);
    }
}
