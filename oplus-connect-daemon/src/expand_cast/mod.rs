//! Oplus_ExpandCast tablet secondary display signaling and control.
pub mod control;

use std::time::Duration;

pub const EXPAND_CAST_TOPIC: &str = "Oplus_ExpandCast";
pub const EXPAND_CAST_PORT: u16 = 7239;
pub const STEP_TIMEOUT: Duration = Duration::from_secs(10);
