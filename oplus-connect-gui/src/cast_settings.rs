//! Runtime switches for the cast session.
//!
//! Audio and codec preferences are snapshotted when a session starts. Input can
//! change live; the cast window releases held controls before disabling it.

use std::sync::atomic::{AtomicBool, Ordering};

static AUDIO: AtomicBool = AtomicBool::new(true);
static INPUT: AtomicBool = AtomicBool::new(true);
static H265: AtomicBool = AtomicBool::new(true);

pub fn audio() -> bool {
    AUDIO.load(Ordering::Relaxed)
}

pub fn input() -> bool {
    INPUT.load(Ordering::Relaxed)
}

/// H.265 instead of H.264 for the cast video codec.
pub fn h265() -> bool {
    H265.load(Ordering::Relaxed)
}

pub fn set_h265(enabled: bool) {
    H265.store(enabled, Ordering::Relaxed);
}

pub fn set_audio(enabled: bool) {
    AUDIO.store(enabled, Ordering::Relaxed);
}

pub fn set_input(enabled: bool) {
    INPUT.store(enabled, Ordering::Relaxed);
}
