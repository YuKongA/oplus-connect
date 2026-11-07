use crate::state::{
    CastIncoming, CastStatus, DaemonStatus, DeviceInfo, ExpandCastStatus, RemoteDevicePayload,
};
use anyhow::{Context, Result, anyhow};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug)]
pub struct CastRequestRejected(pub String);

impl std::fmt::Display for CastRequestRejected {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for CastRequestRejected {}

#[derive(Debug, serde::Deserialize)]
pub struct KmsStatus {
    pub connection_id: Option<u64>,
    pub status: Option<String>,
}

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
        let data = resp
            .get("data")
            .ok_or_else(|| anyhow!("No data in /status"))?;
        let status: DaemonStatus = serde_json::from_value(data.clone())?;
        Ok(status)
    }

    pub async fn get_devices(&self) -> Result<Vec<DeviceInfo>> {
        let body = self.request("GET", "/devices", None).await?;
        let resp: serde_json::Value = serde_json::from_str(&body)?;
        let data = resp
            .get("data")
            .ok_or_else(|| anyhow!("No data in /devices"))?;
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

    pub async fn accept_connection(
        &self,
        device_address: &str,
        connection_id: &str,
    ) -> Result<bool> {
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

    pub async fn reject_connection(
        &self,
        device_address: &str,
        connection_id: &str,
    ) -> Result<bool> {
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

    /// Start the authenticated cast bus. A successful response only means that
    /// the daemon is listening; callers must wait for connection_ids and media
    /// engine events before reporting a picture.
    pub async fn cast_start(&self, device_id: &str) -> Result<CastStatus> {
        let body = self
            .control_request(
                "POST",
                "/cast/start",
                Some(&serde_json::json!({"device_id": device_id}).to_string()),
            )
            .await?;
        parse_cast_status(&body)
    }

    pub async fn cast_status(&self, device_id: &str) -> Result<CastStatus> {
        let body = self
            .control_request(
                "POST",
                "/cast/status",
                Some(&serde_json::json!({"device_id": device_id}).to_string()),
            )
            .await?;
        parse_cast_status(&body)
    }

    pub async fn cast_stop(&self, device_id: &str) -> Result<()> {
        let body = self
            .control_request(
                "POST",
                "/cast/stop",
                Some(&serde_json::json!({"device_id": device_id}).to_string()),
            )
            .await?;
        ensure_ok(&body)
    }

    pub async fn cast_send(
        &self,
        device_id: &str,
        connection_id: u64,
        payload: &serde_json::Value,
    ) -> Result<()> {
        let body = self
            .control_request(
                "POST",
                "/cast/send",
                Some(
                    &serde_json::json!({
                        "device_id": device_id,
                        "connection_id": connection_id,
                        "payload": payload,
                    })
                    .to_string(),
                ),
            )
            .await?;
        ensure_ok(&body)
    }

    /// Polls one bounded item from a cast connection. A null payload is a
    /// normal no-message result and is not an error.
    pub async fn cast_receive(
        &self,
        device_id: &str,
        connection_id: u64,
    ) -> Result<Option<CastIncoming>> {
        Ok(self
            .cast_receive_batch(device_id, connection_id, 1)
            .await?
            .into_iter()
            .next())
    }

    /// Fetch up to `max` queued messages in one IPC round trip. A peer sends its
    /// SDP answer beside dozens of ICE candidates, so one-per-call polling put
    /// the answer seconds behind them.
    pub async fn cast_receive_batch(
        &self,
        device_id: &str,
        connection_id: u64,
        max: usize,
    ) -> Result<Vec<CastIncoming>> {
        let body = self
            .control_request(
                "POST",
                "/cast/receive",
                Some(
                    &serde_json::json!({
                        "device_id": device_id,
                        "connection_id": connection_id,
                        "max": max,
                    })
                    .to_string(),
                ),
            )
            .await?;
        let response: serde_json::Value =
            serde_json::from_str(&body).with_context(|| "invalid /cast/receive response")?;
        if response.get("code").and_then(|v| v.as_i64()) != Some(0) {
            return Err(anyhow!(
                "{}",
                response
                    .get("msg")
                    .and_then(|v| v.as_str())
                    .unwrap_or("cast receive failed")
            ));
        }
        let payloads = response
            .get("data")
            .and_then(|data| data.get("payloads"))
            .and_then(|value| value.as_array())
            .cloned()
            .unwrap_or_default();
        payloads
            .into_iter()
            .map(|payload| serde_json::from_value(payload).map_err(Into::into))
            .collect()
    }

    pub async fn cast_configure(
        &self,
        device_id: &str,
        connection_id: u64,
        width: u32,
        fps: u32,
    ) -> Result<()> {
        let body = self
            .control_request(
                "POST",
                "/cast/configure",
                Some(
                    &serde_json::json!({
                        "device_id": device_id,
                        "connection_id": connection_id,
                        "width": width,
                        "fps": fps,
                    })
                    .to_string(),
                ),
            )
            .await?;
        ensure_ok(&body)
    }
    pub async fn expand_cast_start(
        &self,
        device_id: &str,
        mode: Option<u8>,
    ) -> Result<ExpandCastStatus> {
        let mut request = serde_json::json!({"device_id": device_id});
        if let Some(mode) = mode {
            request["mode"] = mode.into();
        }
        let body = self
            .control_request("POST", "/expand_cast/start", Some(&request.to_string()))
            .await?;
        parse_expand_cast_status(&body)
    }

    pub async fn expand_cast_status(&self, device_id: &str) -> Result<ExpandCastStatus> {
        let body = self
            .control_request(
                "POST",
                "/expand_cast/status",
                Some(&serde_json::json!({"device_id": device_id}).to_string()),
            )
            .await?;
        parse_expand_cast_status(&body)
    }

    pub async fn expand_cast_stop(&self, device_id: &str) -> Result<()> {
        let body = self
            .control_request(
                "POST",
                "/expand_cast/stop",
                Some(&serde_json::json!({"device_id": device_id}).to_string()),
            )
            .await?;
        ensure_ok(&body)
    }

    pub async fn expand_cast_send(
        &self,
        device_id: &str,
        connection_id: u64,
        payload: &serde_json::Value,
    ) -> Result<()> {
        let body = self
            .control_request(
                "POST",
                "/expand_cast/send",
                Some(
                    &serde_json::json!({
                        "device_id": device_id,
                        "connection_id": connection_id,
                        "payload": payload,
                    })
                    .to_string(),
                ),
            )
            .await?;
        ensure_ok(&body)
    }

    pub async fn expand_cast_receive(
        &self,
        device_id: &str,
        connection_id: u64,
        max: usize,
    ) -> Result<Vec<serde_json::Value>> {
        let body = self
            .control_request(
                "POST",
                "/expand_cast/receive",
                Some(
                    &serde_json::json!({
                        "device_id": device_id,
                        "connection_id": connection_id,
                        "max": max,
                    })
                    .to_string(),
                ),
            )
            .await?;
        let response: serde_json::Value =
            serde_json::from_str(&body).with_context(|| "invalid expand_cast receive response")?;
        if response.get("code").and_then(|v| v.as_i64()) != Some(0) {
            let message = response
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("daemon rejected expand_cast receive");
            return Err(anyhow!("{message}"));
        }
        let messages = response
            .get("data")
            .and_then(|d| d.get("messages"))
            .and_then(|m| m.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(messages)
    }
    pub async fn kms_start(
        &self,
        device_id: &str,
        width: u32,
        height: u32,
        direction: u8,
    ) -> Result<()> {
        self.control_request("POST", "/kms/start", Some(&serde_json::json!({
            "device_id": device_id, "width": width, "height": height, "direction": direction,
        }).to_string())).await?;
        Ok(())
    }

    pub async fn kms_status(&self, device_id: &str) -> Result<KmsStatus> {
        let body = self
            .control_request(
                "POST",
                "/kms/status",
                Some(&serde_json::json!({"device_id": device_id}).to_string()),
            )
            .await?;
        let response: serde_json::Value = serde_json::from_str(&body)?;
        serde_json::from_value(
            response
                .get("data")
                .context("KMS status lacks data")?
                .clone(),
        )
        .context("invalid KMS status")
    }

    pub async fn kms_receive(
        &self,
        device_id: &str,
        connection_id: u64,
    ) -> Result<Option<(f64, f64)>> {
        #[derive(serde::Deserialize)]
        struct CrossBack {
            pct_x: f64,
            pct_y: f64,
        }
        #[derive(serde::Deserialize)]
        struct Data {
            cross_back: Option<CrossBack>,
        }
        let body = self
            .control_request(
                "POST",
                "/kms/receive",
                Some(
                    &serde_json::json!({
                        "device_id": device_id, "connection_id": connection_id,
                    })
                    .to_string(),
                ),
            )
            .await?;
        let response: serde_json::Value = serde_json::from_str(&body)?;
        let data: Data = serde_json::from_value(
            response
                .get("data")
                .context("KMS receive lacks data")?
                .clone(),
        )?;
        match data.cross_back {
            Some(position) if position.pct_x.is_finite() && position.pct_y.is_finite() => {
                Ok(Some((position.pct_x, position.pct_y)))
            }
            Some(_) => Err(anyhow!("invalid KMS return coordinates")),
            None => Ok(None),
        }
    }

    pub async fn kms_input(
        &self,
        device_id: &str,
        connection_id: u64,
        event: &oplus_connect_daemon::kms::input::InputEvent,
    ) -> Result<()> {
        self.control_request(
            "POST",
            "/kms/input",
            Some(
                &serde_json::json!({
                    "device_id": device_id, "connection_id": connection_id, "event": event,
                })
                .to_string(),
            ),
        )
        .await?;
        Ok(())
    }

    pub async fn kms_release(&self, device_id: &str, connection_id: u64) -> Result<()> {
        self.control_request(
            "POST",
            "/kms/release",
            Some(
                &serde_json::json!({"device_id": device_id, "connection_id": connection_id})
                    .to_string(),
            ),
        )
        .await?;
        Ok(())
    }

    pub async fn kms_stop(&self, device_id: &str) -> Result<()> {
        self.control_request(
            "POST",
            "/kms/stop",
            Some(&serde_json::json!({"device_id": device_id}).to_string()),
        )
        .await?;
        Ok(())
    }

    // Cast and KMS workers run on TOKIO_RT. Unlike the legacy UI RPC adapter this
    // socket is dropped on cancellation, and its deadline covers the daemon's
    // 15-second command deadline without occupying a blocking worker.
    async fn control_request(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> Result<String> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        tokio::time::timeout(Duration::from_secs(17), async {
            let mut stream = tokio::net::UnixStream::connect(&self.socket_path).await?;
            let body = body.unwrap_or("");
            let request = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            stream.write_all(request.as_bytes()).await?;
            let mut response = Vec::new();
            stream.take(8 * 1024 * 1024 + 1).read_to_end(&mut response).await?;
            if response.len() > 8 * 1024 * 1024 { return Err(anyhow!("cast IPC response is too large")); }
            let response = String::from_utf8(response)?;
            let (_, body) = response.split_once("\r\n\r\n").ok_or_else(|| anyhow!("invalid cast IPC response"))?;
            // Cast endpoints must return a structured success, never treat a
            // truncated or non-JSON response as a successful send or stop.
            let value: serde_json::Value = serde_json::from_str(body)?;
            let code = value["code"].as_i64().context("cast IPC response lacks a result code")?;
            if code != 0 {
                return Err(CastRequestRejected(value["msg"].as_str().unwrap_or("cast request rejected").to_owned()).into());
            }
            Ok(body.to_owned())
        }).await.context("cast IPC request timed out")?
    }
}

fn parse_cast_status(body: &str) -> Result<CastStatus> {
    let response: serde_json::Value =
        serde_json::from_str(body).with_context(|| "invalid cast status response")?;
    if response.get("code").and_then(|v| v.as_i64()) != Some(0) {
        return Err(anyhow!(
            "{}",
            response
                .get("msg")
                .and_then(|v| v.as_str())
                .unwrap_or("cast request failed")
        ));
    }
    Ok(serde_json::from_value(
        response.get("data").cloned().unwrap_or_default(),
    )?)
}
fn parse_expand_cast_status(body: &str) -> Result<ExpandCastStatus> {
    let response: serde_json::Value =
        serde_json::from_str(body).with_context(|| "invalid expand_cast status response")?;
    if response.get("code").and_then(|v| v.as_i64()) != Some(0) {
        let message = response
            .get("message")
            .or_else(|| response.get("msg"))
            .and_then(|v| v.as_str())
            .unwrap_or("daemon rejected expand_cast request");
        return Err(anyhow!("{message}"));
    }
    let data = response
        .get("data")
        .cloned()
        .ok_or_else(|| anyhow!("missing expand_cast status data"))?;
    serde_json::from_value(data).with_context(|| "malformed expand_cast status payload")
}

/// IPC routes answer with `{"code":0,"msg":"success"}`; older ones used
/// `status`. A successful request must not surface as a connection failure.
fn ensure_ok(body: &str) -> Result<()> {
    let value: serde_json::Value =
        serde_json::from_str(body).context("守护进程返回了无效的确认消息")?;
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
    fn malformed_or_missing_acknowledgements_are_not_success() {
        for body in ["", "not json", "{}", r#"{"msg":"success"}"#] {
            assert!(ensure_ok(body).is_err(), "accepted {body:?}");
        }
    }

    #[test]
    fn error_response_carries_the_message() {
        let error = ensure_ok(r#"{"code":400,"msg":"invalid ip"}"#).unwrap_err();
        assert!(error.to_string().contains("invalid ip"));
    }
}
