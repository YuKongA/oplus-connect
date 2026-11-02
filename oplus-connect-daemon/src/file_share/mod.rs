use crate::state::AppState;
use std::path::Path;
use tracing::info;

pub struct FileShareManager {
    state: AppState,
}

impl FileShareManager {
    pub fn new(state: AppState) -> Self {
        Self { state }
    }

    pub async fn send_file_to_device(
        &self,
        local_path: &str,
        _serial: Option<String>,
    ) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
        if self.state.get_active_device().await.is_none() {
            return Err("No active device to receive file".into());
        }

        let file_path = Path::new(local_path);
        if !file_path.is_file() {
            return Err("File does not exist or is not a regular file".into());
        }

        let file_name = file_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();

        info!("File transfer requested for: {}", file_name);
        // File sharing uses the Panta channel only; no ADB push fallback.
        Err("Native Panta file transfer channel is not yet established".into())
    }

    pub async fn list_device_downloads(
        &self,
        _serial: Option<String>,
    ) -> Result<Vec<String>, Box<dyn std::error::Error + Send + Sync>> {
        Ok(Vec::new())
    }
}
