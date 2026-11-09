//! Reverse-control frames for the phone's cast data channel.
//!
//! Big-endian frames recovered from the official client's debug log:
//! 24-byte key frames, 34-byte touch frames, and 31-byte scroll frames.

/// `ACTION_DOWN`
pub const ACTION_DOWN: u8 = 0;
/// `ACTION_UP`
pub const ACTION_UP: u8 = 1;
/// `ACTION_MOVE` (pointer held)
pub const ACTION_MOVE: u8 = 2;
/// `ACTION_HOVER_MOVE` (pointer moving with no button)
pub const ACTION_HOVER_MOVE: u8 = 7;

const MAGIC: [u8; 4] = [0x81, 0x03, 0x01, 0x07];
const TYPE_TOUCH: u16 = 0x20;
const TYPE_KEY: u16 = 0x0e;
const CLASS_TOUCH: u8 = 2;
const CLASS_KEY: u8 = 0;

/// Android `META_SHIFT_ON | META_SHIFT_LEFT_ON`.
pub const META_SHIFT: u32 = 0x41;
/// Android `META_ALT_ON | META_ALT_LEFT_ON`.
pub const META_ALT: u32 = 0x12;
/// Android `META_CTRL_ON | META_CTRL_LEFT_ON`.
pub const META_CTRL: u32 = 0x3000;

/// Android `KEYCODE_HOME`.
pub const KEYCODE_HOME: u8 = 3;
/// Android `KEYCODE_BACK`.
pub const KEYCODE_BACK: u8 = 4;
/// Android `KEYCODE_APP_SWITCH`.
pub const KEYCODE_APP_SWITCH: u8 = 187;

/// Height of the cast window's three-button navigation bar below the picture.
pub const NAV_BAR_HEIGHT: f32 = 44.0;

fn frame(
    kind: u16,
    class: u8,
    flag: u8,
    action: u8,
    code: u8,
    meta: u32,
    x: i32,
    y: i32,
) -> [u8; 24] {
    let mut frame = [0u8; 24];
    frame[..4].copy_from_slice(&MAGIC);
    frame[4..6].copy_from_slice(&kind.to_be_bytes());
    frame[6] = class;
    // Byte 7 is the press/release flag; byte 10 is the MotionEvent action,
    // which only touch frames use.
    frame[7] = flag;
    frame[10] = if class == CLASS_TOUCH { action } else { 0 };
    frame[11] = code;
    frame[12..16].copy_from_slice(&meta.to_be_bytes());
    frame[16..20].copy_from_slice(&x.to_be_bytes());
    frame[20..24].copy_from_slice(&y.to_be_bytes());
    frame
}

/// Touch in `width`x`height` pixel space.
///
/// The phone reads the pointer record at 12..26 and then a tail of
/// `width u16`, `height u16`, `deviceId u32` from the last 8 payload bytes, so
/// the payload must be 34 bytes; a 24-byte frame made it read x/y as the tail.
pub fn touch(action: u8, x: i32, y: i32, width: u32, height: u32) -> Vec<u8> {
    let mut out = frame(TYPE_TOUCH, CLASS_TOUCH, 0, action, 1, 0, x, y).to_vec();
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&(width.min(u16::MAX as u32) as u16).to_be_bytes());
    out.extend_from_slice(&(height.min(u16::MAX as u32) as u16).to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
    out
}

/// Key press (`pressed`) or release with the Android keycode and meta state.
pub fn key(keycode: u8, meta: u32, pressed: bool) -> [u8; 24] {
    frame(
        TYPE_KEY,
        CLASS_KEY,
        u8::from(!pressed),
        0,
        keycode,
        meta,
        0,
        0,
    )
}

/// Wheel event: pointer position, its coordinate space, then the two scroll
/// axis values (the phone applies them through `MotionEvent.ACTION_SCROLL`).
pub fn scroll(delta_x: i32, delta_y: i32, x: i32, y: i32, width: u32, height: u32) -> Vec<u8> {
    let mut out = vec![0u8; 31];
    out[..4].copy_from_slice(&MAGIC);
    out[4..6].copy_from_slice(&TYPE_KEY.to_be_bytes());
    out[6] = 0x03; // event type 3 = scroll
    out[7..11].copy_from_slice(&x.to_be_bytes());
    out[11..15].copy_from_slice(&y.to_be_bytes());
    out[15..17].copy_from_slice(&(width.min(u16::MAX as u32) as u16).to_be_bytes());
    out[17..19].copy_from_slice(&(height.min(u16::MAX as u32) as u16).to_be_bytes());
    out[19..23].copy_from_slice(&delta_x.to_be_bytes());
    out[23..27].copy_from_slice(&delta_y.to_be_bytes());
    out[27..31].copy_from_slice(&0u32.to_be_bytes());
    out
}

/// Android keycode for a GPUI keystroke key name.
pub fn android_keycode(key: &str) -> Option<u8> {
    let lower = key.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    if bytes.len() == 1 && bytes[0].is_ascii_lowercase() {
        return Some(bytes[0] - b'a' + 29);
    }
    if bytes.len() == 1 && bytes[0].is_ascii_digit() {
        return Some(bytes[0] - b'0' + 7);
    }
    match lower.as_str() {
        "space" => Some(62),
        "enter" => Some(66),
        "backspace" => Some(67),
        "tab" => Some(61),
        "escape" => Some(111),
        "delete" => Some(112),
        "shift" => Some(59),
        "control" => Some(113),
        "alt" => Some(57),
        "up" => Some(19),
        "down" => Some(20),
        "left" => Some(21),
        "right" => Some(22),
        "minus" | "-" => Some(69),
        "equal" | "=" => Some(70),
        "comma" | "," => Some(55),
        "period" | "." => Some(56),
        "slash" | "/" => Some(76),
        "semicolon" | ";" => Some(74),
        "quote" | "'" => Some(75),
        "backslash" | "\\" => Some(73),
        "left_bracket" | "[" => Some(71),
        "right_bracket" | "]" => Some(72),
        "backquote" | "`" => Some(68),
        _ => None,
    }
}

/// Android meta state for held modifiers.
pub fn meta_state(shift: bool, control: bool, alt: bool) -> u32 {
    (if shift { META_SHIFT } else { 0 })
        | (if control { META_CTRL } else { 0 })
        | (if alt { META_ALT } else { 0 })
}

const MODIFIER_KEYS: [(u32, u8); 3] = [(META_SHIFT, 59), (META_CTRL, 113), (META_ALT, 57)];

pub(crate) struct KeyboardState {
    held: [bool; 256],
    modifiers: u32,
}

impl Default for KeyboardState {
    fn default() -> Self {
        Self {
            held: [false; 256],
            modifiers: 0,
        }
    }
}

impl KeyboardState {
    pub fn modifiers_changed(&mut self, meta: u32, mut emit: impl FnMut([u8; 24])) {
        for (bit, code) in MODIFIER_KEYS {
            let pressed = meta & bit != 0;
            if pressed != (self.modifiers & bit != 0) {
                emit(key(code, meta, pressed));
            }
        }
        self.modifiers = meta;
    }

    pub fn key_changed(
        &mut self,
        code: Option<u8>,
        mut meta: u32,
        pressed: bool,
        mut emit: impl FnMut([u8; 24]),
    ) {
        if let Some((bit, _)) = MODIFIER_KEYS
            .iter()
            .copied()
            .find(|(_, modifier)| Some(*modifier) == code)
        {
            if pressed {
                meta |= bit;
            } else {
                meta &= !bit;
            }
            self.modifiers_changed(meta, emit);
            return;
        }
        if pressed {
            self.modifiers_changed(meta, &mut emit);
        }
        if let Some(code) = code {
            let held = &mut self.held[usize::from(code)];
            if pressed || *held {
                emit(key(code, meta, pressed));
                *held = pressed;
            }
        }
        if !pressed {
            self.modifiers_changed(meta, emit);
        }
    }

    pub fn release(&mut self, mut emit: impl FnMut([u8; 24])) {
        for (code, held) in self.held.iter_mut().enumerate() {
            if *held {
                emit(key(code as u8, 0, false));
                *held = false;
            }
        }
        self.modifiers_changed(0, emit);
    }
}

/// Picture rectangle inside the window: the picture keeps its aspect ratio, so
/// the offset is the letterbox margin when the window is not the phone's shape.
/// `titlebar` and `bottom` are window chrome and reduce the available height.
pub fn picture_rect(
    window: (f32, f32),
    titlebar: f32,
    bottom: f32,
    phone: (u32, u32),
) -> (f32, f32, f32) {
    let (window_width, window_height) = window;
    let content_height = (window_height - titlebar - bottom).max(1.0);
    let phone_width = phone.0.max(1) as f32;
    let phone_height = phone.1.max(1) as f32;
    let scale = (window_width / phone_width).min(content_height / phone_height);
    let left = (window_width - phone_width * scale) / 2.0;
    let top = titlebar + (content_height - phone_height * scale) / 2.0;
    (left, top, scale.max(f32::MIN_POSITIVE))
}

/// Window point to phone pixel, clamped to the picture.
pub fn to_phone(
    position: (f32, f32),
    window: (f32, f32),
    titlebar: f32,
    bottom: f32,
    phone: (u32, u32),
) -> (i32, i32) {
    let (left, top, scale) = picture_rect(window, titlebar, bottom, phone);
    let x = ((position.0 - left) / scale).round();
    let y = ((position.1 - top) / scale).round();
    (
        x.clamp(0.0, (phone.0.saturating_sub(1)) as f32) as i32,
        y.clamp(0.0, (phone.1.saturating_sub(1)) as f32) as i32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modifier_events_and_key_events_emit_one_transition() {
        let mut keyboard = KeyboardState::default();
        let mut frames = Vec::new();
        keyboard.modifiers_changed(META_CTRL, |frame| frames.push(frame));
        keyboard.key_changed(Some(113), META_CTRL, true, |frame| frames.push(frame));
        keyboard.key_changed(Some(113), 0, false, |frame| frames.push(frame));
        keyboard.modifiers_changed(0, |frame| frames.push(frame));
        assert_eq!(frames, vec![key(113, META_CTRL, true), key(113, 0, false)]);
    }

    #[test]
    fn focus_loss_releases_keys_and_modifiers_only_once() {
        let mut keyboard = KeyboardState::default();
        let mut frames = Vec::new();
        keyboard.key_changed(Some(50), META_CTRL, true, |frame| frames.push(frame));
        keyboard.release(|frame| frames.push(frame));
        keyboard.release(|frame| frames.push(frame));
        keyboard.key_changed(Some(50), 0, false, |frame| frames.push(frame));
        assert_eq!(
            frames,
            vec![
                key(113, META_CTRL, true),
                key(50, META_CTRL, true),
                key(50, 0, false),
                key(113, 0, false),
            ]
        );
    }

    #[test]
    fn modifier_changes_release_without_a_mapped_key() {
        let mut keyboard = KeyboardState::default();
        let mut frames = Vec::new();
        keyboard.key_changed(Some(59), 0, true, |frame| frames.push(frame));
        keyboard.key_changed(None, 0, false, |frame| frames.push(frame));
        assert_eq!(frames, vec![key(59, META_SHIFT, true), key(59, 0, false)]);
    }

    #[test]
    fn ordinary_key_repeats_preserve_one_final_release() {
        let mut keyboard = KeyboardState::default();
        let mut frames = Vec::new();
        keyboard.key_changed(Some(29), 0, true, |frame| frames.push(frame));
        keyboard.key_changed(Some(29), 0, true, |frame| frames.push(frame));
        keyboard.key_changed(Some(29), 0, false, |frame| frames.push(frame));
        keyboard.release(|frame| frames.push(frame));
        assert_eq!(
            frames,
            vec![key(29, 0, true), key(29, 0, true), key(29, 0, false)]
        );
    }

    #[test]
    fn touch_frame_matches_captured_bytes() {
        // Documented example: ACTION_HOVER_MOVE at (468, 1370) plus the tail the
        // phone reads its coordinate space from.
        assert_eq!(
            touch(ACTION_HOVER_MOVE, 468, 1370, 720, 1560),
            vec![
                0x81, 0x03, 0x01, 0x07, 0x00, 0x20, 0x02, 0x00, 0x00, 0x00, 0x07, 0x01, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x01, 0xD4, 0x00, 0x00, 0x05, 0x5A, 0x00, 0x00, 0x02, 0xD0,
                0x06, 0x18, 0x00, 0x00, 0x00, 0x00,
            ]
        );
    }

    #[test]
    fn key_frame_matches_captured_bytes() {
        // Documented example: keycode 31 ('C') pressed with Shift.
        assert_eq!(
            key(31, META_SHIFT, true),
            [
                0x81, 0x03, 0x01, 0x07, 0x00, 0x0E, 0x00, 0x00, 0x00, 0x00, 0x00, 0x1F, 0x00, 0x00,
                0x00, 0x41, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            ]
        );
        assert_eq!(key(31, META_SHIFT, false)[7], 1);
    }

    #[test]
    fn keycodes_follow_the_android_table() {
        assert_eq!(android_keycode("a"), Some(29));
        assert_eq!(android_keycode("Z"), Some(54));
        assert_eq!(android_keycode("0"), Some(7));
        assert_eq!(android_keycode("9"), Some(16));
        assert_eq!(android_keycode("-"), Some(69));
        assert_eq!(android_keycode("shift"), Some(59));
        assert_eq!(android_keycode("unknown-key"), None);
    }

    #[test]
    fn phone_coordinates_map_through_the_picture_rect() {
        // A 720x1560 phone in a 720-wide window: no letterbox, titlebar above.
        assert_eq!(
            to_phone(
                (360.0, 34.0 + 780.0),
                (720.0, 34.0 + 1560.0),
                34.0,
                0.0,
                (720, 1560)
            ),
            (360, 780)
        );
        // Outside the picture clamps instead of sending negative coordinates.
        assert_eq!(
            to_phone((0.0, 0.0), (720.0, 34.0 + 1560.0), 34.0, 0.0, (720, 1560)),
            (0, 0)
        );
        // Excluding the navigation bar keeps the phone mapping unchanged.
        assert_eq!(
            to_phone(
                (360.0, 34.0 + 780.0),
                (720.0, 34.0 + 1560.0 + NAV_BAR_HEIGHT),
                34.0,
                NAV_BAR_HEIGHT,
                (720, 1560)
            ),
            (360, 780)
        );
        // A bar-shrunk picture area letterboxes instead of cropping.
        assert_eq!(
            picture_rect(
                (720.0, 34.0 + 780.0 + NAV_BAR_HEIGHT),
                34.0,
                NAV_BAR_HEIGHT,
                (720, 1560)
            ),
            (180.0, 34.0, 0.5)
        );
    }
}
