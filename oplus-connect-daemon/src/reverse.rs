//! PC-initiated LAN connection.
//!
//! The phone listens for SCP on `10152`; the desktop becomes the SCP client for
//! this direction. After `interact hello` the phone opens its data channel back
//! to the port advertised in our `Sync.custom_data`, so the existing panta
//! server and device lifecycle handle the rest.

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{info, warn};

use crate::scp::codec::{CMD_ACTIVE_CONNECTION, ScpFrame, write_len_delimited, write_varint_field};
use crate::scp::proto::{
    MSG_HELLO, MSG_SYNC, MSG_SYNC_SECURE, MSG_UKEY2_FINISHED, MSG_UKEY2_INIT, ScpHelloData,
    ScpSyncData, ScpSyncSecureData, ScpWrapData,
};
use crate::state::{AppState, DeviceInfo};

/// Android listens for reverse SCP connections on this port.
pub const REVERSE_SCP_PORT: u16 = 10152;
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(120);
const STEP_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_FRAME_LEN: usize = 1 << 20;

static CONNECTING: AtomicBool = AtomicBool::new(false);

struct ConnectGuard;

impl Drop for ConnectGuard {
    fn drop(&mut self) {
        CONNECTING.store(false, Ordering::SeqCst);
    }
}

/// Starts one PC-initiated LAN connection. Returns after the phone accepted and
/// the data channel had a chance to attach.
pub async fn connect(ip: IpAddr, device_id: String, state: AppState) -> Result<(), String> {
    connect_at(SocketAddr::new(ip, REVERSE_SCP_PORT), device_id, state).await
}

async fn connect_at(addr: SocketAddr, device_id: String, state: AppState) -> Result<(), String> {
    if CONNECTING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Err("已有连接请求正在进行".into());
    }
    let _guard = ConnectGuard;

    if !state.devices.read().await.is_empty() {
        return Err("已有设备连接".into());
    }
    let result = connect_inner(addr, device_id, &state).await;
    if let Err(error) = &result {
        warn!("reverse connect to {} failed: {}", addr, error);
        broadcast(
            &state,
            "connect_failed",
            serde_json::json!({ "reason": error }),
        );
    }
    result
}

async fn connect_inner(
    addr: SocketAddr,
    device_id: String,
    state: &AppState,
) -> Result<(), String> {
    info!("reverse connect: dialling {}", addr);
    let mut sock = tokio::time::timeout(STEP_TIMEOUT, TcpStream::connect(addr))
        .await
        .map_err(|_| "连接手机超时".to_string())?
        .map_err(|error| format!("无法连接手机：{error}"))?;

    // Hello
    let hello = ScpHelloData {
        sequence_no: sequence(),
        service_name: "active_connection".into(),
        namespace: "com.heytap.accessory".into(),
        channel: String::new(),
    };
    send(
        &mut sock,
        ScpFrame::new(
            CMD_ACTIVE_CONNECTION,
            ScpWrapData::new(MSG_HELLO, encode_hello(&hello)).encode(),
        ),
    )
    .await?;
    let reply = read(&mut sock, STEP_TIMEOUT).await?;
    expect_wrap(&reply, MSG_HELLO)?;

    // Sync: advertise the data port the phone should dial back.
    let data_port = std::env::var("OPLUS_DATA_PORT")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(48999);
    let mut custom_data = Vec::new();
    write_varint_field(&mut custom_data, 1, data_port as u64);
    let mut sync = ScpSyncData::new(crate::net_utils::hostname(), custom_data, 6, "000001");
    sync.key_type = 3;
    sync.active_trigger_mode = 1;
    send(
        &mut sock,
        ScpFrame::new(
            CMD_ACTIVE_CONNECTION,
            ScpWrapData::new(MSG_SYNC, sync.encode()).encode(),
        ),
    )
    .await?;
    let reply = read(&mut sock, STEP_TIMEOUT).await?;
    let phone = parse_phone_sync(&reply)?;

    // UKEY2 as initiator. The phone shows its connection confirmation before
    // replying with ServerInit.
    let (mut handshake, client_init) =
        crate::scp::ukey2::ClientHandshake::start().map_err(|error| error.to_string())?;
    send(
        &mut sock,
        ScpFrame::new(
            CMD_ACTIVE_CONNECTION,
            ScpWrapData::new(MSG_UKEY2_INIT, client_init.clone()).encode(),
        ),
    )
    .await?;
    info!("reverse connect: waiting for phone confirmation");
    let reply = read(&mut sock, APPROVAL_TIMEOUT).await?;
    let server_wrap = decode_wrap(&reply).ok_or("手机返回了无法解析的 UKEY2 响应")?;
    if server_wrap.msg_type != MSG_UKEY2_INIT {
        return Err(error_reason(&server_wrap).unwrap_or_else(|| "手机拒绝了连接".into()));
    }
    let client_finished = handshake
        .handle_server_init(&server_wrap.data)
        .map_err(|error| format!("UKEY2 协商失败：{error}"))?;
    send(
        &mut sock,
        ScpFrame::new(
            CMD_ACTIVE_CONNECTION,
            ScpWrapData::new(MSG_UKEY2_FINISHED, client_finished).encode(),
        ),
    )
    .await?;
    let ack = read(&mut sock, STEP_TIMEOUT).await?;
    expect_wrap(&ack, MSG_UKEY2_FINISHED)?;

    let channel = crate::scp::ukey2::SecureChannel::derive(
        handshake.shared_secret().ok_or("UKEY2 未产生共享密钥")?,
        &client_init,
        &server_wrap.data,
        &[0x42; 16],
    )
    .map_err(|error| error.to_string())?;

    // SyncSecure carries the desktop identity and KSC. The phone echoes it.
    let ksc = crate::probe::KSC
        .as_ref()
        .map_err(|error| format!("无法读取 KSC：{error}"))?;
    let secure = ScpSyncSecureData {
        device_id: crate::net_utils::pdid(),
        ksc_alias: ksc.alias.clone(),
        ksc: ksc.key.clone(),
    };
    let body = ScpWrapData::new(MSG_SYNC_SECURE, secure.encode()).encode();
    let mut frame = ScpFrame::new(CMD_ACTIVE_CONNECTION, channel.encrypt(&body).map_err(err)?);
    frame.is_encrypted = true;
    send(&mut sock, frame).await?;
    let reply = read(&mut sock, STEP_TIMEOUT).await?;
    if reply.get(1).map(|byte| byte & 0x08) != Some(0x08) {
        return Err("手机未返回加密的 SyncSecure".into());
    }
    let decrypted = channel.decrypt(&reply[12..]).map_err(err)?;
    let wrap = ScpWrapData::decode(&decrypted).ok_or("SyncSecure 响应无法解析")?;
    if wrap.msg_type != MSG_SYNC_SECURE {
        return Err(error_reason(&wrap).unwrap_or_else(|| "手机拒绝了安全同步".into()));
    }
    // The phone echoes our SyncSecure; the target device id comes from the
    // last paired device stored by the GUI, because Sync does not carry it.
    let peer_secure = ScpSyncSecureData::decode(&wrap.data);
    let channel_ksc = peer_secure
        .as_ref()
        .map(|secure| secure.ksc.clone())
        .filter(|key| key.len() == 16)
        .unwrap_or_else(|| ksc.key.clone());
    // panta authenticates the phone's data channel through this registry.
    crate::probe::remember_session_ksc(&device_id, &channel_ksc).await;

    // Business interaction: the phone may still require user confirmation.
    let mut interaction = ScpFrame::new(
        CMD_ACTIVE_CONNECTION,
        channel.encrypt(b"interact hello").map_err(err)?,
    );
    interaction.is_encrypted = true;
    interaction.is_business = true;
    send(&mut sock, interaction).await?;

    let accepted = read_until_business_accept(&mut sock, &channel).await;
    if let Err(error) = accepted {
        state.remove_device(&device_id).await;
        return Err(error);
    }
    info!("reverse connect: phone accepted the connection");

    // Report "connected" only after the phone user approved the interaction;
    // SyncSecure alone can complete before the confirmation dialog is answered.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let device = DeviceInfo {
        device_id: device_id.clone(),
        device_name: phone.name.clone(),
        device_model: phone.model.clone(),
        device_type: crate::state::device_type_name(phone.device_type).into(),
        brand: String::new(),
        os_version: String::new(),
        battery: 100,
        is_charging: false,
        connect_type: "wifi".into(),
        ip_address: Some(addr.ip().to_string()),
        is_connected: true,
        last_seen: now,
    };
    state.upsert_device(device).await;
    broadcast(
        state,
        "connect_connected",
        serde_json::json!({ "device_id": device_id, "device_name": phone.name }),
    );

    // The SCP handshake socket may stay open or close; the data channel is
    // handled by the panta server and owns the connection lifetime. Drain the
    // socket in the background so this request returns and stops blocking
    // further connect attempts.
    let cleanup_state = state.clone();
    let cleanup_device = device_id.clone();
    tokio::spawn(async move {
        while read_frame(&mut sock).await.is_ok() {}
        cleanup_state
            .schedule_device_removal(&cleanup_device, Duration::from_secs(5))
            .await;
    });
    Ok(())
}

struct PhoneSync {
    name: String,
    model: String,
    device_type: u64,
}

fn parse_phone_sync(frame: &[u8]) -> Result<PhoneSync, String> {
    let wrap = decode_wrap(frame).ok_or("手机 Sync 响应无法解析")?;
    if wrap.msg_type != MSG_SYNC {
        return Err(error_reason(&wrap).unwrap_or_else(|| "手机未返回 Sync".into()));
    }
    let sync = ScpSyncData::decode(&wrap.data).ok_or("手机 Sync 字段无法解析")?;
    Ok(PhoneSync {
        name: sync.device_name,
        model: sync.model_id,
        device_type: sync.device_type,
    })
}

fn encode_hello(hello: &ScpHelloData) -> Vec<u8> {
    let mut out = Vec::new();
    write_varint_field(&mut out, 1, hello.sequence_no as u64);
    write_len_delimited(&mut out, 2, hello.service_name.as_bytes());
    write_len_delimited(&mut out, 3, hello.namespace.as_bytes());
    if !hello.channel.is_empty() {
        write_len_delimited(&mut out, 4, hello.channel.as_bytes());
    }
    out
}

fn decode_wrap(frame: &[u8]) -> Option<ScpWrapData> {
    frame.get(12..).and_then(ScpWrapData::decode)
}

fn expect_wrap(frame: &[u8], expected: i32) -> Result<(), String> {
    let wrap = decode_wrap(frame).ok_or("SCP 响应无法解析")?;
    if wrap.msg_type == expected {
        Ok(())
    } else {
        Err(error_reason(&wrap).unwrap_or_else(|| format!("意外的 SCP 消息：{}", wrap.msg_type)))
    }
}

fn error_reason(wrap: &ScpWrapData) -> Option<String> {
    if wrap.msg_type != 0xFFFF {
        return None;
    }
    let mut bytes = wrap.data.as_slice();
    let mut text = None;
    while let Some(key) = crate::scp::codec::read_varint(&mut bytes) {
        match (key >> 3, key & 7) {
            (3, 2) => {
                let len = crate::scp::codec::read_varint(&mut bytes)? as usize;
                if bytes.len() < len {
                    return None;
                }
                text = Some(String::from_utf8_lossy(&bytes[..len]).to_string());
                bytes = &bytes[len..];
            }
            (_, 0) => {
                crate::scp::codec::read_varint(&mut bytes)?;
            }
            (_, 2) => {
                let len = crate::scp::codec::read_varint(&mut bytes)? as usize;
                if bytes.len() < len {
                    return None;
                }
                bytes = &bytes[len..];
            }
            _ => return None,
        }
    }
    text
}

async fn read_until_business_accept(
    sock: &mut TcpStream,
    channel: &crate::scp::ukey2::SecureChannel,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + STEP_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err("等待手机确认超时".into());
        }
        let frame = read(sock, remaining).await?;
        if frame.get(1).map(|byte| byte & 0x08) != Some(0x08) {
            continue;
        }
        let Ok(plain) = channel.decrypt(&frame[12..]) else {
            continue;
        };
        if frame[1] & 0x01 != 0 {
            if plain == b"accept" {
                return Ok(());
            }
            if plain == b"reject" {
                return Err("手机拒绝了连接".into());
            }
        }
    }
}

async fn send(sock: &mut TcpStream, frame: ScpFrame) -> Result<(), String> {
    sock.write_all(&frame.encode())
        .await
        .map_err(|error| format!("写入手机失败：{error}"))
}

async fn read(sock: &mut TcpStream, timeout: Duration) -> Result<Vec<u8>, String> {
    tokio::time::timeout(timeout, read_frame(sock))
        .await
        .map_err(|_| "等待手机响应超时".to_string())?
}

async fn read_frame(sock: &mut TcpStream) -> Result<Vec<u8>, String> {
    let mut head = [0u8; 12];
    sock.read_exact(&mut head)
        .await
        .map_err(|error| format!("读取手机数据失败：{error}"))?;
    let len = u16::from_be_bytes([head[2], head[3]]) as usize;
    if len > MAX_FRAME_LEN {
        return Err("手机返回了超长 SCP 帧".into());
    }
    let mut payload = vec![0u8; len];
    sock.read_exact(&mut payload)
        .await
        .map_err(|error| format!("读取手机数据失败：{error}"))?;
    let mut frame = head.to_vec();
    frame.extend_from_slice(&payload);
    Ok(frame)
}

fn broadcast(state: &AppState, event: &str, data: serde_json::Value) {
    let payload = serde_json::json!({ "event": event, "data": data });
    state.broadcast_event(&payload.to_string());
}

fn sequence() -> i32 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    nanos as i32
}

fn err(error: impl std::fmt::Display) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sync_identity_from_the_phone() {
        let frame = hex(
            "40340020000000007b08ad120801121c0a0ae4b880e58aa02031355410032008280132063242313438383801",
        );
        let phone = parse_phone_sync(&frame).unwrap();
        assert_eq!(phone.name, "一加 15T");
        assert_eq!(phone.model, "2B1488");
        assert_eq!(crate::state::device_type_name(phone.device_type), "phone");
    }

    #[test]
    fn extracts_phone_error_reason() {
        let wrap = ScpWrapData {
            msg_type: 0xFFFF,
            data: hex(
                "08faacd191011085141a3373796e6320736563757265206d657373616765206661696c65642e205b43616e63656c6c6174696f6e457863657074696f6e5d",
            ),
        };
        assert_eq!(
            error_reason(&wrap).as_deref(),
            Some("sync secure message failed. [CancellationException]")
        );
    }

    #[tokio::test]
    async fn reports_connected_only_after_phone_accepts() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = AppState::new();
        let server_state = state.clone();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
        let (allow_tx, allow_rx) = tokio::sync::oneshot::channel::<()>();

        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();

            // Hello
            let hello = read_test_frame(&mut sock).await;
            assert_eq!(decode_wrap(&hello).unwrap().msg_type, MSG_HELLO);
            let reply = ScpHelloData {
                sequence_no: 1,
                service_name: "active_connection".into(),
                namespace: "com.heytap.accessory".into(),
                channel: String::new(),
            };
            sock.write_all(
                &ScpFrame::new(
                    CMD_ACTIVE_CONNECTION,
                    ScpWrapData::new(MSG_HELLO, encode_hello(&reply)).encode(),
                )
                .encode(),
            )
            .await
            .unwrap();

            // Sync
            let sync = read_test_frame(&mut sock).await;
            assert_eq!(decode_wrap(&sync).unwrap().msg_type, MSG_SYNC);
            let phone_sync = ScpSyncData::new("一加 15T", Vec::new(), 8, "2B1488");
            sock.write_all(
                &ScpFrame::new(
                    CMD_ACTIVE_CONNECTION,
                    ScpWrapData::new(MSG_SYNC, phone_sync.encode()).encode(),
                )
                .encode(),
            )
            .await
            .unwrap();

            // UKEY2
            let (mut server, _) = (crate::scp::ukey2::ServerHandshake::new(), ());
            let init = read_test_frame(&mut sock).await;
            let init_wrap = decode_wrap(&init).unwrap();
            assert_eq!(init_wrap.msg_type, MSG_UKEY2_INIT);
            let server_init = server.handle_client_init(&init_wrap.data).unwrap();
            sock.write_all(
                &ScpFrame::new(
                    CMD_ACTIVE_CONNECTION,
                    ScpWrapData::new(MSG_UKEY2_INIT, server_init).encode(),
                )
                .encode(),
            )
            .await
            .unwrap();
            let finished = read_test_frame(&mut sock).await;
            let finished_wrap = decode_wrap(&finished).unwrap();
            assert_eq!(finished_wrap.msg_type, MSG_UKEY2_FINISHED);
            server.handle_client_finished(&finished_wrap.data).unwrap();
            sock.write_all(
                &ScpFrame::new(
                    CMD_ACTIVE_CONNECTION,
                    ScpWrapData::new(MSG_UKEY2_FINISHED, Vec::new()).encode(),
                )
                .encode(),
            )
            .await
            .unwrap();
            let channel = server.secure_channel().unwrap();

            // SyncSecure: echo the payload back.
            let secure_frame = read_test_frame(&mut sock).await;
            assert!(secure_frame[1] & 0x08 != 0);
            let secure_plain = channel.decrypt(&secure_frame[12..]).unwrap();
            let secure_wrap = ScpWrapData::decode(&secure_plain).unwrap();
            assert_eq!(secure_wrap.msg_type, MSG_SYNC_SECURE);
            let mut reply = ScpFrame::new(
                CMD_ACTIVE_CONNECTION,
                channel.encrypt(&secure_wrap.encode()).unwrap(),
            );
            reply.is_response = true;
            reply.is_encrypted = true;
            sock.write_all(&reply.encode()).await.unwrap();

            // SyncSecure is done, but the phone has not approved yet.
            assert!(server_state.devices.read().await.is_empty());
            ready_tx.send(()).unwrap();
            allow_rx.await.unwrap();

            let mut accept =
                ScpFrame::new(CMD_ACTIVE_CONNECTION, channel.encrypt(b"accept").unwrap());
            accept.is_encrypted = true;
            accept.is_business = true;
            sock.write_all(&accept.encode()).await.unwrap();
            tokio::time::sleep(Duration::from_millis(200)).await;
        });

        let client_state = state.clone();
        let client =
            tokio::spawn(
                async move { connect_at(addr, "C77C702EC021".into(), client_state).await },
            );

        tokio::time::timeout(Duration::from_secs(5), ready_rx)
            .await
            .expect("server reaches SyncSecure")
            .unwrap();
        assert!(
            state.devices.read().await.is_empty(),
            "device must not appear before the phone accepts"
        );

        allow_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(5), client)
            .await
            .expect("reverse connect completes")
            .unwrap()
            .unwrap();
        assert!(state.devices.read().await.contains_key("C77C702EC021"));
        server.await.unwrap();
    }

    async fn read_test_frame(sock: &mut tokio::net::TcpStream) -> Vec<u8> {
        use tokio::io::AsyncReadExt;
        let mut head = [0u8; 12];
        sock.read_exact(&mut head).await.unwrap();
        let len = u16::from_be_bytes([head[2], head[3]]) as usize;
        let mut payload = vec![0u8; len];
        sock.read_exact(&mut payload).await.unwrap();
        let mut frame = head.to_vec();
        frame.extend_from_slice(&payload);
        frame
    }

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
            .collect()
    }
}
