//! Native cartridge export. Only the bounded screenshot and view settings come
//! from the browser; the pixels carry a compact song link and view payload.
use std::fs::OpenOptions;
use std::io::{BufWriter, Cursor, Write};
use std::path::{Path, PathBuf};

use ab_glyph::{Font, FontRef, PxScale, ScaleFont, point};
use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use image::{DynamicImage, ImageFormat, ImageReader, Rgb, RgbImage};
use serde_json::{Value, json};

const MAX_SCREENSHOT_BYTES: usize = 4 * 1024 * 1024;
const MAX_METADATA_BYTES: usize = 64 * 1024;
pub const STREAM_CHUNK_BYTES: usize = 256 * 1024;

#[derive(Clone)]
pub enum CartridgeStorage {
    Disk(PathBuf),
    Memory { name: String, size: u64 },
}

impl CartridgeStorage {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Disk(_) => "disk",
            Self::Memory { .. } => "memory",
        }
    }
}

pub struct CartridgeFile {
    pub storage: CartridgeStorage,
    pub filename: String,
    pub display_path: Option<PathBuf>,
}

pub fn create(
    source: &crate::ipc::VisualizationSource,
    screenshot: &str,
    metadata: Value,
) -> Result<CartridgeFile> {
    create_in(
        &source.storage_root.join("cartridges"),
        source,
        screenshot,
        metadata,
    )
}

fn create_in(
    directory: &Path,
    source: &crate::ipc::VisualizationSource,
    screenshot: &str,
    metadata: Value,
) -> Result<CartridgeFile> {
    create_with_upload(directory, source, screenshot, metadata, NativeUpload::begin)
}

fn create_with_upload<T: UploadTarget>(
    directory: &Path,
    source: &crate::ipc::VisualizationSource,
    screenshot: &str,
    metadata: Value,
    begin_upload: impl FnOnce(&str) -> Result<T>,
) -> Result<CartridgeFile> {
    let screenshot = decode_screenshot(screenshot)?;
    let metadata = prepare_metadata(source, metadata)?;
    let payload = super::cartridge_codec::pack(&metadata)?;
    let mut card = render_card(&screenshot, source, &metadata)?;
    super::cartridge_codec::embed(&mut card, &payload)?;
    let mut png = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(card).write_to(&mut png, ImageFormat::Png)?;
    let identifier = uuid::Uuid::new_v4();
    let filename = format!("{}-{identifier}.png", safe_filename(&source.title));
    if source.staticfs_enabled {
        // The setting, not mount availability, chooses storage. A missing mount
        // or a failed upload must never silently persist this cartridge on disk.
        let mut output = UploadWriter::new(begin_upload(&filename)?);
        output.write_all(png.get_ref())?;
        let info = output.finish()?;
        let display_path = source
            .staticfs_mountpoint
            .as_deref()
            .and_then(|mountpoint| {
                #[cfg(feature = "staticfs")]
                let mounted = crate::staticfs::is_live_staticfs_mount(mountpoint);
                #[cfg(not(feature = "staticfs"))]
                let mounted = false;
                mounted.then(|| mountpoint.join("cartridges").join(&filename))
            });
        return Ok(CartridgeFile {
            storage: CartridgeStorage::Memory {
                name: info.name,
                size: info.size,
            },
            filename,
            display_path,
        });
    }
    std::fs::create_dir_all(directory).context("cannot create cartridge directory")?;
    let destination = directory.join(&filename);
    let temporary = directory.join(format!(".{identifier}.tmp"));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(&temporary)
        .context("cannot create cartridge file")?;
    let cleanup = TemporaryFile(temporary.clone());
    let mut output = BufWriter::with_capacity(STREAM_CHUNK_BYTES, file);
    output.write_all(png.get_ref())?;
    output.flush()?;
    output.get_ref().sync_all()?;
    drop(output);
    std::fs::rename(&temporary, &destination).context("cannot finish cartridge file")?;
    drop(cleanup);
    Ok(CartridgeFile {
        storage: CartridgeStorage::Disk(destination.clone()),
        filename,
        display_path: Some(destination),
    })
}

trait UploadTarget {
    fn append(&mut self, bytes: &[u8]) -> Result<()>;
    fn commit(&mut self) -> Result<crate::cartridges::CartridgeInfo>;
    fn abort(&mut self);
}

struct NativeUpload {
    client: crate::ipc::DaemonClient,
    id: String,
}
impl NativeUpload {
    fn begin(name: &str) -> Result<Self> {
        let client = crate::ipc::DaemonClient::connect_existing()?;
        let id = client.begin_cartridge_upload(name)?;
        Ok(Self { client, id })
    }
}
impl UploadTarget for NativeUpload {
    fn append(&mut self, bytes: &[u8]) -> Result<()> {
        self.client.append_cartridge_upload(&self.id, bytes)
    }
    fn commit(&mut self) -> Result<crate::cartridges::CartridgeInfo> {
        self.client.commit_cartridge_upload(&self.id)
    }
    fn abort(&mut self) {
        let _ = self.client.abort_cartridge_upload(&self.id);
    }
}

struct UploadWriter<T: UploadTarget> {
    target: T,
    buffer: Vec<u8>,
    committed: bool,
}
impl<T: UploadTarget> UploadWriter<T> {
    fn new(target: T) -> Self {
        Self {
            target,
            buffer: Vec::with_capacity(STREAM_CHUNK_BYTES),
            committed: false,
        }
    }
    fn finish(mut self) -> Result<crate::cartridges::CartridgeInfo> {
        self.flush()?;
        let info = self.target.commit()?;
        self.committed = true;
        Ok(info)
    }
}
impl<T: UploadTarget> Write for UploadWriter<T> {
    fn write(&mut self, mut bytes: &[u8]) -> std::io::Result<usize> {
        let size = bytes.len();
        while !bytes.is_empty() {
            let count = bytes.len().min(STREAM_CHUNK_BYTES - self.buffer.len());
            self.buffer.extend_from_slice(&bytes[..count]);
            bytes = &bytes[count..];
            if self.buffer.len() == STREAM_CHUNK_BYTES {
                self.flush()?;
            }
        }
        Ok(size)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        if !self.buffer.is_empty() {
            self.target
                .append(&self.buffer)
                .map_err(std::io::Error::other)?;
            self.buffer.clear();
        }
        Ok(())
    }
}
impl<T: UploadTarget> Drop for UploadWriter<T> {
    fn drop(&mut self) {
        if !self.committed {
            self.target.abort();
        }
    }
}

struct TemporaryFile(PathBuf);
impl Drop for TemporaryFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn safe_filename(title: &str) -> String {
    let mut bytes = 0;
    let name: String = title
        .chars()
        .take(80)
        .map(|character| {
            if character.is_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .take_while(|character| {
            bytes += character.len_utf8();
            bytes <= 160
        })
        .collect();
    if name.trim_matches('_').is_empty() {
        "cartridge".to_owned()
    } else {
        name
    }
}

fn decode_screenshot(data: &str) -> Result<DynamicImage> {
    if data.len() > MAX_SCREENSHOT_BYTES {
        bail!("screenshot data URL exceeds 4 MiB");
    }
    let encoded = data
        .strip_prefix("data:image/png;base64,")
        .context("screenshot must be a PNG data URL")?;
    let bytes = STANDARD
        .decode(encoded)
        .context("invalid screenshot base64")?;
    let mut reader = ImageReader::with_format(Cursor::new(bytes), ImageFormat::Png);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    reader
        .decode()
        .context("invalid or oversized screenshot PNG")
}

fn prepare_metadata(source: &crate::ipc::VisualizationSource, metadata: Value) -> Result<Value> {
    let object = metadata
        .as_object()
        .context("cartridge metadata must be an object")?;
    if serde_json::to_vec(object)?.len() > MAX_METADATA_BYTES {
        bail!("cartridge metadata exceeds 64 KiB");
    }
    // JSON is only the local UI transport, never the on-image representation.
    let color = |section: &str, name: &str, default: &str| -> String {
        object
            .get(section)
            .and_then(|value| value.get(name))
            .and_then(Value::as_str)
            .filter(|value| parse_color(value).is_some())
            .unwrap_or(default)
            .to_owned()
    };
    let camera_number = |section: &str, axis: &str, default: f64| -> f64 {
        object
            .get("cameraControls")
            .and_then(|value| value.get(section))
            .and_then(|value| value.get(axis))
            .and_then(Value::as_f64)
            .filter(|value| value.is_finite())
            .unwrap_or(default)
    };
    let fft = object
        .get("fftSize")
        .and_then(Value::as_u64)
        .filter(|value| (8..=128).contains(value))
        .unwrap_or(30);
    let spacing = object
        .get("lineSpacing")
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && *value > 0.0 && *value <= 1000.0)
        .unwrap_or(2.0);
    let distance = object
        .get("cameraControls")
        .and_then(|camera| camera.get("distance"))
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite())
        .unwrap_or(60.0);
    let timestamp = chrono::Utc::now().timestamp_millis();
    Ok(json!({
        "videoId": source.video_id,
        "title": source.title.chars().take(512).collect::<String>(),
        "artist": source.artist.chars().take(512).collect::<String>(),
        "uiTheme": {
            "primary": color("uiTheme", "primary", "#9b87f5"),
            "secondary": color("uiTheme", "secondary", "#7E69AB"),
            "background": color("uiTheme", "background", "#121212")
        },
        "graphTheme": { "line1": color("graphTheme", "line1", "#9b87f5"), "line2": color("graphTheme", "line2", "#D946EF") },
        "cameraControls": {
            "position": { "x": camera_number("position", "x", 0.0), "y": camera_number("position", "y", 0.0), "z": camera_number("position", "z", 0.0) },
            "rotation": { "yaw": camera_number("rotation", "yaw", 0.0), "pitch": camera_number("rotation", "pitch", 0.0), "roll": camera_number("rotation", "roll", 80.0f64.to_radians()) },
            "distance": distance
        },
        "lineSpacing": spacing, "fftSize": fft, "timestamp": timestamp
    }))
}

pub(super) fn parse_color(value: &str) -> Option<[u8; 3]> {
    let hex = value.strip_prefix('#')?;
    if hex.len() != 6 || !hex.is_ascii() {
        return None;
    }
    Some([
        u8::from_str_radix(&hex[0..2], 16).ok()?,
        u8::from_str_radix(&hex[2..4], 16).ok()?,
        u8::from_str_radix(&hex[4..6], 16).ok()?,
    ])
}

fn render_card(
    screenshot: &DynamicImage,
    source: &crate::ipc::VisualizationSource,
    metadata: &Value,
) -> Result<RgbImage> {
    let primary = parse_color(metadata["uiTheme"]["primary"].as_str().unwrap_or(""))
        .unwrap_or([155, 135, 245]);
    let secondary = parse_color(metadata["uiTheme"]["secondary"].as_str().unwrap_or(""))
        .unwrap_or([126, 105, 171]);
    let mut card = RgbImage::from_fn(800, 1120, |_x, y| {
        let mix = y as f32 / 1119.0;
        Rgb(std::array::from_fn(|index| {
            ((primary[index] as f32 * (1.0 - mix) + secondary[index] as f32 * mix) * 0.5 + 9.0)
                as u8
        }))
    });
    for y in 24..1096 {
        for x in 24..776 {
            if !(40..760).contains(&x) || !(40..1080).contains(&y) {
                card.put_pixel(x, y, Rgb(primary));
            }
        }
    }
    for y in 120..560 {
        for x in 40..760 {
            card.put_pixel(x, y, Rgb([0, 0, 0]));
        }
    }
    let screenshot = screenshot.thumbnail(720, 440).to_rgb8();
    image::imageops::replace(
        &mut card,
        &screenshot,
        40 + (720 - screenshot.width()) as i64 / 2,
        120 + (440 - screenshot.height()) as i64 / 2,
    );
    let regular =
        FontRef::try_from_slice(include_bytes!("../../assets/fonts/FiraSans-Regular.ttf"))?;
    let bold = FontRef::try_from_slice(include_bytes!("../../assets/fonts/FiraSans-Bold.ttf"))?;
    let mono = FontRef::try_from_slice(include_bytes!("../../assets/fonts/FiraMono-Regular.ttf"))?;
    draw_text(
        &mut card,
        &bold,
        &source.title,
        48.0,
        60,
        66,
        680,
        [255, 255, 255],
    );
    draw_text(
        &mut card,
        &regular,
        &source.artist,
        26.0,
        60,
        596,
        680,
        [220, 220, 230],
    );
    let timestamp = metadata["timestamp"].as_i64().unwrap_or_default();
    let date = chrono::DateTime::from_timestamp_millis(timestamp)
        .unwrap_or_default()
        .format("%Y-%m-%d %H:%M:%S UTC");
    draw_text(
        &mut card,
        &regular,
        &format!("Captured: {date}"),
        28.0,
        60,
        644,
        680,
        [255, 255, 255],
    );
    draw_text(
        &mut card,
        &regular,
        "Camera Position:",
        32.0,
        60,
        720,
        680,
        [255, 255, 255],
    );
    let camera = &metadata["cameraControls"];
    let number = |section: &str, axis: &str| camera[section][axis].as_f64().unwrap_or_default();
    draw_text(
        &mut card,
        &mono,
        &format!(
            "X: {:.1} Y: {:.1} Z: {:.1}",
            number("position", "x"),
            number("position", "y"),
            number("position", "z")
        ),
        26.0,
        80,
        766,
        640,
        [255, 255, 255],
    );
    draw_text(
        &mut card,
        &regular,
        "Camera Rotation:",
        32.0,
        60,
        832,
        680,
        [255, 255, 255],
    );
    draw_text(
        &mut card,
        &mono,
        &format!(
            "Yaw: {:.1} Pitch: {:.1} Roll: {:.1}",
            number("rotation", "yaw").to_degrees(),
            number("rotation", "pitch").to_degrees(),
            number("rotation", "roll").to_degrees()
        ),
        24.0,
        80,
        878,
        640,
        [255, 255, 255],
    );
    draw_text(
        &mut card,
        &regular,
        &format!(
            "View distance: {:.1}",
            camera["distance"].as_f64().unwrap_or(60.0)
        ),
        26.0,
        60,
        942,
        680,
        [255, 255, 255],
    );
    draw_text(
        &mut card,
        &regular,
        "Static / Audio Visualizer Cartridge",
        26.0,
        160,
        1046,
        600,
        [190, 190, 205],
    );
    Ok(card)
}

#[allow(clippy::too_many_arguments)]
fn draw_text(
    image: &mut RgbImage,
    font: &FontRef<'_>,
    text: &str,
    size: f32,
    x: u32,
    y: u32,
    width: u32,
    color: [u8; 3],
) {
    let scaled = font.as_scaled(PxScale::from(size));
    let mut caret = x as f32;
    let mut previous = None;
    for character in text
        .chars()
        .filter(|character| !character.is_control())
        .take(512)
    {
        let id = scaled.glyph_id(character);
        if let Some(previous) = previous {
            caret += scaled.kern(previous, id);
        }
        if caret + scaled.h_advance(id) > (x + width) as f32 {
            break;
        }
        let glyph = id.with_scale_and_position(size, point(caret, y as f32));
        if let Some(outline) = font.outline_glyph(glyph) {
            let bounds = outline.px_bounds();
            outline.draw(|gx, gy, coverage| {
                let px = bounds.min.x as i32 + gx as i32;
                let py = bounds.min.y as i32 + gy as i32;
                if px >= 0 && py >= 0 && px < image.width() as i32 && py < image.height() as i32 {
                    let old = image.get_pixel_mut(px as u32, py as u32);
                    for index in 0..3 {
                        old[index] = (old[index] as f32 * (1.0 - coverage)
                            + color[index] as f32 * coverage)
                            as u8;
                    }
                }
            });
        }
        caret += scaled.h_advance(id);
        previous = Some(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn screenshot() -> String {
        let mut bytes = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(RgbImage::from_pixel(64, 32, Rgb([100, 200, 50])))
            .write_to(&mut bytes, ImageFormat::Png)
            .unwrap();
        format!(
            "data:image/png;base64,{}",
            STANDARD.encode(bytes.into_inner())
        )
    }

    fn source(uri: String) -> crate::ipc::VisualizationSource {
        crate::ipc::VisualizationSource {
            video_id: "dQw4w9WgXcQ".into(),
            storage_root: std::env::temp_dir(),
            staticfs_enabled: false,
            staticfs_mountpoint: None,
            title: "../../Test ---METADATA-END--- song".into(),
            artist: "Test artist".into(),
            uri,
        }
    }

    #[test]
    fn cartridge_encodes_link_and_view_without_accessing_audio() {
        let root =
            std::env::temp_dir().join(format!("static-link-cartridge-{}", uuid::Uuid::new_v4()));
        let mut source = source("file:///does-not-exist/song.wav".into());
        source.storage_root = root.clone();
        let result = create(&source, &screenshot(), json!({"fftSize": 64})).unwrap();
        let CartridgeStorage::Disk(path) = &result.storage else {
            panic!("expected disk export")
        };
        assert_eq!(path.parent(), Some(root.join("cartridges").as_path()));
        assert!(!result.filename.contains('/'));
        let bytes = std::fs::read(path).unwrap();
        assert!(bytes.len() < 2 * 1024 * 1024);
        // PNG ends at IEND. There are no audio bytes or metadata trailers.
        assert_eq!(&bytes[bytes.len() - 8..], b"IEND\xaeB`\x82");
        let decoded = super::super::cartridge_codec::decode_bytes(&bytes).unwrap();
        assert_eq!(decoded["videoId"], source.video_id);
        assert_eq!(decoded["fftSize"], 64);
        let original = image::load_from_memory(&bytes).unwrap();
        for quality in [70, 30] {
            let mut compressed = Vec::new();
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut compressed, quality)
                .encode_image(&original)
                .unwrap();
            assert_eq!(
                super::super::cartridge_codec::decode_bytes(&compressed).unwrap(),
                decoded
            );
        }
        assert_eq!(
            std::fs::read_dir(root.join("cartridges")).unwrap().count(),
            1
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cartridge_rejects_invalid_screenshots() {
        assert!(decode_screenshot("data:image/jpeg;base64,aGVsbG8=").is_err());
        assert!(decode_screenshot("data:image/png;base64,not base64").is_err());
        assert!(decode_screenshot(&"x".repeat(MAX_SCREENSHOT_BYTES + 1)).is_err());
    }

    #[test]
    fn screenshot_dimensions_are_bounded_before_allocation() {
        let mut png = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(RgbImage::new(4097, 1))
            .write_to(&mut png, ImageFormat::Png)
            .unwrap();
        let url = format!(
            "data:image/png;base64,{}",
            STANDARD.encode(png.into_inner())
        );
        assert!(decode_screenshot(&url).is_err());
    }

    #[test]
    fn metadata_keeps_view_settings_and_rejects_unbounded_payloads() {
        let input = source("file:///tmp/audio?mime=audio%2Fwebm".into());
        let metadata = prepare_metadata(&input, json!({"uiTheme": {"primary":"#112233"}, "cameraControls":{"position":{"x":3.5},"rotation":{"yaw":0.4,"roll":1.2},"distance":125.5}, "lineSpacing":0.25,"fftSize":64})).unwrap();
        assert_eq!(metadata["uiTheme"]["primary"], "#112233");
        assert_eq!(metadata["cameraControls"]["position"]["x"], 3.5);
        assert_eq!(metadata["cameraControls"]["rotation"]["yaw"], 0.4);
        assert_eq!(metadata["cameraControls"]["distance"], 125.5);
        let fallback = prepare_metadata(&input, json!({"cameraControls":{"distance":0}})).unwrap();
        assert!(super::super::cartridge_codec::pack(&fallback).is_err());
        assert_eq!(metadata["lineSpacing"], 0.25);
        assert_eq!(metadata["fftSize"], 64);
        assert!(prepare_metadata(&input, json!({"junk":"x".repeat(MAX_METADATA_BYTES)})).is_err());
        assert!(prepare_metadata(&input, json!([])).is_err());
        assert!(safe_filename(&"界".repeat(100)).len() <= 160);
    }
    #[derive(Default)]
    struct UploadState {
        name: String,
        bytes: Vec<u8>,
        max_chunk: usize,
        commits: usize,
        aborts: usize,
        fail_append: bool,
        fail_commit: bool,
    }
    struct FakeUpload(std::sync::Arc<std::sync::Mutex<UploadState>>);
    impl UploadTarget for FakeUpload {
        fn append(&mut self, bytes: &[u8]) -> Result<()> {
            let mut state = self.0.lock().unwrap();
            if state.fail_append {
                bail!("memory full");
            }
            state.max_chunk = state.max_chunk.max(bytes.len());
            state.bytes.extend_from_slice(bytes);
            Ok(())
        }
        fn commit(&mut self) -> Result<crate::cartridges::CartridgeInfo> {
            let mut state = self.0.lock().unwrap();
            if state.fail_commit {
                bail!("upload expired");
            }
            state.commits += 1;
            Ok(crate::cartridges::CartridgeInfo {
                name: state.name.clone(),
                size: state.bytes.len() as u64,
                created_unix_millis: 0,
            })
        }
        fn abort(&mut self) {
            self.0.lock().unwrap().aborts += 1;
        }
    }

    #[test]
    fn enabled_staticfs_exports_only_to_memory_even_without_a_mount() {
        let root =
            std::env::temp_dir().join(format!("static-memory-cartridge-{}", uuid::Uuid::new_v4()));
        let mut source = source("unused-audio-uri".into());
        source.storage_root = root.clone();
        source.staticfs_enabled = true;
        let state = std::sync::Arc::new(std::sync::Mutex::new(UploadState::default()));
        let result = create_with_upload(
            &root.join("cartridges"),
            &source,
            &screenshot(),
            json!({"fftSize":64}),
            |name| {
                state.lock().unwrap().name = name.into();
                Ok(FakeUpload(state.clone()))
            },
        )
        .unwrap();
        assert!(matches!(result.storage, CartridgeStorage::Memory { .. }));
        assert!(result.display_path.is_none());
        assert!(!root.exists());
        let stored = state.lock().unwrap();
        assert_eq!(stored.commits, 1);
        assert_eq!(stored.aborts, 0);
        assert!(stored.max_chunk <= STREAM_CHUNK_BYTES);
        let metadata = super::super::cartridge_codec::decode_bytes(&stored.bytes).unwrap();
        assert_eq!(metadata["videoId"], source.video_id);
        assert_eq!(metadata["fftSize"], 64);
    }

    #[test]
    fn failed_memory_exports_abort_without_disk_fallback() {
        let root =
            std::env::temp_dir().join(format!("static-memory-fail-{}", uuid::Uuid::new_v4()));
        let mut source = source("file:///missing-song.wav".into());
        source.storage_root = root.clone();
        source.staticfs_enabled = true;
        let failed_begin = create_with_upload::<FakeUpload>(
            &root.join("cartridges"),
            &source,
            &screenshot(),
            json!({}),
            |_| bail!("daemon unavailable"),
        );
        assert!(failed_begin.is_err());
        assert!(!root.exists());
        for fail_append in [false, true] {
            let state = std::sync::Arc::new(std::sync::Mutex::new(UploadState {
                fail_append,
                fail_commit: !fail_append,
                ..Default::default()
            }));
            assert!(
                create_with_upload(
                    &root.join("cartridges"),
                    &source,
                    &screenshot(),
                    json!({}),
                    |_| Ok(FakeUpload(state.clone()))
                )
                .is_err()
            );
            assert_eq!(state.lock().unwrap().commits, 0);
            assert_eq!(state.lock().unwrap().aborts, 1);
            assert!(!root.exists());
        }
    }

    #[test]
    fn upload_writer_aborts_on_drop_append_failure_and_commit_failure() {
        for failure in [0, 1, 2] {
            let state = std::sync::Arc::new(std::sync::Mutex::new(UploadState {
                fail_append: failure == 1,
                fail_commit: failure == 2,
                ..Default::default()
            }));
            let mut output = UploadWriter::new(FakeUpload(state.clone()));
            if failure == 1 {
                assert!(output.write_all(&vec![0; STREAM_CHUNK_BYTES]).is_err());
                drop(output);
            } else if failure == 2 {
                output.write_all(b"cartridge").unwrap();
                assert!(output.finish().is_err());
            } else {
                output.write_all(b"partial").unwrap();
                drop(output);
            }
            assert_eq!(state.lock().unwrap().aborts, 1);
            assert_eq!(state.lock().unwrap().commits, 0);
        }
    }
}
