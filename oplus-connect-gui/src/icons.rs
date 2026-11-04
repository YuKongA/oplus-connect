use base64::Engine as _;
use gpui_kit::{Image, ImageFormat};
use std::sync::Arc;

/// Decodes the notification icon the phone sends. The payload is a PNG in
/// practice, either bare base64 or a `data:` URI; unsupported data yields no
/// icon rather than a broken image.
pub fn decode_notification_icon(encoded: &str) -> Option<Arc<Image>> {
    let payload = encoded.trim();
    let payload = match payload.strip_prefix("data:") {
        Some(rest) => rest.split_once(',').map(|(_, data)| data).unwrap_or(rest),
        None => payload,
    };
    let bytes = decode_base64(payload.trim())?;
    let format = image_format(&bytes)?;
    Some(Arc::new(Image::from_bytes(format, bytes)))
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

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nrest";

    #[test]
    fn decodes_bare_and_data_uri_icons() {
        let engine = base64::engine::general_purpose::STANDARD;
        let bare = engine.encode(PNG);
        assert!(decode_notification_icon(&bare).is_some());
        assert!(decode_notification_icon(&format!("data:image/png;base64,{bare}")).is_some());
    }

    #[test]
    fn rejects_unsupported_or_invalid_icons() {
        let engine = base64::engine::general_purpose::STANDARD;
        assert!(decode_notification_icon("not base64!").is_none());
        assert!(decode_notification_icon(&engine.encode(b"plain text")).is_none());
    }
}
