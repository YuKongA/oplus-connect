use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU8, Ordering};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitDirection {
    #[default]
    Left,
    Right,
}

impl ExitDirection {
    pub fn label(self) -> &'static str {
        match self {
            Self::Left => "向左",
            Self::Right => "向右",
        }
    }

    pub fn from_label(label: &str) -> Option<Self> {
        match label {
            "向左" => Some(Self::Left),
            "向右" => Some(Self::Right),
            _ => None,
        }
    }

    /// Wire `direction` in KMS `arrange` payload:
    /// 1 = Left, 2 = Right (official setCrossEdge=3 bitmask only supports Left|Right)
    pub fn wire_value(self) -> u8 {
        match self {
            Self::Left => 1,
            Self::Right => 2,
        }
    }
}

static DIRECTION: AtomicU8 = AtomicU8::new(0);

pub fn direction() -> ExitDirection {
    match DIRECTION.load(Ordering::Relaxed) {
        1 => ExitDirection::Right,
        _ => ExitDirection::Left,
    }
}

pub fn set_direction(direction: ExitDirection) {
    DIRECTION.store(
        match direction {
            ExitDirection::Left => 0,
            ExitDirection::Right => 1,
        },
        Ordering::Relaxed,
    );
}
