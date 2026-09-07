//! Native cartridge export. Only the bounded screenshot and view settings come
//! from the browser; encoded song bytes stream to daemon memory or disk.
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Cursor, Read, Write};
use std::path::{Path, PathBuf};

use ab_glyph::{Font, FontRef, PxScale, ScaleFont, point};
use anyhow::{Context, Result, bail};
use base64::{Engine, engine::general_purpose::STANDARD};
use image::{DynamicImage, ImageFormat, ImageReader, Rgb, RgbImage};
use serde_json::{Value, json};

const MAX_SCREENSHOT_BYTES: usize = 4 * 1024 * 1024;
const MAX_METADATA_BYTES: usize = 64 * 1024;
const AUDIO_START: &[u8] = b"---AUDIO-START---";
const AUDIO_END: &[u8] = b"---AUDIO-END---";
const METADATA_START: &[u8] = b"---METADATA-START---";
const METADATA_END: &[u8] = b"---METADATA-END---";

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
    let card = render_card(&screenshot, source, &metadata)?;
    // Only the fixed-size card needs a seekable buffer. Audio is never collected.
    let mut png = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(card).write_to(&mut png, ImageFormat::Png)?;
    let metadata_bytes = serde_json::to_string(&metadata)?.replace("---", "\\u002d\\u002d\\u002d");
    if metadata_bytes.len() > MAX_METADATA_BYTES {
        bail!("cartridge metadata exceeds 64 KiB");
    }
    let uri = url::Url::parse(&source.uri).context("invalid song URI")?;
    if !matches!(uri.scheme(), "file" | "https" | "http") {
        bail!("unsupported song URI scheme");
    }
    let identifier = uuid::Uuid::new_v4();
    let filename = format!("{}-{identifier}.png", safe_filename(&source.title));
    if source.staticfs_enabled {
        // The setting, not mount availability, chooses storage. A missing mount
        // or a failed upload must never silently persist this cartridge on disk.
        let mut output = UploadWriter::new(begin_upload(&filename)?);
        write_cartridge(&mut output, png.get_ref(), &uri, metadata_bytes.as_bytes())?;
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
    write_cartridge(&mut output, png.get_ref(), &uri, metadata_bytes.as_bytes())?;
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

fn write_cartridge(
    output: &mut dyn Write,
    png: &[u8],
    uri: &url::Url,
    metadata: &[u8],
) -> Result<()> {
    output.write_all(png)?;
    output.write_all(AUDIO_START)?;
    let count = match uri.scheme() {
        "file" => {
            let path = uri
                .to_file_path()
                .map_err(|_| anyhow::anyhow!("song URI is not a local file"))?;
            let mut input = File::open(&path).context("cannot open song audio")?;
            let info = input.metadata()?;
            if !info.is_file() {
                bail!("song audio must be a regular file");
            }
            let mut buffer = vec![0u8; STREAM_CHUNK_BYTES];
            let mut count = 0;
            loop {
                let read = input.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                output.write_all(&buffer[..read])?;
                count += read as u64;
            }
            if count != info.len() {
                bail!("song audio changed size during cartridge creation");
            }
            count
        }
        _ => crate::network::stream_media_to_writer(uri.as_str(), output)?,
    };
    if count == 0 {
        bail!("song audio is empty");
    }
    output.write_all(AUDIO_END)?;
    output.write_all(METADATA_START)?;
    output.write_all(metadata)?;
    output.write_all(METADATA_END)?;
    output.flush()?;
    Ok(())
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
    // Preserve the original cartridge schema, taking only known, bounded values.
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
            .filter(|value| value.is_finite() && value.abs() <= 1_000_000.0)
            .unwrap_or(default)
    };
    let fft = object
        .get("fftSize")
        .and_then(Value::as_u64)
        .filter(|value| (256..=32768).contains(value) && value.is_power_of_two())
        .unwrap_or(2048);
    let spacing = object
        .get("lineSpacing")
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && *value > 0.0 && *value <= 1000.0)
        .unwrap_or(0.5);
    let distance = object
        .get("cameraControls")
        .and_then(|camera| camera.get("distance"))
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite() && (5.0..=600.0).contains(value))
        .unwrap_or(60.0);
    let timestamp = chrono::Utc::now().timestamp_millis();
    Ok(json!({
        "fileName": format!("{}.{}", safe_filename(&source.title), audio_extension(&source.uri)),
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
            "position": { "x": camera_number("position", "x", 0.0), "y": camera_number("position", "y", 5.0), "z": camera_number("position", "z", 10.0) },
            "rotation": { "yaw": camera_number("rotation", "yaw", 0.0), "pitch": camera_number("rotation", "pitch", 0.0), "roll": camera_number("rotation", "roll", 0.0) },
            "distance": distance
        },
        "lineSpacing": spacing, "fftSize": fft, "timestamp": timestamp
    }))
}

fn audio_extension(uri: &str) -> &'static str {
    let Ok(uri) = url::Url::parse(uri) else {
        return "audio";
    };
    let mime = uri
        .query_pairs()
        .find_map(|(key, value)| (key == "mime").then(|| value.into_owned()));
    match mime.as_deref() {
        Some("audio/webm") => return "webm",
        Some("audio/mp4") => return "m4a",
        Some("audio/mpeg") => return "mp3",
        _ => {}
    }
    match Path::new(uri.path())
        .extension()
        .and_then(|value| value.to_str())
    {
        Some("mp3") => "mp3",
        Some("m4a") => "m4a",
        Some("webm") => "webm",
        Some("ogg") => "ogg",
        Some("opus") => "opus",
        Some("wav") => "wav",
        Some("flac") => "flac",
        _ => "audio",
    }
}

fn parse_color(value: &str) -> Option<[u8; 3]> {
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
            video_id: "test-video".into(),
            storage_root: std::env::temp_dir(),
            staticfs_enabled: false,
            staticfs_mountpoint: None,
            title: "../../Test ---METADATA-END--- song".into(),
            artist: "Test artist".into(),
            uri,
        }
    }

    #[test]
    fn cartridge_streams_local_audio_and_preserves_legacy_format() {
        let root =
            std::env::temp_dir().join(format!("static-cartridge-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let audio = root.join("song with spaces #1.wav");
        let length = 8 * 1024 * 1024 + 17;
        let mut input = File::create(&audio).unwrap();
        std::io::copy(
            &mut std::io::Read::take(std::io::repeat(0x5a), length),
            &mut input,
        )
        .unwrap();
        drop(input);
        let mut source = source(url::Url::from_file_path(&audio).unwrap().to_string());
        source.storage_root = root.clone();
        let result = create(&source, &screenshot(), json!({"fftSize": 4096})).unwrap();
        let CartridgeStorage::Disk(path) = &result.storage else {
            panic!("expected disk export")
        };
        assert_eq!(path.parent(), Some(root.join("cartridges").as_path()));
        assert!(!result.filename.contains('/'));
        let bytes = std::fs::read(path).unwrap();
        let start = bytes
            .windows(AUDIO_START.len())
            .position(|value| value == AUDIO_START)
            .unwrap();
        let png = image::load_from_memory(&bytes[..start]).unwrap();
        assert_eq!((png.width(), png.height()), (800, 1120));
        let audio_start = start + AUDIO_START.len();
        assert!(
            bytes[audio_start..audio_start + length as usize]
                .iter()
                .all(|value| *value == 0x5a)
        );
        let audio_end = audio_start + length as usize;
        assert_eq!(&bytes[audio_end..audio_end + AUDIO_END.len()], AUDIO_END);
        let metadata_start = audio_end + AUDIO_END.len() + METADATA_START.len();
        let metadata: Value =
            serde_json::from_slice(&bytes[metadata_start..bytes.len() - METADATA_END.len()])
                .unwrap();
        assert_eq!(metadata["videoId"], "test-video");
        assert_eq!(metadata["title"], source.title);
        assert_eq!(metadata["fftSize"], 4096);
        assert!(!metadata.to_string().contains("file://"));
        assert_eq!(
            std::fs::read_dir(root.join("cartridges")).unwrap().count(),
            1
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cartridge_rejects_invalid_screenshots_and_remote_file_hosts() {
        assert!(decode_screenshot("data:image/jpeg;base64,aGVsbG8=").is_err());
        assert!(decode_screenshot("data:image/png;base64,not base64").is_err());
        assert!(decode_screenshot(&"x".repeat(MAX_SCREENSHOT_BYTES + 1)).is_err());
        let root =
            std::env::temp_dir().join(format!("static-cartridge-invalid-{}", uuid::Uuid::new_v4()));
        let result = create_in(
            &root,
            &source("file://remotehost/tmp/audio.wav".into()),
            &screenshot(),
            json!({}),
        );
        assert!(result.is_err());
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
        std::fs::remove_dir_all(root).unwrap();
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
        let metadata = prepare_metadata(&input, json!({"uiTheme": {"primary":"#112233"}, "cameraControls":{"position":{"x":3.5},"rotation":{"yaw":0.4,"roll":1.2},"distance":125.5}, "lineSpacing":0.25,"fftSize":1024})).unwrap();
        assert_eq!(metadata["uiTheme"]["primary"], "#112233");
        assert_eq!(metadata["cameraControls"]["position"]["x"], 3.5);
        assert_eq!(metadata["cameraControls"]["rotation"]["yaw"], 0.4);
        assert_eq!(metadata["cameraControls"]["distance"], 125.5);
        let fallback = prepare_metadata(&input, json!({"cameraControls":{"distance":0}})).unwrap();
        assert_eq!(fallback["cameraControls"]["distance"], 60.0);
        assert_eq!(metadata["lineSpacing"], 0.25);
        assert!(metadata["fileName"].as_str().unwrap().ends_with(".webm"));
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
        std::fs::create_dir_all(&root).unwrap();
        let audio = root.join("song.wav");
        let audio_bytes = vec![0x5a; 3 * STREAM_CHUNK_BYTES + 17];
        std::fs::write(&audio, &audio_bytes).unwrap();
        let mut source = source(url::Url::from_file_path(&audio).unwrap().to_string());
        source.storage_root = root.clone();
        source.staticfs_enabled = true;
        let state = std::sync::Arc::new(std::sync::Mutex::new(UploadState::default()));
        let result = create_with_upload(
            &root.join("cartridges"),
            &source,
            &screenshot(),
            json!({"fftSize":4096}),
            |name| {
                state.lock().unwrap().name = name.into();
                Ok(FakeUpload(state.clone()))
            },
        )
        .unwrap();
        assert!(matches!(result.storage, CartridgeStorage::Memory { .. }));
        assert!(result.display_path.is_none());
        assert!(!root.join("cartridges").exists());
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 1);
        let stored = state.lock().unwrap();
        assert_eq!(stored.commits, 1);
        assert_eq!(stored.aborts, 0);
        assert!(stored.max_chunk <= STREAM_CHUNK_BYTES);
        let bytes = &stored.bytes;
        let png_end = bytes
            .windows(AUDIO_START.len())
            .position(|part| part == AUDIO_START)
            .unwrap();
        let png = image::load_from_memory(&bytes[..png_end]).unwrap();
        assert_eq!((png.width(), png.height()), (800, 1120));
        let start = png_end + AUDIO_START.len();
        assert_eq!(&bytes[start..start + audio_bytes.len()], audio_bytes);
        let end = start + audio_bytes.len();
        assert_eq!(&bytes[end..end + AUDIO_END.len()], AUDIO_END);
        assert_eq!(
            &bytes[end + AUDIO_END.len()..end + AUDIO_END.len() + METADATA_START.len()],
            METADATA_START
        );
        let metadata: Value = serde_json::from_slice(
            &bytes[end + AUDIO_END.len() + METADATA_START.len()..bytes.len() - METADATA_END.len()],
        )
        .unwrap();
        assert_eq!(metadata["title"], source.title);
        assert_eq!(metadata["fftSize"], 4096);
        assert!(bytes.ends_with(METADATA_END));
        drop(stored);
        std::fs::remove_dir_all(root).unwrap();
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
