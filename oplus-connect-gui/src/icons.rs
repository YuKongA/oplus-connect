use base64::Engine as _;
use gpui_kit::{Image, ImageFormat};
use std::sync::Arc;

pub const MAX_ENCODED_ICON_LEN: usize = 512 * 1024;
const MAX_ICON_BYTES: usize = 384 * 1024;
/// A tiny compressed payload must not become a multi-gigabyte RGBA allocation.
const MAX_ICON_EDGE: u32 = 512;

pub fn decode_notification_icon(encoded: &str) -> Option<Arc<Image>> {
    let payload = encoded.trim();
    if payload.len() > MAX_ENCODED_ICON_LEN {
        return None;
    }
    let payload = match payload.strip_prefix("data:") {
        Some(rest) => rest.split_once(',').map(|(_, data)| data).unwrap_or(rest),
        None => payload,
    };
    let bytes = decode_base64(payload.trim())?;
    if bytes.len() > MAX_ICON_BYTES {
        return None;
    }
    let format = image_format(&bytes)?;
    if !dimensions_within_limit(&bytes) {
        return None;
    }
    Some(Arc::new(Image::from_bytes(format, bytes)))
}

fn dimensions_within_limit(bytes: &[u8]) -> bool {
    match image_dimensions(bytes) {
        Some((width, height)) => {
            width > 0 && height > 0 && width <= MAX_ICON_EDGE && height <= MAX_ICON_EDGE
        }
        None => false,
    }
}

fn decode_base64(payload: &str) -> Option<Vec<u8>> {
    let compact: String = payload.chars().filter(|ch| !ch.is_whitespace()).collect();
    let engines = [
        base64::engine::general_purpose::STANDARD,
        base64::engine::general_purpose::STANDARD_NO_PAD,
        base64::engine::general_purpose::URL_SAFE,
        base64::engine::general_purpose::URL_SAFE_NO_PAD,
    ];
    engines
        .iter()
        .find_map(|engine| engine.decode(&compact).ok())
}

fn image_format(bytes: &[u8]) -> Option<ImageFormat> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(ImageFormat::Png)
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some(ImageFormat::Jpeg)
    } else if bytes.starts_with(b"GIF8") {
        Some(ImageFormat::Gif)
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some(ImageFormat::Webp)
    } else {
        None
    }
}

fn image_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let format = image_format(bytes)?;
    match format {
        ImageFormat::Png => {
            // Signature (8) + IHDR length (4) + "IHDR" (4) + width (4) + height (4).
            if bytes.len() < 24 || &bytes[12..16] != b"IHDR" {
                return None;
            }
            let width = u32::from_be_bytes(bytes.get(16..20)?.try_into().ok()?);
            let height = u32::from_be_bytes(bytes.get(20..24)?.try_into().ok()?);
            Some((width, height))
        }
        ImageFormat::Gif => {
            if bytes.len() < 10 {
                return None;
            }
            let width = u32::from(u16::from_le_bytes(bytes.get(6..8)?.try_into().ok()?));
            let height = u32::from(u16::from_le_bytes(bytes.get(8..10)?.try_into().ok()?));
            Some((width, height))
        }
        ImageFormat::Jpeg => jpeg_dimensions(bytes),
        ImageFormat::Webp => webp_dimensions(bytes),
        _ => None,
    }
}

fn jpeg_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let mut cursor = 2;
    while cursor + 4 <= bytes.len() {
        if bytes[cursor] != 0xff {
            return None;
        }
        let marker = bytes[cursor + 1];
        if marker == 0xff {
            cursor += 1;
            continue;
        }
        if marker == 0xd8 || marker == 0xd9 || (0xd0..=0xd7).contains(&marker) {
            cursor += 2;
            continue;
        }
        let length = usize::from(u16::from_be_bytes(
            bytes.get(cursor + 2..cursor + 4)?.try_into().ok()?,
        ));
        // SOF0..SOF15 carry the frame size; C4/C8/CC are DHT/JPG/DAC instead.
        if (0xc0..=0xcf).contains(&marker) && !matches!(marker, 0xc4 | 0xc8 | 0xcc) {
            if cursor + 9 > bytes.len() {
                return None;
            }
            let height = u32::from(u16::from_be_bytes(
                bytes.get(cursor + 5..cursor + 7)?.try_into().ok()?,
            ));
            let width = u32::from(u16::from_be_bytes(
                bytes.get(cursor + 7..cursor + 9)?.try_into().ok()?,
            ));
            return Some((width, height));
        }
        cursor = cursor.checked_add(2 + length)?;
    }
    None
}

fn webp_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let chunk = bytes.get(12..16)?;
    let payload = 20;
    match chunk {
        // Extended format: 4 flag/reserved bytes, then 24-bit canvas size minus one.
        b"VP8X" => {
            let width = u32::from_le_bytes([
                *bytes.get(payload + 4)?,
                *bytes.get(payload + 5)?,
                *bytes.get(payload + 6)?,
                0,
            ]) + 1;
            let height = u32::from_le_bytes([
                *bytes.get(payload + 7)?,
                *bytes.get(payload + 8)?,
                *bytes.get(payload + 9)?,
                0,
            ]) + 1;
            Some((width, height))
        }
        // Lossless: 0x2f signature then 14-bit width and height minus one.
        b"VP8L" => {
            if *bytes.get(payload)? != 0x2f {
                return None;
            }
            let bits = u32::from_le_bytes(bytes.get(payload + 1..payload + 5)?.try_into().ok()?);
            Some(((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1))
        }
        // Lossy: 3-byte frame tag, start code, then 14-bit width/height.
        b"VP8 " => {
            if bytes.get(payload + 3..payload + 6)? != [0x9d, 0x01, 0x2a] {
                return None;
            }
            let width = u32::from(
                u16::from_le_bytes(bytes.get(payload + 6..payload + 8)?.try_into().ok()?) & 0x3fff,
            );
            let height = u32::from(
                u16::from_le_bytes(bytes.get(payload + 8..payload + 10)?.try_into().ok()?) & 0x3fff,
            );
            Some((width, height))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nrest";

    fn png_with_size(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Vec::from(&b"\x89PNG\r\n\x1a\n"[..]);
        bytes.extend_from_slice(&13u32.to_be_bytes());
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes
    }

    #[test]
    fn decodes_bare_and_data_uri_icons() {
        let engine = base64::engine::general_purpose::STANDARD;
        let bare = engine.encode(png_with_size(64, 64));
        assert!(decode_notification_icon(&bare).is_some());
        assert!(decode_notification_icon(&format!("data:image/png;base64,{bare}")).is_some());
    }

    #[test]
    fn rejects_unsupported_or_invalid_icons() {
        let engine = base64::engine::general_purpose::STANDARD;
        assert!(decode_notification_icon("not base64!").is_none());
        assert!(decode_notification_icon(&engine.encode(b"plain text")).is_none());
    }

    #[test]
    fn rejects_declared_dimensions_beyond_the_limit() {
        let engine = base64::engine::general_purpose::STANDARD;
        let oversized = engine.encode(png_with_size(MAX_ICON_EDGE + 1, 16));
        assert!(decode_notification_icon(&oversized).is_none());
        let zero = engine.encode(png_with_size(0, 16));
        assert!(decode_notification_icon(&zero).is_none());
        let at_limit = engine.encode(png_with_size(MAX_ICON_EDGE, MAX_ICON_EDGE));
        assert!(decode_notification_icon(&at_limit).is_some());
    }

    #[test]
    fn rejects_oversized_payloads() {
        let engine = base64::engine::general_purpose::STANDARD;
        let mut bytes = png_with_size(64, 64);
        bytes.resize(MAX_ICON_BYTES + 1, 0);
        assert!(decode_notification_icon(&engine.encode(bytes)).is_none());

        let huge = "A".repeat(MAX_ENCODED_ICON_LEN + 1);
        assert!(decode_notification_icon(&huge).is_none());
    }

    #[test]
    fn reads_headers_for_every_supported_format() {
        let mut gif = Vec::from(&b"GIF89a"[..]);
        gif.extend_from_slice(&32u16.to_le_bytes());
        gif.extend_from_slice(&16u16.to_le_bytes());
        assert_eq!(image_dimensions(&gif), Some((32, 16)));

        let mut webp = Vec::from(&b"RIFF\0\0\0\0WEBPVP8L"[..]);
        webp.extend_from_slice(&5u32.to_le_bytes());
        webp.push(0x2f);
        let bits: u32 = (31) | (15 << 14); // 32x16, stored minus one
        webp.extend_from_slice(&bits.to_le_bytes());
        assert_eq!(image_dimensions(&webp), Some((32, 16)));

        let jpeg = [
            0xff, 0xd8, 0xff, 0xe0, 0x00, 0x04, 0x00, 0x00, 0xff, 0xc0, 0x00, 0x0b, 0x08, 0x00,
            0x10, 0x00, 0x20, 0x01,
        ];
        assert_eq!(image_dimensions(&jpeg), Some((32, 16)));
    }
}
