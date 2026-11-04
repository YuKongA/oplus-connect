//! Explicit KMS input events sent to the authenticated peer.
//!
//! This module deliberately contains no operating-system input APIs.  Key codes,
//! modifiers, and button numbers are the native protocol values captured from
//! the KMS peer.

use serde::Deserialize;
use std::io;

/// A single input event in the KMS native protocol.
#[derive(Clone, Debug, Deserialize, serde::Serialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InputEvent {
    Move {
        delta_x: i32,
        delta_y: i32,
    },
    Key {
        key_code: u8,
        modifier: u32,
        pressed: bool,
    },
    Button {
        button_type: u8,
        pressed: bool,
    },
    Scroll {
        delta_x: i32,
        delta_y: i32,
    },
    Enter {
        pct_x: f64,
        pct_y: f64,
    },
}

impl InputEvent {
    /// Validate protocol values before putting an event on the worker queue.
    pub fn validate(&self) -> io::Result<()> {
        match self {
            Self::Key { key_code, .. } if *key_code == 0 => {
                Err(invalid("KMS key code must be nonzero"))
            }
            Self::Button { button_type, .. } if !matches!(*button_type, 0 | 1 | 2 | 6 | 8) => {
                Err(invalid("unsupported KMS button type"))
            }
            Self::Enter { pct_x, pct_y } if !pct_x.is_finite() || !pct_y.is_finite() => {
                Err(invalid("KMS enter coordinates must be finite"))
            }
            _ => Ok(()),
        }
    }

    /// Encode the native event body directly into the caller's reusable buffer.
    pub(crate) fn encode_body(&self, output: &mut Vec<u8>) -> io::Result<()> {
        self.validate()?;
        match self {
            Self::Move { delta_x, delta_y } => serde_json::to_writer(
                &mut *output,
                &MoveWire {
                    delta_x: *delta_x,
                    delta_y: *delta_y,
                    platform: 2,
                    kind: 0,
                },
            ),
            Self::Key {
                key_code,
                modifier,
                pressed,
            } => serde_json::to_writer(
                &mut *output,
                &KeyWire {
                    key_code: *key_code,
                    modifier: *modifier,
                    platform: 2,
                    pressed: *pressed,
                    kind: 3,
                },
            ),
            Self::Button {
                button_type,
                pressed,
            } => serde_json::to_writer(
                &mut *output,
                &ButtonWire {
                    button_type: *button_type,
                    click_count: 0,
                    platform: 2,
                    pressed: *pressed,
                    kind: 1,
                },
            ),
            Self::Scroll { delta_x, delta_y } => serde_json::to_writer(
                &mut *output,
                &ScrollWire {
                    delta_x: *delta_x,
                    delta_y: *delta_y,
                    is_continuous: false,
                    momentum_phase: -1,
                    platform: 2,
                    scroll_phase: -1,
                    kind: 5,
                },
            ),
            Self::Enter { pct_x, pct_y } => serde_json::to_writer(
                &mut *output,
                &EnterWire {
                    left_key: 0,
                    pct_x: *pct_x,
                    pct_y: *pct_y,
                    platform: 2,
                    kind: 6,
                },
            ),
        }
        .map_err(|_| invalid("KMS input JSON encoding failed"))
    }

    pub(crate) fn event_type(&self) -> u8 {
        match self {
            Self::Enter { .. } => 2,
            _ => 3,
        }
    }
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct MoveWire {
    delta_x: i32,
    delta_y: i32,
    platform: u8,
    #[serde(rename = "type")]
    kind: u8,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct KeyWire {
    key_code: u8,
    modifier: u32,
    platform: u8,
    pressed: bool,
    #[serde(rename = "type")]
    kind: u8,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ButtonWire {
    button_type: u8,
    click_count: u8,
    platform: u8,
    pressed: bool,
    #[serde(rename = "type")]
    kind: u8,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ScrollWire {
    delta_x: i32,
    delta_y: i32,
    is_continuous: bool,
    momentum_phase: i8,
    platform: u8,
    scroll_phase: i8,
    #[serde(rename = "type")]
    kind: u8,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct EnterWire {
    #[serde(rename = "left_key")]
    left_key: u8,
    pct_x: f64,
    pct_y: f64,
    platform: u8,
    #[serde(rename = "type")]
    kind: u8,
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_wire_shapes_are_typed_and_stable() {
        let mut body = Vec::new();
        InputEvent::Key {
            key_code: 79,
            modifier: 0,
            pressed: true,
        }
        .encode_body(&mut body)
        .unwrap();
        assert_eq!(
            body,
            br#"{"keyCode":79,"modifier":0,"platform":2,"pressed":true,"type":3}"#
        );
        body.clear();
        InputEvent::Scroll {
            delta_x: -2,
            delta_y: 3,
        }
        .encode_body(&mut body)
        .unwrap();
        assert_eq!(body, br#"{"deltaX":-2,"deltaY":3,"isContinuous":false,"momentumPhase":-1,"platform":2,"scrollPhase":-1,"type":5}"#);
    }

    #[test]
    fn validation_rejects_unsafe_values() {
        assert!(
            InputEvent::Key {
                key_code: 0,
                modifier: 0,
                pressed: true
            }
            .validate()
            .is_err()
        );
        for valid in [0, 1, 2, 6, 8] {
            assert!(
                InputEvent::Button {
                    button_type: valid,
                    pressed: true
                }
                .validate()
                .is_ok()
            );
        }
        for invalid_type in [3, 4, 5, 7, 9, 255] {
            assert!(
                InputEvent::Button {
                    button_type: invalid_type,
                    pressed: true
                }
                .validate()
                .is_err()
            );
        }
        assert!(
            InputEvent::Enter {
                pct_x: f64::NAN,
                pct_y: 0.0
            }
            .validate()
            .is_err()
        );
        assert!(
            InputEvent::Key {
                key_code: 1,
                modifier: u32::MAX,
                pressed: true
            }
            .validate()
            .is_ok()
        );
    }
}
