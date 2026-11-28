use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD_PAD_INDIFFERENT, URL_SAFE_PAD_INDIFFERENT};
use gpui_kit::{App, AppContext, Entity, RenderImage};
use image::{Frame, ImageFormat, ImageReader, Limits};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::Cursor;
use std::sync::{Arc, OnceLock, Weak};

const MAX_ENCODED_ICON_LEN: usize = 512 * 1024;
const MAX_ICON_BYTES: usize = 384 * 1024;
const MAX_ICON_EDGE: u32 = 512;
// Notification rows display 20 logical pixels. Preserve detail at 3× scale.
const THUMBNAIL_EDGE: u32 = 60;

#[derive(Debug)]
pub struct NotificationIcon {
    image: Arc<RenderImage>,
    atlas_owner: OnceLock<Entity<()>>,
}

impl NotificationIcon {
    pub(crate) fn image(&self) -> &Arc<RenderImage> {
        &self.image
    }

    pub(crate) fn attach(&self, app: &mut App) {
        // One owner per image ID, not per record or window. When the last record
        // or queued notification drops it, GPUI runs on_release at effect flush,
        // outside the window update, with every surviving window available.
        self.atlas_owner.get_or_init(|| {
            let image = self.image.clone();
            app.new(|cx| {
                cx.on_release(move |_, app| app.drop_image(image, None))
                    .detach();
            })
        });
    }
}

/// Private to one ordered background event stream. The index never owns pixels
/// or encoded bytes, so history deletion needs no separate cache invalidation.
#[derive(Default)]
pub(crate) struct NotificationIcons(HashMap<[u8; 32], Weak<NotificationIcon>>);

impl NotificationIcons {
    pub(crate) fn decode(&mut self, encoded: &str) -> Option<Arc<NotificationIcon>> {
        if encoded.len() > MAX_ENCODED_ICON_LEN {
            return None;
        }
        let payload = encoded.trim();
        let payload = match payload.strip_prefix("data:") {
            Some(uri) => uri.split_once(',')?.1,
            None => payload,
        };
        let compact;
        let payload = if payload.bytes().any(|b| b.is_ascii_whitespace()) {
            compact = payload
                .bytes()
                .filter(|b| !b.is_ascii_whitespace())
                .collect::<Vec<_>>();
            &compact[..]
        } else {
            payload.as_bytes()
        };
        let engine = if payload.iter().any(|b| matches!(*b, b'-' | b'_')) {
            &URL_SAFE_PAD_INDIFFERENT
        } else {
            &STANDARD_PAD_INDIFFERENT
        };
        let bytes = engine.decode(payload).ok()?;
        if bytes.len() > MAX_ICON_BYTES {
            return None;
        }
        let digest = Sha256::digest(&bytes).into();
        if let Some(icon) = self.0.get(&digest).and_then(Weak::upgrade) {
            return Some(icon);
        }
        let image = thumbnail(&bytes)?;
        self.0.retain(|_, icon| icon.strong_count() != 0);
        let icon = Arc::new(NotificationIcon {
            image: Arc::new(image),
            atlas_owner: OnceLock::new(),
        });
        self.0.insert(digest, Arc::downgrade(&icon));
        Some(icon)
    }
}

fn thumbnail(bytes: &[u8]) -> Option<RenderImage> {
    let format = image::guess_format(bytes).ok()?;
    match format {
        ImageFormat::Gif | ImageFormat::Jpeg | ImageFormat::Png => {}
        ImageFormat::WebP => check_webp(bytes)?,
        _ => return None,
    }
    let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_ICON_EDGE);
    limits.max_image_height = Some(MAX_ICON_EDGE);
    limits.max_alloc = Some(4 * 1024 * 1024);
    reader.limits(limits);
    // Still-image decode reads only the first displayed GIF/WebP frame.
    let image = reader.decode().ok()?;
    if image.width() == 0 || image.height() == 0 {
        return None;
    }
    let mut pixels = if image.width().max(image.height()) > THUMBNAIL_EDGE {
        image.thumbnail(THUMBNAIL_EDGE, THUMBNAIL_EDGE)
    } else {
        image
    }
    .into_rgba8();
    for pixel in pixels.pixels_mut() {
        pixel.0.swap(0, 2); // GPUI expects BGRA, including the original alpha.
    }
    Some(RenderImage::new([Frame::new(pixels)]))
}

// image-webp decodes an embedded lossy VP8 stream before comparing its size
// to the outer canvas/frame. Bound those dimensions before entering the codec.
fn check_webp(bytes: &[u8]) -> Option<()> {
    fn chunk<'a>(bytes: &mut &'a [u8]) -> Option<(&'a [u8], &'a [u8])> {
        let kind = bytes.get(..4)?;
        let len = u32::from_le_bytes(bytes.get(4..8)?.try_into().ok()?) as usize;
        let data = bytes.get(8..)?.get(..len)?;
        *bytes = bytes.get(8..)?.get(len.checked_add(len % 2)?..)?;
        Some((kind, data))
    }
    fn u24(data: &[u8]) -> Option<u32> {
        Some(u32::from_le_bytes([
            *data.first()?,
            *data.get(1)?,
            *data.get(2)?,
            0,
        ]))
    }
    fn check_vp8(data: &[u8], frame: Option<(u32, u32)>) -> Option<()> {
        let edge = |data: &[u8]| u32::from(u16::from_le_bytes([data[0], data[1]]) & 0x3fff);
        let dims = (edge(data.get(6..8)?), edge(data.get(8..10)?));
        ((1..=MAX_ICON_EDGE).contains(&dims.0)
            && (1..=MAX_ICON_EDGE).contains(&dims.1)
            && frame.is_none_or(|frame| frame == dims))
        .then_some(())
    }

    let size = u32::from_le_bytes(bytes.get(4..8)?.try_into().ok()?) as usize;
    if bytes.len().checked_sub(8)? != size {
        return None;
    }
    let mut chunks = bytes.get(12..)?;
    let mut canvas = None;
    while !chunks.is_empty() {
        let (kind, data) = chunk(&mut chunks)?;
        match kind {
            b"VP8X" => canvas = Some((1 + u24(data.get(4..)?)?, 1 + u24(data.get(7..)?)?)),
            b"VP8 " => check_vp8(data, canvas)?,
            b"ANMF" => {
                let dims = (1 + u24(data.get(6..)?)?, 1 + u24(data.get(9..)?)?);
                let mut subchunks = data.get(16..)?;
                while !subchunks.is_empty() {
                    let (kind, data) = chunk(&mut subchunks)?;
                    if kind == b"VP8 " {
                        check_vp8(data, Some(dims))?;
                    }
                }
            }
            _ => {}
        }
    }
    Some(())
}

#[cfg(test)]
pub(crate) const TEST_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAAIAAAABCAYAAAD0In+KAAAAEUlEQVQI12PkUbJoYGBgYAAABTAA6HbbxHIAAAAASUVORK5CYII=";
#[cfg(test)]
pub(crate) const TEST_GIF: &str = "R0lGODlhBAACAPAAAP8AAAAAACH/C05FVFNDQVBFMi4wAwEAAAAh+QQAAAAAACwAAAAABAACAAACA4RvBQAh+QQAAAAAACwAAAAABAACAIAAAP8AAAACA4RvBQA7";

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
    use image::{DynamicImage, RgbImage};

    const WEBP: &str = "UklGRoQAAABXRUJQVlA4WAoAAAACAAAAAwAAAQAAQU5JTQYAAAD/////AABBTk1GKAAAAAAAAAAAAAMAAAEAAGQAAAJWUDhMDwAAAC8DQAAABxD9j/4HIqL/AQBBTk1GKAAAAAAAAAAAAAMAAAEAAGQAAABWUDhMDwAAAC8DQAAABxDR//4HIqL/AQA=";

    fn encoded(width: u32, height: u32, format: ImageFormat) -> String {
        let mut bytes = Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(RgbImage::new(width, height))
            .write_to(&mut bytes, format)
            .unwrap();
        STANDARD.encode(bytes.into_inner())
    }

    #[test]
    fn png_preserves_bgra_and_shares_an_image_across_wire_encodings() {
        let mut cache = NotificationIcons::default();
        let a = cache.decode(TEST_PNG).unwrap();
        assert_eq!(
            a.image.as_bytes(0).unwrap(),
            &[56, 34, 12, 128, 56, 34, 12, 128]
        );
        let url = URL_SAFE_NO_PAD.encode(STANDARD.decode(TEST_PNG).unwrap());
        let b = cache
            .decode(&format!("data:image/png;base64,\n{url}"))
            .unwrap();
        assert!(Arc::ptr_eq(&a, &b));
    }

    #[test]
    fn animated_gif_and_webp_keep_only_the_first_frame() {
        for data in [TEST_GIF, WEBP] {
            let icon = NotificationIcons::default().decode(data).unwrap();
            assert_eq!(icon.image.frame_count(), 1);
            assert_eq!(icon.image.as_bytes(0).unwrap(), [0, 0, 255, 255].repeat(8));
        }
    }

    #[test]
    fn extended_lossy_webp_keeps_only_the_first_frame() {
        let data = "UklGRsAAAABXRUJQVlA4WAoAAAACAAAAAwAAAQAAQU5JTQYAAAD/////AABBTk1GSAAAAAAAAAAAAAMAAAEAAGQAAAJWUDggMAAAANABAJ0BKgQAAgACADQloAJ0ugH4AAOwAP7wxAv/ILlhdcjX/yA/5Af8gP/48gAAAEFOTUZEAAAAAAAAAAAAAwAAAQAAZAAAAFZQOCAsAAAAlAEAnQEqBAACAAAANCWgAnS6AAOYAP75k2//kB//kB//kB//ID/iF3sgMAA=";
        let icon = NotificationIcons::default().decode(data).unwrap();
        assert_eq!(icon.image.frame_count(), 1);
        let pixel = &icon.image.as_bytes(0).unwrap()[..4];
        assert!(pixel[2] > 230 && pixel[0] < 20 && pixel[1] < 20 && pixel[3] == 255);
    }

    #[test]
    fn thumbnails_limit_retained_pixels_without_upscaling() {
        for format in [ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::WebP] {
            let icon = NotificationIcons::default()
                .decode(&encoded(512, 256, format))
                .unwrap();
            let size = icon.image.size(0);
            assert_eq!((size.width.0, size.height.0), (60, 30));
            assert_eq!(icon.image.as_bytes(0).unwrap().len(), 60 * 30 * 4);
        }
    }

    #[test]
    fn rejects_invalid_truncated_oversized_and_unsupported_icons() {
        let mut cache = NotificationIcons::default();
        for input in [
            "not base64!".to_owned(),
            STANDARD.encode(b"plain text"),
            STANDARD.encode(&STANDARD.decode(TEST_PNG).unwrap()[..24]),
            " ".repeat(MAX_ENCODED_ICON_LEN + 1),
            encoded(MAX_ICON_EDGE + 1, 1, ImageFormat::Png),
        ] {
            assert!(cache.decode(&input).is_none());
        }
    }
}
