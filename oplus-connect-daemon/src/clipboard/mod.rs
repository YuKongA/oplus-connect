use crate::{platform::clipboard, state::AppState};
use tracing::{info, warn};

const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(1000);

pub struct ClipboardSync {
    state: AppState,
    last_text: Option<String>,
}

impl ClipboardSync {
    pub fn new(state: AppState) -> Self {
        Self {
            state,
            last_text: None,
        }
    }

    pub fn start_sync(self) {
        tokio::spawn(async move {
            self.sync_loop().await;
        });
    }

    async fn sync_loop(mut self) {
        info!("Starting Linux Wayland/X11 clipboard sync loop");
        loop {
            match tokio::task::spawn_blocking(clipboard::read_text).await {
                Ok(Some(text)) => {
                    let changed = self.last_text.as_ref() != Some(&text);
                    if changed {
                        self.last_text = Some(text.clone());
                    }

                    if changed {
                        let is_remote = {
                            let remote = self.state.last_remote_clipboard.read().await;
                            remote.as_ref() == Some(&text)
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
                                "content": text,
                            });
                            self.state.broadcast_event(&event.to_string());
                        }
                    }
                }
                Ok(None) => {}
                Err(error) => warn!("Linux clipboard read task failed: {}", error),
            }

            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }
}
