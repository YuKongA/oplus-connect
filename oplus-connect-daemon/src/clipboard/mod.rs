use crate::{platform::clipboard, state::AppState};
use tracing::info;

const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(1000);

pub struct ClipboardSync {
    state: AppState,
    last_text: Option<std::sync::Arc<String>>,
}

impl ClipboardSync {
    pub fn new(state: AppState) -> Self {
        Self {
            state,
            last_text: None,
        }
    }

    pub async fn run(mut self) {
        info!("Starting Linux Wayland/X11 clipboard sync loop");
        loop {
            if let Some(mut read) = clipboard::request_read()
                && let Some(text) = read.finish().await
                && self.last_text.as_ref() != Some(&text)
            {
                let is_remote = {
                    let remote = self.state.last_remote_clipboard.read().await;
                    remote.as_ref() == Some(text.as_ref())
                };
                if !is_remote {
                    info!(
                        "Linux clipboard changed (len: {}), notifying frontend",
                        text.len()
                    );
                    let event = serde_json::json!({
                        "event": "clipboard_callback",
                        "type": "text",
                        "source": "local",
                        "content": text.as_str(),
                    });
                    self.state.broadcast_event(&event.to_string());
                }
                self.last_text = Some(text);
            }

            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }
}
