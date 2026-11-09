//! Linux evdev inputs translated to the captured Windows KMS wire vocabulary.
use crate::kms_capture::CaptureEvent;
use oplus_connect_daemon::kms::input::InputEvent;
use smallvec::{SmallVec, smallvec};

#[derive(Default)]
pub struct InputMapper {
    motion: [f64; 2],
    scroll: [f64; 2],
}

impl InputMapper {
    pub fn reset(&mut self) {
        self.motion = [0.0; 2];
        self.scroll = [0.0; 2];
    }

    pub fn translate(&mut self, event: &CaptureEvent) -> SmallVec<[InputEvent; 2]> {
        match *event {
            CaptureEvent::Activated { pct_x, pct_y, .. } => {
                self.reset();
                smallvec![InputEvent::Enter { pct_x, pct_y }]
            }
            CaptureEvent::Motion { dx, dy } => {
                let delta_x = accumulate(&mut self.motion[0], dx);
                let delta_y = accumulate(&mut self.motion[1], dy);
                if delta_x == 0 && delta_y == 0 {
                    return SmallVec::new();
                }
                smallvec![InputEvent::Move { delta_x, delta_y }]
            }
            CaptureEvent::Scroll { dx, dy } => {
                // libei positive Y is down; Windows wheel positive Y is up.
                let delta_x = accumulate(&mut self.scroll[0], dx * 120.0);
                let delta_y = accumulate(&mut self.scroll[1], -dy * 120.0);
                if delta_x == 0 && delta_y == 0 {
                    return SmallVec::new();
                }
                smallvec![InputEvent::Scroll { delta_x, delta_y }]
            }
            CaptureEvent::Button { code, pressed } => {
                match code {
                    0x110 => smallvec![InputEvent::Button {
                        button_type: 0,
                        pressed
                    }],
                    0x111 => smallvec![InputEvent::Button {
                        button_type: 1,
                        pressed
                    }],
                    0x112 => smallvec![InputEvent::Button {
                        button_type: 2,
                        pressed
                    }],
                    0x113 | 0x116 => {
                        // Send mouse button 6 AND VK_ESCAPE (Android system Back key)
                        // so both browser history and all Android apps/settings respond to Back.
                        smallvec![
                            InputEvent::Button {
                                button_type: 6,
                                pressed
                            },
                            InputEvent::Key {
                                key_code: 0x1b,
                                modifier: 0,
                                pressed
                            },
                        ]
                    }
                    0x114 | 0x115 => smallvec![InputEvent::Button {
                        button_type: 8,
                        pressed
                    }],
                    other => {
                        tracing::debug!(code = other, pressed, "unmapped mouse button code");
                        SmallVec::new()
                    }
                }
            }
            CaptureEvent::Key { code, pressed } => {
                let Some(key_code) = windows_vk(code) else {
                    return SmallVec::new();
                };
                // Official captures send modifier keys as VK events, modifier=0.
                smallvec![InputEvent::Key {
                    key_code,
                    modifier: 0,
                    pressed,
                }]
            }
            _ => SmallVec::new(),
        }
    }
}

fn accumulate(remainder: &mut f64, delta: f64) -> i32 {
    if !delta.is_finite() {
        return 0;
    }
    let total = (*remainder + delta).clamp(i32::MIN as f64, i32::MAX as f64);
    let integer = total.trunc();
    *remainder = total - integer;
    integer as i32
}

// Physical evdev positions mapped to Windows virtual keys, not Android keycodes.
// Every key, Escape included, belongs to the peer device while the pointer is there.
fn windows_vk(code: u32) -> Option<u8> {
    Some(match code {
        1 => 0x1b,
        2..=10 => b'1' + (code - 2) as u8,
        11 => b'0',
        12 => 0xbd,
        13 => 0xbb,
        14 => 0x08,
        15 => 0x09,
        16 => b'Q',
        17 => b'W',
        18 => b'E',
        19 => b'R',
        20 => b'T',
        21 => b'Y',
        22 => b'U',
        23 => b'I',
        24 => b'O',
        25 => b'P',
        26 => 0xdb,
        27 => 0xdd,
        28 | 96 => 0x0d,
        29 => 0xa2,
        30 => b'A',
        31 => b'S',
        32 => b'D',
        33 => b'F',
        34 => b'G',
        35 => b'H',
        36 => b'J',
        37 => b'K',
        38 => b'L',
        39 => 0xba,
        40 => 0xde,
        41 => 0xc0,
        42 => 0xa0,
        43 => 0xdc,
        44 => b'Z',
        45 => b'X',
        46 => b'C',
        47 => b'V',
        48 => b'B',
        49 => b'N',
        50 => b'M',
        51 => 0xbc,
        52 => 0xbe,
        53 => 0xbf,
        54 => 0xa1,
        55 => 0x6a,
        56 => 0xa4,
        57 => 0x20,
        58 => 0x14,
        59..=68 => 0x70 + (code - 59) as u8,
        69 => 0x90,
        70 => 0x91,
        71 => 0x67,
        72 => 0x68,
        73 => 0x69,
        74 => 0x6d,
        75 => 0x64,
        76 => 0x65,
        77 => 0x66,
        78 => 0x6b,
        79 => 0x61,
        80 => 0x62,
        81 => 0x63,
        82 => 0x60,
        83 => 0x6e,
        87 => 0x7a,
        88 => 0x7b,
        97 => 0xa3,
        98 => 0x6f,
        99 => 0x2c,
        100 => 0xa5,
        102 => 0x24,
        103 => 0x26,
        104 => 0x21,
        105 => 0x25,
        106 => 0x27,
        107 => 0x23,
        108 => 0x28,
        109 => 0x22,
        110 => 0x2d,
        111 => 0x2e,
        119 => 0x13,
        125 => 0x5b,
        126 => 0x5c,
        127 => 0x5d,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_fractional_motion_and_resets_on_new_capture() {
        let mut mapper = InputMapper::default();
        assert!(
            mapper
                .translate(&CaptureEvent::Motion { dx: 0.6, dy: -0.4 })
                .is_empty()
        );
        assert_eq!(
            mapper
                .translate(&CaptureEvent::Motion { dx: 0.6, dy: -0.8 })
                .as_slice(),
            &[InputEvent::Move {
                delta_x: 1,
                delta_y: -1
            }]
        );
        mapper.reset();
        assert!(
            mapper
                .translate(&CaptureEvent::Motion { dx: 0.9, dy: -0.9 })
                .is_empty()
        );
    }

    #[test]
    fn maps_scroll_direction_and_modifiers_to_captured_wire_values() {
        let mut mapper = InputMapper::default();
        assert_eq!(
            mapper
                .translate(&CaptureEvent::Scroll { dx: 0.0, dy: 1.0 })
                .as_slice(),
            &[InputEvent::Scroll {
                delta_x: 0,
                delta_y: -120
            }]
        );
        assert_eq!(
            mapper
                .translate(&CaptureEvent::Key {
                    code: 42,
                    pressed: true
                })
                .as_slice(),
            &[InputEvent::Key {
                key_code: 160,
                modifier: 0,
                pressed: true
            }]
        );
        assert_eq!(
            mapper
                .translate(&CaptureEvent::Key {
                    code: 24,
                    pressed: false
                })
                .as_slice(),
            &[InputEvent::Key {
                key_code: 79,
                modifier: 0,
                pressed: false
            }]
        );
        assert_eq!(
            mapper
                .translate(&CaptureEvent::Key {
                    code: 1,
                    pressed: true
                })
                .as_slice(),
            &[InputEvent::Key {
                key_code: 0x1b,
                modifier: 0,
                pressed: true
            }]
        );
        assert_eq!(
            mapper
                .translate(&CaptureEvent::Key {
                    code: 1,
                    pressed: false
                })
                .as_slice(),
            &[InputEvent::Key {
                key_code: 0x1b,
                modifier: 0,
                pressed: false
            }]
        );
        assert_eq!(
            mapper
                .translate(&CaptureEvent::Button {
                    code: 0x112,
                    pressed: true
                })
                .as_slice(),
            &[InputEvent::Button {
                button_type: 2,
                pressed: true
            }]
        );
        assert_eq!(
            mapper
                .translate(&CaptureEvent::Button {
                    code: 0x113,
                    pressed: true
                })
                .as_slice(),
            &[
                InputEvent::Button {
                    button_type: 6,
                    pressed: true
                },
                InputEvent::Key {
                    key_code: 0x1b,
                    modifier: 0,
                    pressed: true
                },
            ]
        );
        assert_eq!(
            mapper
                .translate(&CaptureEvent::Button {
                    code: 0x114,
                    pressed: true
                })
                .as_slice(),
            &[InputEvent::Button {
                button_type: 8,
                pressed: true
            }]
        );
        assert!(
            mapper
                .translate(&CaptureEvent::Button {
                    code: 0x117,
                    pressed: true
                })
                .is_empty()
        );
    }
}
