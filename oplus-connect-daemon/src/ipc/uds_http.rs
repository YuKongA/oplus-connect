use crate::ipc::protocol::{ApiResponse, ConnectionDecisionReq};
use crate::ipc::runtime::{BoundSocket, RuntimeDir};
use crate::state::AppState;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::task::JoinSet;
use tokio::time::{Duration, timeout};
use tracing::{info, warn};

#[derive(serde::Deserialize)]
struct FileSendReq {
    filepath: String,
}

#[derive(serde::Deserialize)]
struct DisconnectReq {
    device_id: String,
}

pub struct UdsHttpServer {
    state: AppState,
    socket: BoundSocket,
}

impl UdsHttpServer {
    pub fn bind(
        state: AppState,
        directory: &Arc<RuntimeDir>,
        name: &str,
    ) -> std::io::Result<Self> {
        Ok(Self {
            state,
            socket: directory.bind(name)?,
        })
    }

    pub async fn run(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut clients = JoinSet::new();
        loop {
            tokio::select! {
                accepted = self.socket.listener.accept() => {
                    let (stream, _) = accepted?;
                    let state = self.state.clone();
                    clients.spawn(async move {
                        if let Err(error) = handle_client(stream, state).await {
                            warn!("Client connection error: {}", error);
                        }
                    });
                }
                Some(result) = clients.join_next(), if !clients.is_empty() => {
                    if let Err(error) = result {
                        warn!("HTTP client task failed: {}", error);
                    }
                }
            }
        }
    }
}

const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_BODY_BYTES: usize = 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
struct RequestError {
    status: u16,
    message: &'static str,
}

impl RequestError {
    fn bad_request(message: &'static str) -> Self {
        Self {
            status: 400,
            message,
        }
    }
}

struct HttpRequest {
    text: String,
    body_offset: usize,
}

fn is_token(value: &[u8]) -> bool {
    !value.is_empty()
        && value
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(byte))
}

fn parse_headers(header: &[u8]) -> Result<usize, RequestError> {
    let header = std::str::from_utf8(header)
        .map_err(|_| RequestError::bad_request("Headers must be valid UTF-8"))?;
    let mut lines = header.split("\r\n");
    let mut request_line = lines.next().unwrap_or_default().split(' ');
    let method = request_line.next().unwrap_or_default();
    let target = request_line.next().unwrap_or_default();
    let version = request_line.next().unwrap_or_default();
    if !is_token(method.as_bytes())
        || !target.starts_with('/')
        || target.bytes().any(|byte| byte.is_ascii_control())
        || !matches!(version, "HTTP/1.1" | "HTTP/1.0")
        || request_line.next().is_some()
    {
        return Err(RequestError::bad_request("Malformed HTTP request line"));
    }
    let mut content_length = None;
    let mut transfer_encoding = false;
    for line in lines {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| RequestError::bad_request("Malformed HTTP header"))?;
        if !is_token(name.as_bytes())
            || value
                .bytes()
                .any(|byte| byte.is_ascii_control() && byte != b'\t')
        {
            return Err(RequestError::bad_request("Malformed HTTP header"));
        }
        let value = value.trim_matches([' ', '\t']);
        if name.eq_ignore_ascii_case("content-length") {
            if content_length.is_some()
                || value.is_empty()
                || !value.bytes().all(|byte| byte.is_ascii_digit())
            {
                return Err(RequestError::bad_request(
                    "Invalid or duplicate Content-Length",
                ));
            }
            content_length = Some(
                value
                    .parse::<usize>()
                    .map_err(|_| RequestError::bad_request("Invalid Content-Length"))?,
            );
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            if transfer_encoding
                || !value
                    .split(',')
                    .all(|coding| is_token(coding.trim().as_bytes()))
            {
                return Err(RequestError::bad_request("Invalid Transfer-Encoding"));
            }
            transfer_encoding = true;
        }
    }
    if transfer_encoding {
        if content_length.is_some() {
            return Err(RequestError::bad_request(
                "Transfer-Encoding and Content-Length cannot be combined",
            ));
        }
        return Err(RequestError {
            status: 501,
            message: "Transfer-Encoding is not supported",
        });
    }
    let length = content_length.unwrap_or(0);
    if length > MAX_BODY_BYTES {
        return Err(RequestError {
            status: 413,
            message: "Request body is too large",
        });
    }
    Ok(length)
}

async fn read_request(stream: &mut UnixStream) -> Result<Option<HttpRequest>, RequestError> {
    let mut bytes = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    let body_offset = loop {
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
        if bytes.len() == MAX_HEADER_BYTES {
            return Err(RequestError {
                status: 431,
                message: "Request headers are too large",
            });
        }
        let limit = chunk.len().min(MAX_HEADER_BYTES - bytes.len());
        let count = stream
            .read(&mut chunk[..limit])
            .await
            .map_err(|_| RequestError::bad_request("Failed to read HTTP headers"))?;
        if count == 0 {
            return if bytes.is_empty() {
                Ok(None)
            } else {
                Err(RequestError::bad_request("Incomplete HTTP headers"))
            };
        }
        bytes.extend_from_slice(&chunk[..count]);
    };
    let length = parse_headers(&bytes[..body_offset - 4])?;
    let total = body_offset + length;
    bytes.truncate(total);
    if bytes.len() < total {
        let received = bytes.len();
        bytes.resize(total, 0);
        stream
            .read_exact(&mut bytes[received..])
            .await
            .map_err(|_| RequestError::bad_request("Incomplete HTTP body"))?;
    }
    let text = String::from_utf8(bytes)
        .map_err(|_| RequestError::bad_request("Request body must be valid UTF-8"))?;
    Ok(Some(HttpRequest { text, body_offset }))
}

async fn read_request_with_timeout(
    stream: &mut UnixStream,
    deadline: Duration,
) -> Result<Option<HttpRequest>, RequestError> {
    timeout(deadline, read_request(stream))
        .await
        .unwrap_or(Err(RequestError {
            status: 408,
            message: "Request deadline exceeded",
        }))
}

async fn write_response(
    stream: &mut UnixStream,
    status: u16,
    body: &str,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        408 => "Request Timeout",
        409 => "Conflict",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        501 => "Not Implemented",
        _ => "Internal Server Error",
    };
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    timeout(REQUEST_TIMEOUT, stream.write_all(response.as_bytes())).await??;
    Ok(())
}

async fn handle_client(
    mut stream: UnixStream,
    state: AppState,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let request = match read_request_with_timeout(&mut stream, REQUEST_TIMEOUT).await {
        Ok(Some(request)) => request,
        Ok(None) => return Ok(()),
        Err(error) => {
            let body =
                serde_json::to_string(&ApiResponse::error(i32::from(error.status), error.message))?;
            return write_response(&mut stream, error.status, &body).await;
        }
    };
    let mut parts = request.text.split("\r\n").next().unwrap().split(' ');
    let method = parts.next().unwrap();
    let path_and_query = parts.next().unwrap();
    let path = path_and_query.split('?').next().unwrap_or("/");
    let body = &request.text[request.body_offset..];

    info!("UDS Request: {} {}", method, path);

    let (status_code, json_response) = match (method, path) {
        ("GET", "/get_remote_device") | ("POST", "/get_remote_device") => {
            let active = state.get_active_device().await;
            if let Some(dev) = active {
                // The official renderer expects `{status:0, data:"<json string>"}` where the
                // inner payload carries lan_ip / local_ip as "ip:port" strings.
                let lan_ip = dev.ip_address.clone().unwrap_or_default();
                let local_ip =
                    crate::net_utils::local_ip_for(dev.ip_address.as_deref()).unwrap_or_default();
                let payload = serde_json::json!({
                    "device_id": dev.device_id,
                    "usb_serial": dev.device_id,
                    "device_name": dev.device_name,
                    "device_model": dev.device_model,
                    "phone_model": dev.device_model,
                    "brand": dev.brand,
                    "device_type": "tablet",
                    "os_version": dev.os_version,
                    "battery_level": dev.battery,
                    "battery_charging": dev.is_charging,
                    "connect_type": dev.connect_type,
                    "lan_ip": if lan_ip.is_empty() { String::new() } else { format!("{}:10150", lan_ip) },
                    "local_ip": if local_ip.is_empty() { String::new() } else { format!("{}:0", local_ip) },
                    "device_address": lan_ip,
                    "support_link_change": true,
                });
                let envelope = serde_json::json!({
                    "status": 0,
                    "code": 0,
                    "msg": "success",
                    "data": payload.to_string(),
                });
                (200, envelope.to_string())
            } else {
                let envelope = serde_json::json!({
                    "status": -1,
                    "code": -1,
                    "msg": "No device connected",
                    "data": serde_json::Value::Null,
                });
                (200, envelope.to_string())
            }
        }
        ("GET", "/get_qr_info") | ("POST", "/get_qr_info") => {
            // The renderer builds the pairing QR locally from this payload:
            //   https://pc-assistant-cn.allawnfs.com/h5/index.html?param=<this json>
            // The H5 page only rewrites it into
            //   heycast://synergy/start?param=<same json>
            // and the phone parses that `param` (verified on a ColorOS 17
            // tablet, `[PcUtil] arg2:...`):
            //   * `ConnectionData { lanConnect: LanConnect { .. } }` is what the
            //     pad's QR handler actually deserializes, and
            //   * the flat `addr/ssid/dvd/d_type/name/model/ver/acc` block is the
            //     `QrInfo` the Mac daemon serializes for the same QR.
            // Both are emitted so either consumer accepts the code.
            let dev = state.get_active_device().await;
            let peer_ip = dev.as_ref().and_then(|d| d.ip_address.clone());
            let lan_ip = crate::net_utils::local_ip_for(peer_ip.as_deref()).unwrap_or_default();
            let ssid = crate::net_utils::current_ssid().unwrap_or_default();
            let pc_name = crate::net_utils::hostname();
            let pdid = crate::net_utils::pdid();
            let acc = std::env::var("OPLUS_AD").unwrap_or_else(|_| "456564".to_string());
            const QR_VERSION: i64 = 3;

            let lan_connect = serde_json::json!({
                "address": lan_ip,
                "altAddr": "",
                "deviceId": pdid,
                "deviceType": "6",
                "name": pc_name,
                "modelId": "1",
                "version": QR_VERSION.to_string(),
                "accountHash": acc,
                "ssid": ssid,
                "bm": "",
                "cp": "",
                "pm": "",
                "remoteDvd": "",
            });
            let payload = serde_json::json!({
                "addr": lan_ip,
                "ssid": ssid,
                "dvd": pdid,
                "d_type": 6,
                "name": pc_name,
                "model": "1",
                "ver": QR_VERSION,
                "acc": acc,
                "lanConnect": lan_connect,
            });
            let envelope = serde_json::json!({
                "status": 0,
                "code": 0,
                "msg": "success",
                "data": payload.to_string(),
            });
            info!("get_qr_info -> {}", payload);
            (200, envelope.to_string())
        }
        ("GET", "/accept_connection") | ("POST", "/accept_connection") => {
            match parse_connection_decision(path_and_query, body).and_then(|req| {
                decision_peer_ip(req.device_address).map(|ip| (ip, req.connection_id))
            }) {
                Ok((peer_ip, connection_id)) => {
                    let accepted = crate::probe::accept_peer(&peer_ip, &connection_id).await;
                    if accepted {
                        (200, serde_json::to_string(&ApiResponse::ok()).unwrap())
                    } else {
                        let response = ApiResponse::error(
                            409,
                            "No pending approval for this device and connection ID",
                        );
                        (409, serde_json::to_string(&response).unwrap())
                    }
                }
                Err(message) => {
                    let response = ApiResponse::error(400, message);
                    (400, serde_json::to_string(&response).unwrap())
                }
            }
        }
        ("GET", "/reject_connection") | ("POST", "/reject_connection") => {
            match parse_connection_decision(path_and_query, body).and_then(|req| {
                decision_peer_ip(req.device_address).map(|ip| (ip, req.connection_id))
            }) {
                Ok((peer_ip, connection_id)) => {
                    let rejected = crate::probe::reject_peer(&peer_ip, &connection_id).await;
                    if rejected {
                        let response = ApiResponse::ok();
                        (200, serde_json::to_string(&response).unwrap())
                    } else {
                        let response = ApiResponse::error(
                            409,
                            "No pending approval for this device and connection ID",
                        );
                        (409, serde_json::to_string(&response).unwrap())
                    }
                }
                Err(message) => {
                    let response = ApiResponse::error(400, message);
                    (400, serde_json::to_string(&response).unwrap())
                }
            }
        }
        ("GET", "/send_file") => match parse_file_send_request(path_and_query) {
            Ok(request) => {
                if state.get_active_device().await.is_none() {
                    let response =
                        ApiResponse::error(409, "No active ADB device for file transfer");
                    (409, serde_json::to_string(&response).unwrap())
                } else {
                    let manager = crate::file_share::FileShareManager::new(state.clone());
                    match manager.send_file_to_device(&request.filepath, None).await {
                        Ok(destination) => {
                            let response = ApiResponse::success(serde_json::json!({
                                "destination": destination,
                            }));
                            (200, serde_json::to_string(&response).unwrap())
                        }
                        Err(_) => {
                            let response = ApiResponse::error(500, "File transfer failed");
                            (500, serde_json::to_string(&response).unwrap())
                        }
                    }
                }
            }
            Err(message) => {
                let response = ApiResponse::error(400, message);
                (400, serde_json::to_string(&response).unwrap())
            }
        },
        ("GET", "/devices") => {
            let mut devices: Vec<_> = state
                .devices
                .read()
                .await
                .values()
                .filter(|device| device.is_connected)
                .cloned()
                .collect();
            devices.sort_by(|left, right| left.device_id.cmp(&right.device_id));
            let active_device_id = state
                .get_active_device()
                .await
                .map(|device| device.device_id)
                .filter(|device_id| devices.iter().any(|device| &device.device_id == device_id));
            let response = ApiResponse::success(serde_json::json!({
                "devices": devices,
                "active_device_id": active_device_id,
                "daemon_version": env!("CARGO_PKG_VERSION"),
            }));
            (200, serde_json::to_string(&response).unwrap())
        }
        ("GET", "/status") => {
            let dev_count = state.devices.read().await.len();
            let resp = ApiResponse::success(serde_json::json!({
                "daemon_version": "1.0.0",
                "running": true,
                "active_devices": dev_count,
            }));
            (200, serde_json::to_string(&resp).unwrap())
        }
        ("POST", "/disconnect_device") => match serde_json::from_str::<DisconnectReq>(body) {
            Ok(request) if !request.device_id.trim().is_empty() => {
                let device_id = request.device_id.trim().to_ascii_uppercase();
                let connected = state
                    .devices
                    .read()
                    .await
                    .get(&device_id)
                    .is_some_and(|device| device.is_connected);
                if connected {
                    state.request_disconnect(&device_id).await;
                    (200, serde_json::to_string(&ApiResponse::ok()).unwrap())
                } else {
                    let response = ApiResponse::error(409, "Device is not connected");
                    (409, serde_json::to_string(&response).unwrap())
                }
            }
            _ => {
                let response = ApiResponse::error(400, "Malformed disconnect request");
                (400, serde_json::to_string(&response).unwrap())
            }
        },
        ("GET", "/notifications") => {
            let notifs = state.notifications.read().await.clone();
            let response = ApiResponse::success(serde_json::json!({
                "notifications": notifs,
            }));
            (200, serde_json::to_string(&response).unwrap())
        }
        ("POST", "/notifications/clear") => {
            state.notifications.write().await.clear();
            let response = ApiResponse::ok();
            (200, serde_json::to_string(&response).unwrap())
        }
        ("GET", "/clipboard_history") => {
            let history = state.clipboard_history.read().await.clone();
            let response = ApiResponse::success(serde_json::json!({
                "history": history,
            }));
            (200, serde_json::to_string(&response).unwrap())
        }
        ("POST", "/clipboard/set") => {
            #[derive(serde::Deserialize)]
            struct ClipSetReq {
                content: String,
            }
            match serde_json::from_str::<ClipSetReq>(body) {
                Ok(req) => {
                    let ok = crate::platform::clipboard::write_text(&req.content);
                    if ok {
                        let response = ApiResponse::ok();
                        (200, serde_json::to_string(&response).unwrap())
                    } else {
                        let response = ApiResponse::error(500, "Failed to write clipboard");
                        (500, serde_json::to_string(&response).unwrap())
                    }
                }
                Err(_) => {
                    let response = ApiResponse::error(400, "Malformed clipboard set request");
                    (400, serde_json::to_string(&response).unwrap())
                }
            }
        }
        ("POST", "/clipboard/clear") => {
            state.clipboard_history.write().await.clear();
            let response = ApiResponse::ok();
            (200, serde_json::to_string(&response).unwrap())
        }
        ("POST", "/connect_device") | ("GET", "/connect_device") => {
            let ip = query_param(path_and_query, "ip")
                .map(str::to_string)
                .or_else(|| {
                    serde_json::from_str::<serde_json::Value>(body)
                        .ok()
                        .and_then(|value| value.get("ip").and_then(|ip| ip.as_str()).map(str::to_string))
                });
            let device_id = query_param(path_and_query, "device_id")
                .map(str::to_string)
                .or_else(|| {
                    serde_json::from_str::<serde_json::Value>(body)
                        .ok()
                        .and_then(|value| {
                            value
                                .get("device_id")
                                .and_then(|id| id.as_str())
                                .map(str::to_string)
                        })
                });
            match (ip.and_then(|ip| ip.parse::<std::net::IpAddr>().ok()), device_id) {
                (Some(ip), Some(device_id)) if !device_id.is_empty() => {
                    let state = state.clone();
                    tokio::spawn(async move {
                        if let Err(error) = crate::reverse::connect(ip, device_id, state.clone()).await
                        {
                            warn!("reverse connect failed: {}", error);
                        }
                    });
                    (200, serde_json::to_string(&ApiResponse::ok()).unwrap())
                }
                _ => {
                    let response =
                        ApiResponse::error(400, "connect_device requires a valid ip and device_id");
                    (400, serde_json::to_string(&response).unwrap())
                }
            }
        }
        ("POST", "/create_agent") | ("POST", "/connect_agent") | ("POST", "/unregister_agent") => {
            let response = ApiResponse::error(
                501,
                "Native agent transport is not implemented; no agent operation succeeded",
            );
            (501, serde_json::to_string(&response).unwrap())
        }
        _ => {
            let resp = ApiResponse::error(404, "Not Found");
            (404, serde_json::to_string(&resp).unwrap())
        }
    };

    write_response(&mut stream, status_code, &json_response).await
}
fn parse_connection_decision(
    path_and_query: &str,
    body: &str,
) -> Result<ConnectionDecisionReq, String> {
    let request: ConnectionDecisionReq = if !body.trim().is_empty() {
        serde_json::from_str(body).map_err(|_| {
            "Malformed connection decision JSON (connection_id is required)".to_string()
        })?
    } else {
        let encoded = query_param(path_and_query, "params")
            .ok_or_else(|| "Missing connection decision params".to_string())?;
        let json = decode_base64(encoded)?;
        serde_json::from_slice(&json).map_err(|_| {
            "Malformed connection decision JSON (connection_id is required)".to_string()
        })?
    };
    if request.connection_id.trim().is_empty() {
        return Err("Missing connection_id".to_string());
    }
    Ok(request)
}

fn decision_peer_ip(address: Option<String>) -> Result<String, String> {
    address
        .ok_or_else(|| "Missing device_address".to_string())?
        .parse::<std::net::IpAddr>()
        .map(|ip| ip.to_string())
        .map_err(|_| "device_address must be an IP address".to_string())
}

fn parse_file_send_request(path_and_query: &str) -> Result<FileSendReq, String> {
    let encoded = query_param(path_and_query, "params")
        .ok_or_else(|| "Missing file-send params".to_string())?;
    let json = decode_base64(encoded)?;
    let mut request: FileSendReq =
        serde_json::from_slice(&json).map_err(|_| "Malformed file-send JSON".to_string())?;
    request.filepath = percent_decode(&request.filepath)?;
    Ok(request)
}

fn query_param<'a>(path_and_query: &'a str, name: &str) -> Option<&'a str> {
    path_and_query
        .split_once('?')
        .map(|(_, query)| query)
        .and_then(|query| {
            query.split('&').find_map(|pair| {
                let (key, value) = pair.split_once('=')?;
                (key == name).then_some(value)
            })
        })
}

fn percent_decode(value: &str) -> Result<String, String> {
    let input = value.as_bytes();
    let mut decoded = Vec::with_capacity(input.len());
    let mut index = 0;
    while index < input.len() {
        if input[index] == b'%' {
            if index + 2 >= input.len() {
                return Err("Malformed percent-encoded file path".to_string());
            }
            let high = hex_value(input[index + 1])
                .ok_or_else(|| "Malformed percent-encoded file path".to_string())?;
            let low = hex_value(input[index + 2])
                .ok_or_else(|| "Malformed percent-encoded file path".to_string())?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(input[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).map_err(|_| "File path is not valid UTF-8".to_string())
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn decode_base64(input: &str) -> Result<Vec<u8>, String> {
    if input.is_empty() || input.len() % 4 != 0 {
        return Err("Invalid base64 connection decision params".to_string());
    }

    let chunks = input.as_bytes().chunks_exact(4);
    let mut output = Vec::with_capacity(input.len() / 4 * 3);
    let chunk_count = input.len() / 4;
    for (index, chunk) in chunks.enumerate() {
        let last = index + 1 == chunk_count;
        let padding = match (chunk[2] == b'=', chunk[3] == b'=') {
            (true, true) if last => 2,
            (false, true) if last => 1,
            (false, false) => 0,
            _ => return Err("Invalid base64 connection decision params".to_string()),
        };
        let a = base64_value(chunk[0]).ok_or_else(invalid_base64)?;
        let b = base64_value(chunk[1]).ok_or_else(invalid_base64)?;
        let c = if padding == 2 {
            0
        } else {
            base64_value(chunk[2]).ok_or_else(invalid_base64)?
        };
        let d = if padding > 0 {
            0
        } else {
            base64_value(chunk[3]).ok_or_else(invalid_base64)?
        };
        if (padding == 2 && b & 0x0f != 0) || (padding == 1 && c & 0x03 != 0) {
            return Err("Invalid base64 connection decision params".to_string());
        }

        output.push((((a as u16) << 2) | ((b as u16) >> 4)) as u8);
        if padding < 2 {
            output.push((((b as u16) << 4) | ((c as u16) >> 2)) as u8);
        }
        if padding == 0 {
            output.push((((c as u16) << 6) | d as u16) as u8);
        }
    }
    Ok(output)
}

fn base64_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

fn invalid_base64() -> String {
    "Invalid base64 connection decision params".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    const OFFICIAL_FILE_PARAMS: &str =
        "eyJmaWxlcGF0aCI6IiUyRm9wcG8tZmlsZS10aGF0LWRvZXMtbm90LWV4aXN0IiwicmVzdW1lIjp0cnVlfQ==";

    struct RpcServer {
        directory: Arc<RuntimeDir>,
        state: AppState,
        task: tokio::task::JoinHandle<Result<(), Box<dyn std::error::Error + Send + Sync>>>,
    }

    impl RpcServer {
        fn start() -> Self {
            let directory = RuntimeDir::temporary().unwrap();
            let state = AppState::new();
            let server = UdsHttpServer::bind(state.clone(), &directory, "dsprc-ipc").unwrap();
            Self {
                directory,
                state,
                task: tokio::spawn(server.run()),
            }
        }

        async fn connect(&self) -> UnixStream {
            UnixStream::connect(self.directory.path().join("dsprc-ipc"))
                .await
                .unwrap()
        }

        async fn request(&self, request: &[u8]) -> String {
            let mut client = self.connect().await;
            client.write_all(request).await.unwrap();
            client.shutdown().await.unwrap();
            receive_response(&mut client).await
        }

        async fn decision(&self, action: &str, peer_ip: &str, connection_id: &str) -> String {
            let body =
                serde_json::json!({ "device_address": peer_ip, "connection_id": connection_id })
                    .to_string();
            self.request(
                format!(
                    "POST /{action}_connection HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            )
            .await
        }
    }

    impl Drop for RpcServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn receive_response(client: &mut UnixStream) -> String {
        let mut response = String::new();
        timeout(
            REQUEST_TIMEOUT + Duration::from_secs(2),
            client.read_to_string(&mut response),
        )
        .await
        .unwrap()
        .unwrap();
        response
    }

    fn assert_status(response: &str, status: u16) {
        assert!(
            response.starts_with(&format!("HTTP/1.1 {status} ")),
            "{response}"
        );
    }

    #[test]
    fn parses_official_file_send_path_and_rejects_bad_percent_encoding() {
        let req =
            parse_file_send_request(&format!("/send_file?scope=5&params={OFFICIAL_FILE_PARAMS}"))
                .unwrap();
        assert_eq!(req.filepath, "/oppo-file-that-does-not-exist");
        assert!(percent_decode("%2").is_err());
        assert!(percent_decode("%GG").is_err());
    }

    #[test]
    fn parses_unicode_percent_encoded_file_path() {
        let body = serde_json::json!({
            "filepath": "%2Ftmp%2F%E4%BD%A0%E5%A5%BD.txt",
        })
        .to_string();
        let encoded = base64::engine::general_purpose::STANDARD.encode(body);
        let request = parse_file_send_request(&format!("/send_file?params={encoded}")).unwrap();
        assert_eq!(request.filepath, "/tmp/你好.txt");
    }

    #[test]
    fn approval_query_requires_connection_id_and_valid_peer_address() {
        let body = r#"{"device_address":"192.168.2.111","connection_id":"current-connection"}"#;
        let encoded = base64::engine::general_purpose::STANDARD.encode(body);
        let request =
            parse_connection_decision(&format!("/accept_connection?params={encoded}"), "").unwrap();
        assert_eq!(
            decision_peer_ip(request.device_address).unwrap(),
            "192.168.2.111"
        );
        assert_eq!(request.connection_id, "current-connection");
        for body in [
            r#"{"device_address":"127.0.0.1"}"#,
            r#"{"device_address":"127.0.0.1","connection_id":" "}"#,
        ] {
            assert!(parse_connection_decision("/accept_connection", body).is_err());
        }
        assert!(parse_connection_decision("/accept_connection?params=A===", "").is_err());
        assert!(decision_peer_ip(Some("not-an-ip".to_string())).is_err());
    }

    async fn start_peer_session() -> (
        tokio::net::TcpStream,
        String,
        String,
        tokio::task::JoinHandle<()>,
    ) {
        use crate::scp::codec::{
            CMD_ACTIVE_CONNECTION, SCP_HEADER_LEN, ScpFrame, write_len_delimited,
            write_varint_field,
        };
        use crate::scp::proto::{MSG_HELLO, MSG_SYNC, ScpSyncData, ScpWrapData};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut client = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (server, peer) = listener.accept().await.unwrap();
        let peer_ip = peer.ip().to_string();
        let state = AppState::new();
        let mut events = state.event_sender.subscribe();
        let worker = tokio::spawn(crate::probe::serve_peer(server, 10152, peer, state));
        let mut hello = Vec::new();
        write_varint_field(&mut hello, 1, 1);
        write_len_delimited(&mut hello, 2, b"active_connection");
        write_len_delimited(&mut hello, 3, b"com.heytap.accessory");
        client
            .write_all(
                &ScpFrame::new(
                    CMD_ACTIVE_CONNECTION,
                    ScpWrapData::new(MSG_HELLO, hello).encode(),
                )
                .encode(),
            )
            .await
            .unwrap();
        let sync = ScpSyncData::new("OnePlus 15T", Vec::new(), 8, "PLZ110");
        client
            .write_all(
                &ScpFrame::new(
                    CMD_ACTIVE_CONNECTION,
                    ScpWrapData::new(MSG_SYNC, sync.encode()).encode(),
                )
                .encode(),
            )
            .await
            .unwrap();
        let connection_id = timeout(Duration::from_secs(1), async {
            loop {
                let event: serde_json::Value =
                    serde_json::from_str(&events.recv().await.unwrap()).unwrap();
                if event["payload"]["event"] == "onConnectRequested" {
                    let data = &event["payload"]["data"];
                    assert_eq!(data["device_name"], "OnePlus 15T");
                    assert_eq!(data["device_type"], "phone");
                    break data["connection_id"].as_str().unwrap().to_owned();
                }
            }
        })
        .await
        .unwrap();
        // Hello and Sync each get a reply before the approval decision.
        for _ in 0..2 {
            let mut header = [0u8; SCP_HEADER_LEN];
            timeout(Duration::from_secs(1), client.read_exact(&mut header))
                .await
                .unwrap()
                .unwrap();
            let mut payload = vec![0; usize::from(u16::from_be_bytes([header[2], header[3]]))];
            timeout(Duration::from_secs(1), client.read_exact(&mut payload))
                .await
                .unwrap()
                .unwrap();
        }
        (client, peer_ip, connection_id, worker)
    }

    #[tokio::test]
    async fn reject_rpc_closes_only_the_addressed_pending_session() {
        let _guard = crate::probe::PEER_TEST_LOCK.lock().await;
        let rpc = RpcServer::start();
        let (mut peer, peer_ip, connection_id, worker) = start_peer_session().await;
        assert_status(
            &rpc.decision("reject", &peer_ip, "stale-connection-id")
                .await,
            409,
        );
        assert_status(&rpc.decision("reject", &peer_ip, &connection_id).await, 200);
        let mut byte = [0];
        assert_eq!(
            timeout(Duration::from_secs(1), peer.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        timeout(Duration::from_secs(1), worker)
            .await
            .unwrap()
            .unwrap();
        assert!(!crate::probe::has_peer(&peer_ip).await);
    }

    #[tokio::test]
    async fn approval_rpc_rejects_stale_ids_cross_peer_fallback_and_replay() {
        let _guard = crate::probe::PEER_TEST_LOCK.lock().await;
        let rpc = RpcServer::start();
        let (peer, peer_ip, connection_id, worker) = start_peer_session().await;
        assert_status(
            &rpc.decision("accept", "127.0.0.2", &connection_id).await,
            409,
        );
        assert_status(
            &rpc.decision("accept", &peer_ip, "previous-connection-id")
                .await,
            409,
        );
        assert_status(&rpc.decision("accept", &peer_ip, "").await, 400);
        assert_status(&rpc.decision("accept", &peer_ip, &connection_id).await, 200);
        assert_status(&rpc.decision("accept", &peer_ip, &connection_id).await, 409);
        assert!(crate::probe::has_peer(&peer_ip).await);
        drop(peer);
        timeout(Duration::from_secs(1), worker)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn old_connection_approval_cannot_authorize_reconnected_peer() {
        let _guard = crate::probe::PEER_TEST_LOCK.lock().await;
        let rpc = RpcServer::start();
        let (old_peer, peer_ip, old_id, old_worker) = start_peer_session().await;
        drop(old_peer);
        timeout(Duration::from_secs(1), old_worker)
            .await
            .unwrap()
            .unwrap();
        let (peer, _, connection_id, worker) = start_peer_session().await;
        assert_ne!(old_id, connection_id);
        assert_status(&rpc.decision("accept", &peer_ip, &old_id).await, 409);
        assert_status(&rpc.decision("accept", &peer_ip, &connection_id).await, 200);
        drop(peer);
        timeout(Duration::from_secs(1), worker)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn rc_unix_socket_serves_file_send_and_reports_missing_device() {
        let rpc = RpcServer::start();
        let request = format!(
            "GET /send_file?scope=5&params={OFFICIAL_FILE_PARAMS} HTTP/1.1\r\nHost: localhost\r\n\r\n"
        );
        assert_status(&rpc.request(request.as_bytes()).await, 409);
    }

    #[tokio::test]
    async fn devices_snapshot_keeps_effective_active_after_active_disconnects() {
        let rpc = RpcServer::start();
        let device = |device_id: &str| crate::state::DeviceInfo {
            device_id: device_id.to_owned(),
            device_name: device_id.to_owned(),
            device_model: "model".to_owned(),
            brand: "OPPO".to_owned(),
            os_version: "ColorOS".to_owned(),
            battery: 100,
            is_charging: false,
            connect_type: "usb".to_owned(),
            ip_address: None,
            is_connected: true,
            last_seen: 1,
        };
        rpc.state.upsert_device(device("device-b")).await;
        rpc.state.upsert_device(device("device-a")).await;

        let response = rpc
            .request(b"GET /devices HTTP/1.1\r\nContent-Length: 0\r\n\r\n")
            .await;
        assert_status(&response, 200);
        let body = response.split("\r\n\r\n").nth(1).unwrap();
        let api: ApiResponse<serde_json::Value> = serde_json::from_str(body).unwrap();
        let ids: Vec<_> = api.data.as_ref().unwrap()["devices"]
            .as_array()
            .unwrap()
            .iter()
            .map(|device| device["device_id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["device-a", "device-b"]);

        rpc.state.remove_device("device-b").await;
        let response = rpc
            .request(b"GET /devices HTTP/1.1\r\nContent-Length: 0\r\n\r\n")
            .await;
        let body = response.split("\r\n\r\n").nth(1).unwrap();
        let api: ApiResponse<serde_json::Value> = serde_json::from_str(body).unwrap();
        assert_eq!(api.data.unwrap()["active_device_id"], "device-a");
    }

    #[tokio::test]
    async fn fragmented_headers_and_large_body_are_read_completely() {
        let rpc = RpcServer::start();
        let body = "x".repeat(12 * 1024);
        let header = format!(
            "POST /notifications/clear HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let mut client = rpc.connect().await;
        let mut byte = [0];
        for fragment in [
            &header.as_bytes()[..17],
            &header.as_bytes()[17..header.len() - 1],
            &header.as_bytes()[header.len() - 1..],
            &body.as_bytes()[..8000],
        ] {
            client.write_all(fragment).await.unwrap();
            assert!(
                timeout(Duration::from_millis(10), client.read(&mut byte))
                    .await
                    .is_err(),
                "server replied before the request was complete"
            );
        }
        client.write_all(&body.as_bytes()[8000..]).await.unwrap();
        assert_status(&receive_response(&mut client).await, 200);
    }

    #[tokio::test]
    async fn disconnect_device_requests_session_close() {
        let rpc = RpcServer::start();
        rpc.state
            .upsert_device(crate::state::DeviceInfo {
                device_id: "DEVICE-A".to_owned(),
                device_name: "Phone".to_owned(),
                device_model: "model".to_owned(),
                brand: "OPPO".to_owned(),
                os_version: "ColorOS".to_owned(),
                battery: 100,
                is_charging: false,
                connect_type: "wlan".to_owned(),
                ip_address: None,
                is_connected: true,
                last_seen: 1,
            })
            .await;
        let body = r#"{"device_id":"DEVICE-A"}"#;
        let request = format!(
            "POST /disconnect_device HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        assert_status(&rpc.request(request.as_bytes()).await, 200);
        assert!(rpc.state.take_disconnect_request("DEVICE-A").await);

        let body = r#"{"device_id":"DEVICE-MISSING"}"#;
        let request = format!(
            "POST /disconnect_device HTTP/1.1\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        assert_status(&rpc.request(request.as_bytes()).await, 409);
    }

    #[tokio::test]
    async fn rejects_invalid_framing_and_oversized_requests() {
        let rpc = RpcServer::start();
        for (headers, status) in [
            ("Content-Length: -1\r\n", 400),
            ("Content-Length: 2\r\nContent-Length: 2\r\n", 400),
            ("Content-Length: 2, 3\r\n", 400),
            ("Content-Length: 999999999999999999999999999999\r\n", 400),
            ("Transfer-Encoding:\r\n", 400),
            ("Transfer-Encoding: chunked,\r\n", 400),
            ("Transfer-Encoding: chunked\r\nContent-Length: 2\r\n", 400),
            ("Transfer-Encoding: chunked\r\n", 501),
            ("Transfer-Encoding: gzip\r\n", 501),
            ("Content-Length: 1048577\r\n", 413),
            ("Bad Header: value\r\n", 400),
        ] {
            let request = format!("POST /notifications/clear HTTP/1.1\r\n{headers}\r\n");
            assert_status(&rpc.request(request.as_bytes()).await, status);
        }
        let header = format!(
            "GET /status HTTP/1.1\r\nX-Long: {}",
            "x".repeat(MAX_HEADER_BYTES)
        );
        assert_status(
            &rpc.request(&header.as_bytes()[..MAX_HEADER_BYTES]).await,
            431,
        );
    }

    #[tokio::test]
    async fn incomplete_bodies_and_headers_return_bad_request() {
        let rpc = RpcServer::start();
        for request in [
            b"POST /notifications/clear HTTP/1.1\r\nContent-Length: 20\r\n\r\n{}".as_slice(),
            b"GET /status HTTP/1.1\r\nHost:",
        ] {
            assert_status(&rpc.request(request).await, 400);
        }
    }

    #[tokio::test]
    async fn stalled_body_hits_request_deadline_over_real_socket() {
        let rpc = RpcServer::start();
        let mut client = rpc.connect().await;
        client
            .write_all(b"POST /notifications/clear HTTP/1.1\r\nContent-Length: 20\r\n\r\n{}")
            .await
            .unwrap();
        assert_status(&receive_response(&mut client).await, 408);
    }

    #[tokio::test]
    async fn unimplemented_agent_rpcs_return_not_implemented() {
        let rpc = RpcServer::start();
        for path in ["/create_agent", "/connect_agent", "/unregister_agent"] {
            let request = format!("POST {path} HTTP/1.1\r\nContent-Length: 2\r\n\r\n{{}}");
            assert_status(&rpc.request(request.as_bytes()).await, 501);
        }
    }

    #[tokio::test]
    async fn aborting_http_server_closes_incomplete_clients_and_cleans_socket() {
        let mut rpc = RpcServer::start();
        let mut client = rpc.connect().await;
        client.write_all(b"GET /status HTTP/1.1\r\n").await.unwrap();
        let mut byte = [0];
        assert!(
            timeout(Duration::from_millis(10), client.read(&mut byte))
                .await
                .is_err()
        );
        rpc.task.abort();
        let _ = (&mut rpc.task).await;
        assert_eq!(
            timeout(Duration::from_secs(1), client.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
        assert!(!rpc.directory.path().join("dsprc-ipc").exists());
    }

    #[tokio::test]
    async fn notifications_and_clipboard_history_endpoints() {
        let rpc = RpcServer::start();
        let notif_evt = serde_json::json!({
            "event": "notification_callback",
            "data": {
                "key": "item_1",
                "label": "Test",
                "title": "T1",
                "content": "C1",
                "icon": null,
                "timestamp": 1234,
            }
        });
        rpc.state.broadcast_event(&notif_evt.to_string());
        let clip_evt = serde_json::json!({
            "event": "clipboard_callback",
            "type": "text",
            "source": "remote",
            "content": "test text",
        });
        rpc.state.broadcast_event(&clip_evt.to_string());
        tokio::time::sleep(Duration::from_millis(50)).await;

        let response = rpc.request(b"GET /notifications HTTP/1.1\r\n\r\n").await;
        assert_status(&response, 200);
        let body = response.split("\r\n\r\n").nth(1).unwrap();
        let api: ApiResponse<serde_json::Value> = serde_json::from_str(body).unwrap();
        assert_eq!(api.data.unwrap()["notifications"].as_array().unwrap().len(), 1);

        let response = rpc.request(b"GET /clipboard_history HTTP/1.1\r\n\r\n").await;
        assert_status(&response, 200);
        let body = response.split("\r\n\r\n").nth(1).unwrap();
        let api: ApiResponse<serde_json::Value> = serde_json::from_str(body).unwrap();
        assert_eq!(api.data.unwrap()["history"].as_array().unwrap().len(), 1);
    }
}
