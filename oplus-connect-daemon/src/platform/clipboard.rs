//! Platform clipboard adapter used by the protocol core.
//!
//! Linux uses arboard's native backend: Wayland data-control is preferred when
//! available and X11/XWayland is used as the fallback. Windows uses the native
//! Win32 backend exposed by the same crate.

#[cfg(unix)]
fn read_text_impl() -> Option<String> {
    let mut clipboard = arboard::Clipboard::new().ok()?;
    clipboard.get_text().ok()
}

#[cfg(unix)]
fn write_text_impl(text: &str) -> bool {
    let Ok(mut clipboard) = arboard::Clipboard::new() else {
        return false;
    };
    clipboard.set_text(text).is_ok()
}

/// Returns the current clipboard text, or `None` when the backend cannot read
/// text (empty clipboard, unsupported format, or unavailable desktop session).
pub fn read_text() -> Option<String> {
    read_text_impl()
}

/// Replaces the clipboard with `text`. Returns `true` when the backend accepted
/// the write.
pub fn write_text(text: &str) -> bool {
    write_text_impl(text)
}

#[cfg(windows)]
fn read_text_impl() -> Option<String> {
    let mut clipboard = arboard::Clipboard::new().ok()?;
    clipboard.get_text().ok()
}

#[cfg(windows)]
fn write_text_impl(text: &str) -> bool {
    let Ok(mut clipboard) = arboard::Clipboard::new() else {
        return false;
    };
    clipboard.set_text(text).is_ok()
}
