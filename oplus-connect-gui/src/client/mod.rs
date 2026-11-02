use crate::state::{DaemonStatus, DeviceInfo, RemoteDevicePayload};
use anyhow::{anyhow, Context, Result};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Clone)]
pub struct DaemonClient {
    pub socket_path: PathBuf,
}

impl Default for DaemonClient {
    fn default() -> Self {
        Self::new()
    }
}

impl DaemonClient {
    pub fn new() -> Self {
        let socket_path = Self::rpc_socket_path();
        Self { socket_path }
    }

    pub fn rpc_socket_path() -> PathBuf {
        if let Some(dir) = std::env::var_os("OPLUS_CONNECT_RUNTIME_DIR") {
            PathBuf::from(dir).join("dsprc-ipc")
        } else if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
            PathBuf::from(dir).join("oppo-connect").join("dsprc-ipc")
        } else {
            PathBuf::from("/tmp/oppo-connect/dsprc-ipc")
        }
    }

    pub fn event_socket_paths() -> Vec<PathBuf> {
        let mut paths = Vec::new();
        if let Some(dir) = std::env::var_os("OPLUS_CONNECT_RUNTIME_DIR") {
            paths.push(PathBuf::from(&dir).join("dsp-ipc-event"));
            paths.push(PathBuf::from(&dir).join("dsprc-ipc-event"));
        } else if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
            let base = PathBuf::from(dir).join("oppo-connect");
            paths.push(base.join("dsp-ipc-event"));
            paths.push(base.join("dsprc-ipc-event"));
        } else {
            paths.push(PathBuf::from("/tmp/oppo-connect/dsp-ipc-event"));
            paths.push(PathBuf::from("/tmp/oppo-connect/dsprc-ipc-event"));
        }
        paths
    }

    pub fn event_socket_path() -> PathBuf {
        Self::event_socket_paths()
            .into_iter()
            .next()
            .unwrap_or_else(|| PathBuf::from("/tmp/oppo-connect/dsp-ipc-event"))
    }

    /// Asks the daemon to dial a previously paired device on the LAN.
    pub async fn connect_device(&self, address: &str, device_id: &str) -> Result<()> {
        let path = format!("/connect_device?ip={address}&device_id={device_id}");
        let body = self.request("POST", &path, None).await?;
        ensure_ok(&body)
    }

    pub async fn disconnect_device(&self, device_id: &str) -> Result<()> {
        let req = serde_json::json!({ "device_id": device_id });
        let body = self
            .request("POST", "/disconnect_device", Some(&req.to_string()))
            .await?;
        ensure_ok(&body)
    }

    pub async fn request(&self, method: &str, path: &str, body: Option<&str>) -> Result<String> {
        let socket_path = self.socket_path.clone();
        let method = method.to_string();
        let path = path.to_string();
        let body_str = body.unwrap_or("").to_string();

        crate::TOKIO_RT.spawn_blocking(move || {
            let mut stream = UnixStream::connect(&socket_path)
                .with_context(|| format!("Cannot connect to UDS socket at {:?}", socket_path))?;

            stream.set_read_timeout(Some(Duration::from_secs(4)))?;
            stream.set_write_timeout(Some(Duration::from_secs(4)))?;

            let req = format!(
                "{} {} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                method,
                path,
                body_str.len(),
                body_str
            );

            stream.write_all(req.as_bytes())?;
            stream.flush()?;

            let mut response_bytes = Vec::new();
            stream.read_to_end(&mut response_bytes)?;
            let response = String::from_utf8(response_bytes)?;

            if let Some(pos) = response.find("\r\n\r\n") {
                let body = &response[pos + 4..];
                Ok(body.to_string())
            } else {
                Ok(response)
            }
        })
        .await?
    }

    pub async fn get_status(&self) -> Result<DaemonStatus> {
        let body = self.request("GET", "/status", None).await?;
        let resp: serde_json::Value = serde_json::from_str(&body)?;
        let data = resp.get("data").ok_or_else(|| anyhow!("No data in /status"))?;
        let status: DaemonStatus = serde_json::from_value(data.clone())?;
        Ok(status)
    }

    pub async fn get_devices(&self) -> Result<Vec<DeviceInfo>> {
        let body = self.request("GET", "/devices", None).await?;
        let resp: serde_json::Value = serde_json::from_str(&body)?;
        let data = resp.get("data").ok_or_else(|| anyhow!("No data in /devices"))?;
        let devices: Vec<DeviceInfo> = serde_json::from_value(
            data.get("devices")
                .cloned()
                .unwrap_or(serde_json::Value::Array(vec![])),
        )?;
        Ok(devices)
    }

    pub async fn get_remote_device(&self) -> Result<Option<RemoteDevicePayload>> {
        let body = self.request("GET", "/get_remote_device", None).await?;
        let resp: serde_json::Value = serde_json::from_str(&body)?;
        if resp.get("code").and_then(|c| c.as_i64()) == Some(0) {
            if let Some(data_str) = resp.get("data").and_then(|d| d.as_str()) {
                let dev: RemoteDevicePayload = serde_json::from_str(data_str)?;
                return Ok(Some(dev));
            }
        }
        Ok(None)
    }

    pub async fn get_qr_info(&self) -> Result<String> {
        let body = self.request("GET", "/get_qr_info", None).await?;
        let resp: serde_json::Value = serde_json::from_str(&body)?;
        if let Some(data_str) = resp.get("data").and_then(|d| d.as_str()) {
            Ok(data_str.to_string())
        } else {
            Ok(body)
        }
    }

    pub async fn accept_connection(&self, device_address: &str, connection_id: &str) -> Result<bool> {
        let req = serde_json::json!({
            "device_address": device_address,
            "connection_id": connection_id,
        });
        let body = self
            .request("POST", "/accept_connection", Some(&req.to_string()))
            .await?;
        let resp: serde_json::Value = serde_json::from_str(&body)?;
        Ok(resp.get("code").and_then(|c| c.as_i64()) == Some(0))
    }

    pub async fn reject_connection(&self, device_address: &str, connection_id: &str) -> Result<bool> {
        let req = serde_json::json!({
            "device_address": device_address,
            "connection_id": connection_id,
        });
        let body = self
            .request("POST", "/reject_connection", Some(&req.to_string()))
            .await?;
        let resp: serde_json::Value = serde_json::from_str(&body)?;
        Ok(resp.get("code").and_then(|c| c.as_i64()) == Some(0))
    }


}

/// IPC routes answer with `{"code":0,"msg":"success"}`; older ones used
/// `status`. A successful request must not surface as a connection failure.
fn ensure_ok(body: &str) -> Result<()> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(body) else {
        return Ok(());
    };
    let code = value
        .get("code")
        .or_else(|| value.get("status"))
        .and_then(|code| code.as_i64());
    if code == Some(0) {
        return Ok(());
    }
    Err(anyhow!(
        "{}",
        value
            .get("msg")
            .and_then(|msg| msg.as_str())
            .unwrap_or("连接请求被拒绝")
    ))
}

#[cfg(test)]
mod tests {
    use super::ensure_ok;

    #[test]
    fn success_response_is_not_an_error() {
        assert!(ensure_ok(r#"{"code":0,"msg":"success"}"#).is_ok());
        assert!(ensure_ok(r#"{"status":0}"#).is_ok());
    }

    #[test]
    fn error_response_carries_the_message() {
        let error = ensure_ok(r#"{"code":400,"msg":"invalid ip"}"#).unwrap_err();
        assert!(error.to_string().contains("invalid ip"));
    }
}
