//! Versioned, fixed-position payload wrapped by established watermark/ECC libraries.
//! All heavy image work stays native. No PNG chunks or trailing bytes carry data.
use anyhow::{Context, Result, ensure};
use bitvec::prelude::*;
use blind_watermark::prelude::*;
use image::{DynamicImage, ImageReader, RgbImage, imageops::FilterType};
use serde_json::{Value, json};
use std::io::Cursor;

pub const WIDTH: u32 = 800;
pub const HEIGHT: u32 = 1120;
pub const DATA_BYTES: usize = 39;
const ECC_BYTES: usize = 128;
const CODE_BYTES: usize = DATA_BYTES + ECC_BYTES;
const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
pub const MAX_IMAGE_BYTES: usize = 16 * 1024 * 1024;

pub fn pack(metadata: &Value) -> Result<[u8; DATA_BYTES]> {
    let mut bytes = [0; DATA_BYTES];
    bytes[..3].copy_from_slice(b"SC\x01");
    let id = metadata["videoId"].as_str().context("missing video ID")?;
    ensure!(
        id.len() == 11,
        "Cartridges require an 11-character YouTube video ID"
    );
    // Eleven URL-safe base64 characters occupy 66 bits, packed into nine bytes.
    let mut id_bits = 0u128;
    for byte in id.bytes() {
        let digit = ALPHABET
            .iter()
            .position(|&c| c == byte)
            .context("invalid YouTube video ID")?;
        id_bits = (id_bits << 6) | digit as u128;
    }
    bytes[3..12].copy_from_slice(&id_bits.to_be_bytes()[7..]);
    for (i, (group, key)) in [
        ("uiTheme", "primary"),
        ("uiTheme", "secondary"),
        ("uiTheme", "background"),
        ("graphTheme", "line1"),
        ("graphTheme", "line2"),
    ]
    .iter()
    .enumerate()
    {
        let color = super::cartridge::parse_color(metadata[group][key].as_str().unwrap_or(""))
            .context("invalid cartridge colour")?;
        bytes[12 + i * 3..15 + i * 3].copy_from_slice(&color);
    }
    let camera = &metadata["cameraControls"];
    for (i, axis) in ["x", "y", "z"].iter().enumerate() {
        let value = camera["position"][axis]
            .as_f64()
            .context("missing camera position")?;
        ensure!(
            value.is_finite() && (-256.0..=254.0).contains(&value),
            "Move the view closer to the origin before exporting: camera {axis} must be between -256 and 254"
        );
        bytes[27 + i] = (value / 2.0).round() as i8 as u8;
    }
    let yaw = camera["rotation"]["yaw"]
        .as_f64()
        .context("missing camera yaw")?;
    let polar = camera["rotation"]["roll"]
        .as_f64()
        .context("missing camera polar angle")?;
    let pitch = camera["rotation"]["pitch"].as_f64().unwrap_or(0.0);
    ensure!(
        yaw.is_finite()
            && polar.is_finite()
            && (0.0..=std::f64::consts::PI).contains(&polar)
            && pitch == 0.0,
        "Cartridge camera must use the visualiser's orbit controls"
    );
    bytes[30] = (yaw.rem_euclid(std::f64::consts::TAU) / std::f64::consts::TAU * 256.0).round()
        as u16 as u8;
    bytes[31] = (polar / std::f64::consts::PI * 255.0).round() as u8;
    bytes[32] = quantize(
        camera["distance"].as_f64(),
        5.0,
        255.0,
        1.0,
        "view distance",
    )?;
    bytes[33] = quantize(
        metadata["lineSpacing"].as_f64(),
        0.05,
        12.75,
        20.0,
        "line spacing",
    )?;
    bytes[34] = quantize(
        metadata["fftSize"].as_f64(),
        8.0,
        128.0,
        1.0,
        "frequency bands",
    )?;
    let checksum = crc32fast::hash(&bytes[..35]);
    bytes[35..].copy_from_slice(&checksum.to_be_bytes());
    Ok(bytes)
}

fn quantize(value: Option<f64>, min: f64, max: f64, scale: f64, name: &str) -> Result<u8> {
    let value = value.context(format!("missing {name}"))?;
    ensure!(
        value.is_finite() && (min..=max).contains(&value),
        "Cartridge {name} must be between {min} and {max}"
    );
    Ok((value * scale).round() as u8)
}

pub fn unpack(bytes: &[u8]) -> Result<Value> {
    ensure!(
        bytes.len() == DATA_BYTES && &bytes[..3] == b"SC\x01",
        "Not a supported Static cartridge"
    );
    ensure!(
        crc32fast::hash(&bytes[..35]).to_be_bytes() == bytes[35..],
        "Cartridge checksum failed"
    );
    ensure!(
        bytes[3] < 4 && bytes[32] >= 5 && bytes[33] > 0 && (8..=128).contains(&bytes[34]),
        "Invalid cartridge values"
    );
    let bits = bytes[3..12]
        .iter()
        .fold(0u128, |v, &b| (v << 8) | b as u128);
    let id: String = (0..11)
        .rev()
        .map(|i| ALPHABET[((bits >> (i * 6)) & 63) as usize] as char)
        .collect();
    let color = |i: usize| {
        format!(
            "#{:02x}{:02x}{:02x}",
            bytes[12 + i * 3],
            bytes[13 + i * 3],
            bytes[14 + i * 3]
        )
    };
    Ok(json!({
        "videoId": id, "url": crate::ipc::youtube_music_watch_url(&id),
        "uiTheme": {"primary": color(0), "secondary": color(1), "background": color(2)},
        "graphTheme": {"line1": color(3), "line2": color(4)},
        "cameraControls": {
            "position": {"x": (bytes[27] as i8 as i16)*2, "y": (bytes[28] as i8 as i16)*2, "z": (bytes[29] as i8 as i16)*2},
            "rotation": {"yaw": bytes[30] as f64 / 256.0 * std::f64::consts::TAU, "roll": bytes[31] as f64 / 255.0 * std::f64::consts::PI, "pitch": 0},
            "distance": bytes[32]
        },
        "lineSpacing": bytes[33] as f64 / 20.0, "fftSize": bytes[34]
    }))
}

// Version 1 pins this library and configuration as part of its on-image format.
// This is a public sharing format, so the seed is not a secret or encryption key.
fn watermark_config() -> WatermarkConfig {
    WatermarkConfig {
        strength_1: 72,
        strength_2: None,
        mode: WatermarkMode::Strategy(0x5354415449430001),
    }
}

pub fn embed(image: &mut RgbImage, payload: &[u8; DATA_BYTES]) -> Result<()> {
    ensure!(
        image.dimensions() == (WIDTH, HEIGHT),
        "Invalid cartridge dimensions"
    );
    let encoded = reed_solomon::Encoder::new(ECC_BYTES).encode(payload);
    let matrix: YCrBrAMat = DynamicImage::ImageRgb8(image.clone()).into_rgba32f().into();
    let watermarked = matrix
        .add_padding()
        .dwt()
        .cut()
        .embed_watermark_bits(encoded[..].view_bits::<Lsb0>(), &watermark_config())
        .assemble()
        .idwt()
        .remove_padding();
    let pixels: image::Rgba32FImage = watermarked.into();
    *image = DynamicImage::ImageRgba32F(pixels).to_rgb8();
    Ok(())
}

pub fn decode(image: &DynamicImage) -> Result<Value> {
    ensure!(
        image.width() >= 200 && image.height() >= 280,
        "Cartridge image is too small"
    );
    let ratio = image.width() as f64 / image.height() as f64;
    ensure!(
        (ratio - WIDTH as f64 / HEIGHT as f64).abs() < 0.005,
        "Keep the complete cartridge image, without cropping or borders"
    );
    let matrix: YCrBrAMat = image
        .resize_exact(WIDTH, HEIGHT, FilterType::Triangle)
        .into_rgba32f()
        .into();
    let encoded = matrix
        .add_padding()
        .dwt()
        .cut()
        .extract_watermark_bits(CODE_BYTES * 8, &watermark_config())
        .into_vec();
    let decoded = reed_solomon::Decoder::new(ECC_BYTES)
        .correct(&encoded, None)
        .map_err(|_| {
            anyhow::anyhow!(
                "Cannot recover this cartridge: the image is unrecognised or too damaged"
            )
        })?;
    unpack(decoded.data())
}

pub fn decode_bytes(bytes: &[u8]) -> Result<Value> {
    ensure!(
        bytes.len() <= MAX_IMAGE_BYTES,
        "Cartridge image exceeds 16 MiB"
    );
    let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    ensure!(
        matches!(
            reader.format(),
            Some(image::ImageFormat::Png | image::ImageFormat::Jpeg | image::ImageFormat::WebP)
        ),
        "Use a PNG, JPEG or WebP cartridge"
    );
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(4096);
    limits.max_image_height = Some(4096);
    limits.max_alloc = Some(96 * 1024 * 1024);
    reader.limits(limits);
    decode(
        &reader
            .decode()
            .context("Invalid or oversized cartridge image")?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    fn metadata() -> Value {
        json!({"videoId":"dQw4w9WgXcQ", "uiTheme":{"primary":"#ff0080","secondary":"#10fe23","background":"#000000"}, "graphTheme":{"line1":"#abcdef","line2":"#ffffff"}, "cameraControls":{"position":{"x":-12.4,"y":20,"z":254},"rotation":{"yaw":-0.4,"roll":1.2,"pitch":0},"distance":60},"lineSpacing":2.5,"fftSize":30})
    }
    #[test]
    fn version_one_fixture_remains_readable_across_dependency_updates() {
        // Exported by the native UI, resized 50%, JPEG 4:2:0 quality 70 with
        // ImageMagick. This fixture must not be regenerated with the encoder.
        let expected: Value = serde_json::from_slice(include_bytes!(
            "../../visualizer/tests/fixtures/cartridge-v1.json"
        ))
        .unwrap();
        let recovered = decode_bytes(include_bytes!(
            "../../visualizer/tests/fixtures/cartridge-v1.jpg"
        ))
        .unwrap();
        assert_eq!(recovered, expected);
    }
    #[test]
    fn compact_layout_roundtrips_exact_id_colours_and_bounded_camera() {
        let input = metadata();
        let packed = pack(&input).unwrap();
        let output = unpack(&packed).unwrap();
        assert_eq!(packed.len(), 39);
        assert_eq!(&packed[12..18], &[255, 0, 128, 16, 254, 35]);
        assert_eq!(output["videoId"], input["videoId"]);
        assert_eq!(output["uiTheme"], input["uiTheme"]);
        assert_eq!(output["graphTheme"], input["graphTheme"]);
        assert_eq!(
            output["cameraControls"]["position"],
            json!({"x":-12,"y":20,"z":254})
        );
        assert_eq!(output["lineSpacing"], 2.5);
        assert_eq!(output["fftSize"], 30);
        assert_eq!(pack(&output).unwrap(), packed);
        let mut alternate = input.clone();
        alternate["videoId"] = json!("_-012345678");
        assert_eq!(
            unpack(&pack(&alternate).unwrap()).unwrap()["videoId"],
            alternate["videoId"]
        );
        for bad in ["short", "aaaaaaaaaa!", "雪aaaaaaaaaa"] {
            alternate["videoId"] = json!(bad);
            assert!(pack(&alternate).is_err());
        }
        let mut bad = input.clone();
        bad["cameraControls"]["distance"] = json!(256);
        assert!(pack(&bad).is_err());
        bad = input;
        bad["cameraControls"]["position"]["x"] = json!(-257);
        assert!(pack(&bad).is_err());
    }
    #[test]
    fn reed_solomon_repairs_64_corrupt_bytes_and_crc_rejects_untrusted_data() {
        let payload = pack(&metadata()).unwrap();
        let mut encoded = reed_solomon::Encoder::new(ECC_BYTES)
            .encode(&payload)
            .to_vec();
        for i in 0..64 {
            encoded[(i * 17) % CODE_BYTES] ^= 0xa5;
        }
        let recovered = reed_solomon::Decoder::new(ECC_BYTES)
            .correct(&encoded, None)
            .unwrap();
        assert_eq!(recovered.data(), payload);
        let mut damaged = payload;
        damaged[12] ^= 1;
        assert!(unpack(&damaged).is_err());
        assert!(unpack(&[]).is_err());
    }
    fn fixture() -> RgbImage {
        RgbImage::from_fn(WIDTH, HEIGHT, |x, y| {
            let shade =
                (30.0 + 15.0 * (x as f64 / 120.0).sin() + 20.0 * y as f64 / HEIGHT as f64) as u8;
            let line = y < 550 && ((x as f64 / 25.0).sin() * 50.0 + 250.0 - y as f64).abs() < 2.0;
            image::Rgb(if line {
                [220, 140, 255]
            } else {
                [shade, shade / 2, shade + 20]
            })
        })
    }
    fn jpeg(image: &DynamicImage, quality: u8) -> DynamicImage {
        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, quality)
            .encode_image(image)
            .unwrap();
        image::load_from_memory(&bytes).unwrap()
    }
    #[test]
    fn library_watermark_recovers_after_jpeg_and_resize() {
        let payload = pack(&metadata()).unwrap();
        let expected = unpack(&payload).unwrap();
        let mut image = fixture();
        embed(&mut image, &payload).unwrap();
        let original = DynamicImage::ImageRgb8(image);
        assert_eq!(decode(&original).unwrap(), expected);
        let mut failures = Vec::new();
        for quality in [90, 70, 50, 30] {
            let compressed = jpeg(&original, quality);
            match decode(&compressed) {
                Ok(actual) => assert_eq!(actual, expected),
                Err(err) => failures.push(format!("JPEG {quality}: {err}")),
            }
        }
        for width in [600, 400] {
            let scaled = original.resize_exact(width, width * 7 / 5, FilterType::Lanczos3);
            let compressed = jpeg(&scaled, 70);
            match decode(&compressed) {
                Ok(actual) => assert_eq!(actual, expected),
                Err(err) => failures.push(format!("resize {width}, JPEG70: {err}")),
            }
        }
        let twice = jpeg(&jpeg(&original, 70), 50);
        match decode(&twice) {
            Ok(actual) => assert_eq!(actual, expected),
            Err(err) => failures.push(format!("twice JPEG: {err}")),
        }
        assert!(failures.is_empty(), "{failures:?}");
    }
    #[test]
    fn arbitrary_ecc_input_is_rejected_without_panicking() {
        use rand::{RngExt, SeedableRng};
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        for _ in 0..512 {
            let bytes: Vec<u8> = (0..CODE_BYTES).map(|_| rng.random()).collect();
            if let Ok(decoded) = reed_solomon::Decoder::new(ECC_BYTES).correct(&bytes, None) {
                assert!(unpack(decoded.data()).is_err());
            }
        }
    }
    #[test]
    fn malformed_unmarked_and_oversized_images_are_rejected() {
        assert!(decode_bytes(b"not an image").is_err());
        assert!(decode_bytes(&vec![0; MAX_IMAGE_BYTES + 1]).is_err());
        assert!(decode(&DynamicImage::new_rgb8(800, 800)).is_err());
        assert!(decode(&DynamicImage::ImageRgb8(fixture())).is_err());
    }
}
