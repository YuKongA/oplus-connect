//! Panta data channel (TCP 46999): EPI offer/answer plus the 10/11/12 KSC
//! challenge-response that authenticates the connection.
//!
//! Everything here is taken from the Windows evidence package
//! (`docs/oppo-connect-protocol.md`, `windows-protocol-evidence/`):
//! the phone dials the PC's TCP 46999 and the PC speaks first with a 109-byte
//! EPI offer. Frames are `u16 big-endian length + body`; the length excludes
//! itself.
//!
//! ```text
//! PC  -> phone  EPI offer      (109 B, body[0] = 0x01)
//! phone -> PC   EPI answer     (104 B, body[0] = 0x02)
//! phone -> PC   10 02 request  ( 38 B, exact length)
//! PC  -> phone  11 01 response ( 27 B, exact length)
//! phone -> PC   12 02 confirm  ( 11 B, exact length)
//! ```
//!
//! MAC = `HMAC-SHA512(KSC, input)[..8]`, where `KSC` is the 16-byte key
//! exchanged in the SCP `SyncSecure` of the *same* connection and the u64s are
//! little-endian even though they travel big-endian.

use std::future::Future;
use std::sync::OnceLock;

use aes_gcm::{
    Aes128Gcm, Nonce,
    aead::{Aead, KeyInit},
};
use getrandom::SysRng;
use hmac::{Hmac, KeyInit as HmacKeyInit, Mac};
use rand_core::{Rng, UnwrapErr};
use sha2::Sha512;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{info, warn};

type HmacSha512 = Hmac<Sha512>;

/// Platform byte stamped into every auth frame we send (`0x01` = Windows/PC;
/// the phone sends `0x02` and neither side validates it).
const PLATFORM_PC: u8 = 0x01;

/// Profiles announced in the `0x0008` service list. The official client also
/// advertises the oshare profile here, before registering its agent later.
pub const SERVICE_PROFILES: [&str; 4] = [
    "system:streamtransfer",
    "system:filetransfer",
    "app:unite.oplus.onet",
    "app:oshare.win.agent",
];

/// The EPI TLV uses the historical value from the official client. It is not
/// the length of the later `0x0008` profile list (which contains four entries).
pub const EPI_SERVICE_PROFILE_COUNT: u8 = 3;

// The phone negotiates these values with the official Windows client. The
// daemon may run on Linux, but it must present the same PC protocol profile.
const PC_DEVICE_NAME: &str = "";
const PC_OS_TYPE: &str = "windows";
const PC_OS_VERSION: &str = "10.0.28000";
const PC_NOTICE_MATERIAL_FLOW: u8 = 0;

pub const MSG_EPI_OFFER: u8 = 0x01;
pub const MSG_EPI_ANSWER: u8 = 0x02;
pub const MSG_AUTH_REQUEST: u8 = 0x10;
pub const MSG_AUTH_RESPONSE: u8 = 0x11;
pub const MSG_AUTH_CONFIRM: u8 = 0x12;

pub const AUTH_REQUEST_LEN: usize = 38;
pub const AUTH_RESPONSE_LEN: usize = 27;
pub const AUTH_CONFIRM_LEN: usize = 11;

/// Allocates a usable OAF transport session.
///
/// The official client assigns a fresh local session for every service
/// connection. The value is not an opcode and must not be copied from an old
/// capture; sessions 0..=4 are reserved for control framing.
pub fn allocate_session_id() -> u16 {
    let mut random = [0u8; 2];
    UnwrapErr(SysRng).fill_bytes(&mut random);
    5 + (u16::from_be_bytes(random) % 1019)
}

/// Encodes the OAF transport header used by data/control frames.
///
/// `control` is zero for ordinary data. Bit 4 of the first byte is the
/// "extended control header follows" flag in the Windows parser.
pub fn encode_transport_header(session_id: u16, control: u8) -> [u8; 2] {
    let session = session_id & 0x03ff;
    [
        ((session >> 6) as u8) | ((control & 1) << 4),
        (((session & 0x3f) as u8) << 2) | ((control >> 1) & 0x03),
    ]
}

/// Decodes an OAF transport header. Returns `None` when the top three version
/// bits are nonzero.
pub fn decode_transport_header(header: [u8; 2]) -> Option<(u16, u8)> {
    if header[0] >> 5 != 0 {
        return None;
    }
    let session = (((header[0] & 0x0f) as u16) << 6) | ((header[1] >> 2) as u16);
    let control = ((header[0] >> 4) & 1) | ((header[1] & 0x03) << 1);
    Some((session, control))
}

fn accepts_business_session(
    session: u16,
    peer_sessions: &[u16],
    local_session: u16,
    local_session_confirmed: bool,
    oshare_session: u16,
    oshare_session_confirmed: bool,
) -> bool {
    session > 4
        && (peer_sessions.contains(&session)
            || (session == local_session && local_session_confirmed)
            || (session == oshare_session && oshare_session_confirmed))
}

fn configured_session(name: &str) -> Option<u16> {
    std::env::var(name)
        .ok()
        .and_then(|value| {
            let value = value.trim();
            if let Some(hex) = value
                .strip_prefix("0x")
                .or_else(|| value.strip_prefix("0X"))
            {
                u16::from_str_radix(hex, 16).ok()
            } else {
                value.parse::<u16>().ok()
            }
        })
        .map(|value| value & 0x03ff)
        .filter(|value| *value > 4)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceConnectionRequest {
    pub initiator_id: u16,
    pub acceptor_id: u16,
    pub profile: String,
    /// `(session_id, channel_id, qos_type, qos_class, reserved)`
    pub infos: Vec<(u16, u16, u8, u8, u8)>,
}

/// Parses OAF message type 1 (create service connection) from the reserved
/// transport session 1 (`00 04` on the wire).
pub fn parse_service_connection_request(body: &[u8]) -> Option<ServiceConnectionRequest> {
    if body.len() < 9 || decode_transport_header([body[0], body[1]])?.0 != 1 || body[2] != 1 {
        return None;
    }
    let profile_end = body[7..].iter().position(|byte| *byte == b';')? + 7;
    let profile = std::str::from_utf8(&body[7..profile_end]).ok()?.to_string();
    let count = u16::from_be_bytes([*body.get(profile_end + 1)?, *body.get(profile_end + 2)?]);
    let mut pos = profile_end + 3;
    let mut infos = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let fields = body.get(pos..pos + 7)?;
        infos.push((
            u16::from_be_bytes([fields[0], fields[1]]),
            u16::from_be_bytes([fields[2], fields[3]]),
            fields[4],
            fields[5],
            fields[6],
        ));
        pos += 7;
    }
    Some(ServiceConnectionRequest {
        acceptor_id: u16::from_be_bytes([body[3], body[4]]),
        initiator_id: u16::from_be_bytes([body[5], body[6]]),
        profile,
        infos,
    })
}

/// Fixed wire sizes for the numeric EPI TLVs; every other tag is
/// `tag, u8 length, bytes`.
fn fixed_tlv_size(tag: u8) -> Option<usize> {
    Some(match tag {
        1 => 4,
        2 | 3 | 4 | 5 => 2,
        10 | 11 | 12 | 13 => 1,
        14 => 6,
        _ => return None,
    })
}

fn put_fixed(out: &mut Vec<u8>, tag: u8, value: &[u8]) {
    debug_assert_eq!(Some(value.len()), fixed_tlv_size(tag));
    out.push(tag);
    out.extend_from_slice(value);
}

fn put_string(out: &mut Vec<u8>, tag: u8, value: &str, limit: usize) {
    let bytes = value.as_bytes();
    let len = bytes.len().min(limit);
    out.push(tag);
    out.push(len as u8);
    out.extend_from_slice(&bytes[..len]);
}

/// `"OAFP_M" + 12 hex chars + 18 random hex chars` (`epComposeOffer` ->
/// `0x1400a7120`; the 6-byte global it mixes in is zero in the shipped build).
pub fn generate_endpoint_id() -> String {
    let mut random = [0u8; 9];
    UnwrapErr(SysRng).fill_bytes(&mut random);
    let mut id = String::from("OAFP_M000000000000");
    for byte in random {
        id.push_str(&format!("{:02x}", byte));
    }
    id
}

/// Official Windows keeps the same `OAFP_M...` endpoint id across reconnects
/// while its service process remains alive. Generate one id per process and
/// reuse it for every EPI offer.
fn process_endpoint_id() -> &'static str {
    static ENDPOINT_ID: OnceLock<String> = OnceLock::new();
    ENDPOINT_ID.get_or_init(generate_endpoint_id)
}

/// The 109-byte offer body. `service_count` is the EPI TLV value and is kept
/// separate from the later `0x0008` profile-list length for compatibility with
/// the official Windows handshake.
pub fn build_offer(hostname: &str, service_count: u8, endpoint_id: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(109);
    out.push(MSG_EPI_OFFER);
    out.extend_from_slice(&0x0002u16.to_be_bytes());
    let mut id = endpoint_id.as_bytes().to_vec();
    id.resize(36, b'0');
    out.extend_from_slice(&id[..36]);
    out.push(0x20); // flags: 0x20 | peer-device-type bit (clear for PC types)
    out.push(13); // TLV count

    put_fixed(&mut out, 1, &0x0010_0000u32.to_be_bytes());
    put_fixed(&mut out, 3, &0x03feu16.to_be_bytes());
    put_fixed(&mut out, 4, &0x2710u16.to_be_bytes());
    put_fixed(&mut out, 2, &0xfffbu16.to_be_bytes());
    put_fixed(&mut out, 5, &0x000au16.to_be_bytes());
    put_fixed(&mut out, 11, &[0x02]);
    put_fixed(&mut out, 13, &[0x01]);
    put_fixed(&mut out, 14, &[0u8; 6]);
    put_string(&mut out, 8, hostname, 0x40);
    put_string(&mut out, 7, "manufa-test", 0x20);
    put_string(&mut out, 6, "product-test", 0x20);
    put_fixed(&mut out, 10, &[0x06]); // device category: PC
    put_fixed(&mut out, 12, &[service_count]);
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EpiAnswer {
    pub endpoint_id: String,
    pub product: String,
    pub manufacturer: String,
    pub friendly_name: String,
    pub device_category: u8,
    pub service_profile_count: u8,
}

/// Parses an EPI answer. Answer length is TLV-driven: official captures include
/// both 104-byte (PLZ110) and 105-byte (OPD2515) bodies.
pub fn parse_answer(body: &[u8]) -> Option<EpiAnswer> {
    if body.len() < 41 || body[0] != MSG_EPI_ANSWER {
        return None;
    }
    let mut answer = EpiAnswer {
        endpoint_id: String::from_utf8_lossy(&body[3..39]).into_owned(),
        product: String::new(),
        manufacturer: String::new(),
        friendly_name: String::new(),
        device_category: 0,
        service_profile_count: 0,
    };
    let count = body[40] as usize;
    let mut cursor = 41;
    for _ in 0..count {
        let tag = *body.get(cursor)?;
        cursor += 1;
        if let Some(size) = fixed_tlv_size(tag) {
            let value = body.get(cursor..cursor + size)?;
            cursor += size;
            match tag {
                10 => answer.device_category = value[0],
                12 => answer.service_profile_count = value[0],
                _ => {}
            }
        } else {
            let len = *body.get(cursor)? as usize;
            cursor += 1;
            let value = body.get(cursor..cursor + len)?;
            cursor += len;
            let text = String::from_utf8_lossy(value).into_owned();
            match tag {
                6 => answer.product = text,
                7 => answer.manufacturer = text,
                8 => answer.friendly_name = text,
                _ => {}
            }
        }
    }
    Some(answer)
}

fn hmac_truncated(ksc: &[u8], input: &[u8]) -> [u8; 8] {
    let mut mac =
        <HmacSha512 as HmacKeyInit>::new_from_slice(ksc).expect("HMAC accepts any key length");
    mac.update(input);
    let digest = mac.finalize().into_bytes();
    let mut out = [0u8; 8];
    out.copy_from_slice(&digest[..8]);
    out
}

/// `10 02`: phone proves it knows the KSC.
pub fn mac_client(ksc: &[u8], client_rc2: &[u8; 8], client_fc: u64) -> [u8; 8] {
    let mut input = Vec::with_capacity(16);
    input.extend_from_slice(client_rc2);
    input.extend_from_slice(&client_fc.to_le_bytes());
    hmac_truncated(ksc, &input)
}

/// `11 01`: PC proves it knows the same KSC and injects its own challenge.
pub fn mac_server(
    ksc: &[u8],
    client_rc2: &[u8; 8],
    server_rc2: &[u8; 8],
    server_time: u64,
) -> [u8; 8] {
    let mut input = Vec::with_capacity(24);
    input.extend_from_slice(client_rc2);
    input.extend_from_slice(server_rc2);
    input.extend_from_slice(&server_time.to_le_bytes());
    hmac_truncated(ksc, &input)
}

/// `12 02`: phone confirms the server challenge. Uses the *request's* `fc`.
pub fn mac_confirm(ksc: &[u8], server_rc2: &[u8; 8], client_fc: u64) -> [u8; 8] {
    let mut input = Vec::with_capacity(16);
    input.extend_from_slice(server_rc2);
    input.extend_from_slice(&client_fc.to_le_bytes());
    hmac_truncated(ksc, &input)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthRequest {
    pub client_rc2: [u8; 8],
    pub client_fc: u64,
    pub mac: [u8; 8],
    pub device_id: [u8; 6],
    pub alias: [u8; 6],
}

pub fn parse_auth_request(body: &[u8]) -> Option<AuthRequest> {
    if body.len() != AUTH_REQUEST_LEN || body[0] != MSG_AUTH_REQUEST {
        return None;
    }
    let mut client_rc2 = [0u8; 8];
    client_rc2.copy_from_slice(&body[2..10]);
    let mut mac = [0u8; 8];
    mac.copy_from_slice(&body[18..26]);
    let mut device_id = [0u8; 6];
    device_id.copy_from_slice(&body[26..32]);
    let mut alias = [0u8; 6];
    alias.copy_from_slice(&body[32..38]);
    Some(AuthRequest {
        client_rc2,
        client_fc: u64::from_be_bytes(body[10..18].try_into().ok()?),
        mac,
        device_id,
        alias,
    })
}

pub fn build_auth_response(server_rc2: &[u8; 8], server_time: u64, mac: &[u8; 8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(AUTH_RESPONSE_LEN);
    out.push(MSG_AUTH_RESPONSE);
    out.push(PLATFORM_PC);
    out.push(0x00);
    out.extend_from_slice(server_rc2);
    out.extend_from_slice(&server_time.to_be_bytes());
    out.extend_from_slice(mac);
    out
}

pub fn parse_auth_confirm(body: &[u8]) -> Option<[u8; 8]> {
    if body.len() != AUTH_CONFIRM_LEN || body[0] != MSG_AUTH_CONFIRM {
        return None;
    }
    let mut mac = [0u8; 8];
    mac.copy_from_slice(&body[3..11]);
    Some(mac)
}

/// Eight ASCII digits, matching `cryptoGenRandomDigitalStr`.
pub fn random_digital_string() -> [u8; 8] {
    let mut random = [0u8; 8];
    UnwrapErr(SysRng).fill_bytes(&mut random);
    let mut out = [0u8; 8];
    for (slot, byte) in out.iter_mut().zip(random) {
        *slot = b'0' + (byte % 10);
    }
    out
}

pub fn unix_time() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// DBR's eight-byte delivery sequence. It is seeded from wall-clock seconds,
/// but every frame must advance it: reusing a value makes the receiver treat
/// the frame as a duplicate and silently discard it.
static DBR_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn next_dbr_sequence() -> u64 {
    let mut previous = DBR_SEQUENCE.load(Ordering::Relaxed);
    loop {
        let next = previous.saturating_add(1).max(unix_time());
        match DBR_SEQUENCE.compare_exchange_weak(
            previous,
            next,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return next,
            Err(actual) => previous = actual,
        }
    }
}

/// Service/profile list (`0x0008` sub-type `0x01`), with semicolon-delimited names.
pub fn build_service_list(profiles: &[&str]) -> Vec<u8> {
    let joined: String = profiles.iter().map(|name| format!("{};", name)).collect();
    let mut out = Vec::with_capacity(9 + joined.len());
    out.extend_from_slice(&[0x00, 0x08, 0x01, 0x02, 0x00, 0x00, 0x00, 0x00]);
    out.push(profiles.len() as u8);
    out.extend_from_slice(joined.as_bytes());
    out
}

/// PC-side agent registration (`0x0008` sub `0x02`, 299 B). The shape comes
/// from the official Windows capture; the handles are our local namespace and
/// must stay in sync with the constants used by `0x0004`.
const PROFILE_REGISTRATION: &str = "0008020200000000000453747265616d5472616e73666572536572766963653b3b0002678473797374656d3a73747265616d7472616e736665723b010001000a002973797374656d3a73747265616d7472616e736665723b010000000a46696c655472616e73666572536572766963653b3b00024ae173797374656d3a66696c657472616e736665723b010001000a482373797374656d3a66696c657472616e736665723b010000000a46696c655472616e7366657252656365697665723b3b00027c886170703a756e6974652e6f706c75732e6f6e65743b010001000a18be6170703a756e6974652e6f706c75732e6f6e65743b010000000a57696e4368616e6e656c536572766963653b3b00011e986170703a6f73686172652e77696e2e6167656e743b010001000a";

/// Profile selection (`0x0008` sub `0x01`, 30 B): the single profile the PC
/// serves out of the phone's list.
const PROFILE_SELECTION: &str = "0008010200000000016170703a756e6974652e6f706c75732e6f6e65743b";

/// Profile bind (`0x0008` sub `0x03`, 63 B) sent right after the selection.
const PROFILE_BIND: &str = "000803020000000000010146696c655472616e7366657252656365697665723b3b00017c886170703a756e6974652e6f706c75732e6f6e65743b010001000a";

fn hex_to_vec(text: &str) -> Vec<u8> {
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
        .collect()
}

pub fn profile_registration() -> Vec<u8> {
    hex_to_vec(PROFILE_REGISTRATION)
}

pub fn profile_selection() -> Vec<u8> {
    hex_to_vec(PROFILE_SELECTION)
}

pub fn profile_bind() -> Vec<u8> {
    hex_to_vec(PROFILE_BIND)
}

/// `0x0010` sub-type `0x01`: `[type(1) name ';'] * count`.
pub fn parse_multichannel_request(body: &[u8]) -> Option<Vec<(u8, String)>> {
    if body.len() < 4 || body[0] != 0x00 || body[1] != 0x10 || body[2] != 0x01 {
        return None;
    }
    let count = body[3] as usize;
    let mut cursor = 4;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        let channel_type = *body.get(cursor)?;
        cursor += 1;
        let end = body[cursor..].iter().position(|b| *b == b';')? + cursor;
        let name = String::from_utf8_lossy(&body[cursor..end]).into_owned();
        cursor = end + 1;
        entries.push((channel_type, name));
    }
    Some(entries)
}

/// `0x0010` sub-type `0x02`: `[type(1) status(1) "port;" ] * count`.
pub fn build_multichannel_reply(entries: &[(u8, u8, u16)]) -> Vec<u8> {
    let mut out = vec![0x00, 0x10, 0x02, entries.len() as u8];
    for (channel_type, status, port) in entries {
        out.push(*channel_type);
        out.push(*status);
        out.extend_from_slice(format!("{};", port).as_bytes());
    }
    out
}

/// Binds a listener for a negotiated channel; the official client's phone never
/// dials these, so accepted sockets are only logged.
async fn open_channel_listener(channel_type: u8, name: String) -> std::io::Result<u16> {
    let listener = tokio::net::TcpListener::bind("0.0.0.0:0").await?;
    let port = listener.local_addr()?.port();
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((mut socket, peer)) => {
                    info!(
                        "panta: channel {} ({}) accepted connection from {}",
                        channel_type, name, peer
                    );
                    tokio::spawn(async move {
                        // Hold the channel open and drain it: dropping the socket
                        // right away resets the stream, which makes the phone abort
                        // the whole session.
                        let mut buf = vec![0u8; 8192];
                        loop {
                            match socket.read(&mut buf).await {
                                Ok(0) => {
                                    info!("panta: channel {} peer closed", channel_type);
                                    return;
                                }
                                Ok(n) => info!(
                                    "panta: channel {} received {} bytes: {}",
                                    channel_type,
                                    n,
                                    hex(&buf[..n])
                                ),
                                Err(error) => {
                                    warn!("panta: channel {} read failed: {}", channel_type, error);
                                    return;
                                }
                            }
                        }
                    });
                }
                Err(error) => {
                    warn!("panta: channel {} accept failed: {}", channel_type, error);
                    return;
                }
            }
        }
    });
    Ok(port)
}

/// Same framing with an explicit OAF transport session ID.
pub fn build_data_bus_with(
    session_id: u16,
    our_id: [u8; 6],
    peer_id: [u8; 6],
    route_head: u8,
    json: &str,
) -> Vec<u8> {
    // DBR uses two routing-block variants. DATA/BROADCAST messages (`0x2a`
    // topic sync, `0x52` topic data) carry an extra `ff000000` route word;
    // control messages (`0x03` HELLO, `0x51` screen/heartbeat) do not.
    const ROUTE_DATA: [u8; 32] = [
        0x2a, 0x05, 0x00, 0x00, 0xff, 0x00, 0x00, 0x00, 0xff, 0x00, 0x00, 0x00, 0xff, 0x00, 0x00,
        0x00, 0xff, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00,
    ];
    const ROUTE_CONTROL: [u8; 32] = [
        0x03, 0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xff, 0x00, 0x00, 0x00, 0xff, 0x00, 0x00,
        0x00, 0xff, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x00,
    ];
    let mut route = if matches!(route_head, 0x2a | 0x52) {
        ROUTE_DATA
    } else {
        ROUTE_CONTROL
    };
    route[0] = route_head;
    route[22..28].copy_from_slice(&peer_id);

    let mut out = Vec::with_capacity(61 + json.len());
    out.extend_from_slice(&encode_transport_header(session_id, 0));
    out.push(0x00);
    out.extend_from_slice(&our_id);
    out.extend_from_slice(&next_dbr_sequence().to_be_bytes());
    out.extend_from_slice(&our_id);
    out.extend_from_slice(&peer_id);
    out.extend_from_slice(&route);
    out.extend_from_slice(json.as_bytes());
    out
}

/// Windows publishes these notification-control messages after topic sync;
/// their order activates the phone's notification publisher for this peer.
fn notification_startup_messages() -> [String; 3] {
    [
        topic_data_json(
            "Oplus_HeySynergy_PcConnect_SyncNotification_Subscriber",
            r#"{"key":"HeySynergy_Notification_Delete","value":""}"#,
        ),
        topic_data_json(
            "Oplus_HeySynergy_PcConnect_SyncNotification_Subscriber",
            r#"{"key":"HeySynergy_Notification_Add","value":""}"#,
        ),
        topic_data_json(
            "Oplus_HeySynergy_PcConnect_SyncNotification_Subscriber",
            r#"{"key":"HeySynergy_Notification_Error","value":""}"#,
        ),
    ]
}

/// Completes the DBR data-bus handshake after the phone's HELLO
/// (`mPayLoadType=1`). Sending these frames earlier is silently dropped.
async fn send_data_bus_handshake(
    sock: &mut TcpStream,
    session_id: u16,
    our_id: [u8; 6],
    peer_id: [u8; 6],
) -> std::io::Result<()> {
    let our_hex = crate::net_utils::pdid();
    let frames = [
        (0x03, link_info_json(&our_hex, pc_device_name())),
        (0x2a, OFFICIAL_WINDOWS_TOPIC_SYNC_JSON.to_string()),
        (0x51, SCREEN_STATUS_JSON.to_string()),
        (
            0x52,
            topic_data_json(
                "Oplus_MacConnect",
                "{\"key\":\"MacClientVersion\",\"value\":\"17.20.3\"}",
            ),
        ),
        (
            0x52,
            topic_data_json(
                "Oplus_MacConnect",
                "{\"key\":\"Notice_CrossDevice\",\"type\":20,\"value\":\"1\"}",
            ),
        ),
        (
            0x52,
            topic_data_json(
                "Oplus_MacConnect",
                &format!("{{\"key\":\"pcAbility\",\"value\":{}}}", pc_ability_json()),
            ),
        ),
        (
            0x52,
            topic_data_json(
                "Oplus_MacConnect",
                "{\"key\":\"clipboard_state_first_time\",\"value\":\"{\\\"crossdeviceClipboardVersion\\\":\\\"1200\\\",\\\"source\\\":\\\"windows\\\",\\\"state\\\":1}\"}",
            ),
        ),
        (
            0x52,
            topic_data_json(
                "Oplus_MacConnect",
                "{\"key\":\"clipboard_state_active\",\"type\":20,\"value\":\"{\\\"crossdeviceClipboardVersion\\\":\\\"1200\\\",\\\"state\\\":1,\\\"source\\\":\\\"windows\\\"}\"}",
            ),
        ),
    ];

    for (route_head, json) in frames {
        let frame = build_data_bus_with(session_id, our_id, peer_id, route_head, &json);
        write_frame(sock, &frame).await?;
    }
    for json in notification_startup_messages() {
        let frame = build_data_bus_with(session_id, our_id, peer_id, 0x52, &json);
        write_frame(sock, &frame).await?;
    }
    Ok(())
}

/// Link-info payload the official PC sends right after the agent exchange
/// (same keys/indentation as the capture, device ids filled in for us).
pub fn link_info_json(our_hex: &str, device_name: &str) -> String {
    let our_hex = our_hex.to_ascii_uppercase();
    format!(
        "{{\n\t\"mPayLoad\":\t{{\n\t\t\"mDeviceIdBondMap\":\t{{\n\t\t\t\"1\":\t\"{our_hex}\"\n\t\t}},\n\t\t\"mDeviceName\":\t\"{device_name}\",\n\t\t\"mLinkCost\":\t2,\n\t\t\"mLinkType\":\t16,\n\t\t\"mMprMode\":\t0,\n\t\t\"mOnetId\":\t\"{our_hex}\",\n\t\t\"mProtocol\":\t1\n\t}},\n\t\"mPayLoadType\":\t1\n}}"
    )
}

/// Exact topic registration observed in the official Windows notification
/// session. Other topics are registered by separate official agents.
pub const OFFICIAL_WINDOWS_TOPIC_SYNC_JSON: &str = r#"{
    "dbs_msg_to_device":"",
    "dbs_msg_priority":1,
    "dbs_msg_type":"msg_topic_sync",
    "dbs_msg_encrypt":true,
    "dbs_msg_payload":{
        "pub":[
            {"dbs_topic_name":"Oplus_HeySynergy_PcConnect_SyncNotification_Subscriber","dbs_topic_tag":0},
            {"dbs_topic_name":"Oplus_MacConnect","dbs_topic_tag":0}
        ],
        "sub":[
            {"dbs_topic_name":"Oplus_HeySynergy_PcConnect_SyncNotification_Publisher","dbs_topic_tag":0},
            {"dbs_topic_name":"Oplus_MacConnect","dbs_topic_tag":0}
        ]
    }
}"#;

/// Topic sync replaces the entire set, so each authenticated bus owns both leases.
#[derive(Default)]
struct BusTopics {
    kms: bool,
    cast: bool,
}

impl BusTopics {
    fn sync_json(&self) -> String {
        let mut value: serde_json::Value =
            serde_json::from_str(OFFICIAL_WINDOWS_TOPIC_SYNC_JSON).expect("valid base topic sync");
        for role in ["pub", "sub"] {
            let topics = value["dbs_msg_payload"][role]
                .as_array_mut()
                .expect("topic array");
            for (enabled, name) in [(self.kms, "Oplus_KeyMouseLink"), (self.cast, "Oplus_Cast")] {
                if enabled {
                    topics.push(serde_json::json!({"dbs_topic_name":name,"dbs_topic_tag":0}));
                }
            }
        }
        value.to_string()
    }
}

/// Screen-status data message the official PC sends on the data bus; phones
/// appear to start syncing once they have seen one.
pub const SCREEN_STATUS_JSON: &str = r#"{"data_sync_msg_encrypt":false,"data_sync_msg_id":"","data_sync_msg_module":15001,"data_sync_msg_payload":"{\"device_screen_status\":true}","data_sync_msg_priority":-1,"data_sync_msg_type":"15001"}"#;

/// `0x0004 sub=0x01` create-service request: agent ids, profile, and the
/// session info list that tells the peer which OAF session we will use.
pub fn create_service_request(
    acceptor: u16,
    initiator: u16,
    profile: &str,
    session_id: u16,
    channel_id: u16,
) -> Vec<u8> {
    let mut out = vec![0x00, 0x04, 0x01];
    out.extend_from_slice(&acceptor.to_be_bytes());
    out.extend_from_slice(&initiator.to_be_bytes());
    out.extend_from_slice(profile.as_bytes());
    out.push(b';');
    out.extend_from_slice(&1u16.to_be_bytes());
    out.extend_from_slice(&session_id.to_be_bytes());
    out.extend_from_slice(&channel_id.to_be_bytes());
    out.extend_from_slice(&[0x00, 0x01, 0x00]); // qosType, qosClass, reserved
    out
}

/// `0x0004 sub=0x05` bind/ready frame. It has the same agent/profile fields as
/// sub=01 but no session info list.
pub fn create_service_bind(acceptor: u16, initiator: u16, profile: &str) -> Vec<u8> {
    let mut out = vec![0x00, 0x04, 0x05];
    out.extend_from_slice(&acceptor.to_be_bytes());
    out.extend_from_slice(&initiator.to_be_bytes());
    out.extend_from_slice(profile.as_bytes());
    out.push(b';');
    out
}

/// `0x0004 sub=0x02` confirm: echoes the peer's agent ids and **its** session id
/// (protocol doc §17.4; the official confirm carries the requester's session).
pub fn create_service_confirm(
    initiator: u16,
    acceptor: u16,
    profile: &str,
    peer_session: u16,
) -> Vec<u8> {
    let mut out = vec![0x00, 0x04, 0x02];
    out.extend_from_slice(&initiator.to_be_bytes());
    out.extend_from_slice(&acceptor.to_be_bytes());
    out.extend_from_slice(profile.as_bytes());
    out.push(b';');
    out.extend_from_slice(&0u16.to_be_bytes());
    out.push(0x01);
    out.extend_from_slice(&peer_session.to_be_bytes());
    out
}

/// `0x0004 sub=0x06` public identity frame. The final u16 is the DER length;
/// `key_kind` is `0x0002` for `app:oshare.win.agent` and `0x0001` for
/// `app:unite.oplus.onet` in the official captures.
pub fn create_service_certificate(
    local_agent: u16,
    peer_agent: u16,
    profile: &str,
    key_kind: u16,
    der: &[u8],
) -> Vec<u8> {
    let mut out = vec![0x00, 0x04, 0x06];
    out.extend_from_slice(&local_agent.to_be_bytes());
    out.extend_from_slice(&peer_agent.to_be_bytes());
    out.extend_from_slice(profile.as_bytes());
    out.push(b';');
    out.extend_from_slice(&key_kind.to_be_bytes());
    out.extend_from_slice(&(der.len() as u16).to_be_bytes());
    out.extend_from_slice(der);
    out
}

fn service_frame_profile(body: &[u8]) -> Option<&str> {
    if body.len() < 8 || body[0] != 0x00 || body[1] != 0x04 {
        return None;
    }
    let semi = body[7..].iter().position(|byte| *byte == b';')? + 7;
    std::str::from_utf8(&body[7..semi]).ok()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateServiceRequest {
    pub initiator: u16,
    pub acceptor: u16,
    pub profile: String,
    pub sessions: Vec<(u16, u16)>,
}

pub fn parse_create_service_request(body: &[u8]) -> Option<CreateServiceRequest> {
    if body.len() < 8 || body[0] != 0x00 || body[1] != 0x04 || body[2] != 0x01 {
        return None;
    }
    let acceptor = u16::from_be_bytes([body[3], body[4]]);
    let initiator = u16::from_be_bytes([body[5], body[6]]);
    let semi = body[7..].iter().position(|byte| *byte == b';')? + 7;
    let profile = String::from_utf8_lossy(&body[7..semi]).into_owned();
    let mut cursor = semi + 1;
    let count = u16::from_be_bytes([*body.get(cursor)?, *body.get(cursor + 1)?]) as usize;
    cursor += 2;
    let mut sessions = Vec::with_capacity(count);
    for _ in 0..count {
        let session = u16::from_be_bytes([*body.get(cursor)?, *body.get(cursor + 1)?]);
        let channel = u16::from_be_bytes([*body.get(cursor + 2)?, *body.get(cursor + 3)?]);
        cursor += 7; // session, channel, qosType, qosClass, reserved
        sessions.push((session, channel));
    }
    Some(CreateServiceRequest {
        acceptor,
        initiator,
        profile,
        sessions,
    })
}

/// Fields of a peer's `0x0004 sub=0x02` acceptance response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateServiceConfirm {
    pub acceptor: u16,
    pub initiator: u16,
    pub profile: String,
    pub status: u8,
    pub sessions: Vec<u16>,
}

pub fn parse_create_service_confirm(body: &[u8]) -> Option<CreateServiceConfirm> {
    if body.len() < 11 || body[0] != 0x00 || body[1] != 0x04 || body[2] != 0x02 {
        return None;
    }
    let acceptor = u16::from_be_bytes([body[3], body[4]]);
    let initiator = u16::from_be_bytes([body[5], body[6]]);
    let semi = body[7..].iter().position(|byte| *byte == b';')? + 7;
    let profile = String::from_utf8_lossy(&body[7..semi]).into_owned();
    let status = *body.get(semi + 1)?;
    let count = u16::from_be_bytes([*body.get(semi + 2)?, *body.get(semi + 3)?]) as usize;
    let mut cursor = semi + 4;
    let mut sessions = Vec::with_capacity(count);
    for _ in 0..count {
        let session = u16::from_be_bytes([*body.get(cursor)?, *body.get(cursor + 1)?]);
        cursor += 2;
        sessions.push(session);
    }
    Some(CreateServiceConfirm {
        acceptor,
        initiator,
        profile,
        status,
        sessions,
    })
}

/// Finds the two-byte agent handle immediately preceding a profile name in a
/// `0x0008 sub=0x02` registration. The registration repeats some profile names
/// for different roles; callers that need the peer's acceptor use the first
/// occurrence, matching the official phone registration.
pub fn agent_for_profile(registration: &[u8], profile: &str) -> Option<u16> {
    let needle = format!("{profile};");
    let position = registration
        .windows(needle.len())
        .position(|window| window == needle.as_bytes())?;
    if position < 2 {
        return None;
    }
    Some(u16::from_be_bytes([
        registration[position - 2],
        registration[position - 1],
    ]))
}

/// Business-frame key material: `HMAC-SHA512(KSC, client_rc2 || server_rc2 || seq)`
/// split into AES-128 key, GCM IV and AAD (protocol doc §7.3).
pub fn business_key(
    ksc: Option<&[u8]>,
    client_rc2: &[u8; 8],
    server_rc2: &[u8; 8],
    sequence: &[u8],
) -> Option<([u8; 16], [u8; 12], [u8; 20])> {
    let ksc = ksc?;
    let mut mac = <HmacSha512 as HmacKeyInit>::new_from_slice(ksc).ok()?;
    mac.update(client_rc2);
    mac.update(server_rc2);
    mac.update(sequence);
    let digest = mac.finalize().into_bytes();
    let mut key = [0u8; 16];
    key.copy_from_slice(&digest[..16]);
    let mut iv = [0u8; 12];
    iv.copy_from_slice(&digest[16..28]);
    let mut aad = [0u8; 20];
    aad.copy_from_slice(&digest[28..48]);
    Some((key, iv, aad))
}

pub fn read_clipboard_text() -> Option<String> {
    crate::platform::clipboard::read_text()
}

pub fn write_clipboard_text(text: &str) -> bool {
    if !crate::platform::clipboard::write_text(text) {
        warn!("platform clipboard write failed ({}B)", text.len());
        return false;
    }
    true
}

/// Reads the platform clipboard without blocking the protocol runtime thread.
pub async fn read_clipboard_text_async() -> Option<String> {
    tokio::task::spawn_blocking(read_clipboard_text)
        .await
        .ok()
        .flatten()
}

/// Writes the platform clipboard without blocking the protocol runtime thread.
pub async fn write_clipboard_text_async(text: &str) -> bool {
    let text = text.to_owned();
    tokio::task::spawn_blocking(move || write_clipboard_text(&text))
        .await
        .unwrap_or(false)
}

/// `clipboard_data` payload (the inner `value` string).
pub fn clipboard_data_value(text: &str) -> String {
    let timestamp = utc_timestamp();
    let data = serde_json::json!({
        "data": {
            "files": null,
            "html": null,
            "htmlData": null,
            "image": null,
            "text": { "content": text, "encoding": "UTF-8" },
        },
        // The phone only knows "windows" / "macos" here (app.asar constants);
        // an unknown source is ignored, so we present ourselves as a Windows peer.
        "source": "windows",
        "timestamp": timestamp,
        "crossdeviceClipboardVersion": "1200",
    });
    serde_json::json!({ "key": "clipboard_data", "value": data.to_string() }).to_string()
}

/// All local clipboard data, including diagnostic injections, must pass the
/// phone's activation switch. Capability/activation handshake messages are not
/// clipboard contents and continue to be exchanged while this gate is closed.
fn clipboard_data_frame(
    enabled: bool,
    session: u16,
    our_id: [u8; 6],
    peer_id: [u8; 6],
    text: &str,
) -> Option<Vec<u8>> {
    enabled.then(|| {
        let json = topic_data_json("Oplus_MacConnect", &clipboard_data_value(text));
        build_data_bus_with(session, our_id, peer_id, 0x52, &json)
    })
}

fn utc_timestamp() -> String {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let days = (elapsed.as_secs() / 86_400) as i64;
    let seconds = elapsed.as_secs() % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:09}+00:00",
        seconds / 3600,
        (seconds % 3600) / 60,
        seconds % 60,
        elapsed.subsec_nanos(),
    )
}

// Gregorian calendar conversion for days since 1970-01-01.
fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month as u32, day as u32)
}

#[cfg(test)]
mod utc_timestamp_tests {
    use super::{civil_from_days, utc_timestamp};

    #[test]
    fn unix_epoch_date_is_correct() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_454), (2026, 1, 1));
    }

    #[test]
    fn timestamp_is_iso_8601_utc() {
        let timestamp = utc_timestamp();
        assert_eq!(timestamp.len(), 35);
        assert_eq!(&timestamp[4..5], "-");
        assert_eq!(&timestamp[10..11], "T");
        assert!(timestamp.ends_with("+00:00"));
    }
}

/// Finds the JSON payload after the binary data-bus header. Searching for the
/// first `{` alone can hit that byte inside a sequence/id field and parse the
/// wrong span; prefer the object that owns `dbs_msg_payload`.
fn payload_json_start(body: &[u8]) -> Option<usize> {
    const MARKER: &[u8] = b"\"dbs_msg_payload\"";
    if let Some(position) = body
        .windows(MARKER.len())
        .position(|window| window == MARKER)
    {
        return body[..position].iter().rposition(|byte| *byte == b'{');
    }
    body.iter().position(|byte| *byte == b'{')
}

/// Pulls the text out of a received `clipboard_data` value.
pub fn clipboard_text_from(body: &str) -> Option<String> {
    let bytes = body.as_bytes();
    let start = payload_json_start(bytes)?;
    let json = std::str::from_utf8(&bytes[start..]).ok()?;
    let outer: serde_json::Value = serde_json::from_str(json).ok()?;
    let kv = match outer["dbs_msg_payload"].as_str() {
        Some(payload) => serde_json::from_str(payload).ok()?,
        None => outer,
    };
    let nested = kv["value"].as_str().unwrap_or("");
    let data: serde_json::Value = serde_json::from_str(nested).ok()?;
    data["data"]["text"]["content"]
        .as_str()
        .map(|s| s.to_string())
}

/// The phone's first data-bus frame is a HELLO whose `mPayLoadType` is 1.
/// The PC must answer that HELLO before publishing topic sync and KV data.
pub fn is_data_bus_hello(body: &[u8]) -> bool {
    let start = match body.iter().position(|b| *b == b'{') {
        Some(start) => start,
        None => return false,
    };
    let value: serde_json::Value = match serde_json::from_slice(&body[start..]) {
        Ok(value) => value,
        Err(_) => return false,
    };
    value.get("mPayLoadType").and_then(|value| value.as_u64()) == Some(1)
        && value
            .get("mPayLoad")
            .and_then(|value| value.get("mOnetId"))
            .and_then(|value| value.as_str())
            .is_some_and(|value| !value.is_empty())
}

/// Reads the phone's clipboard activation flag out of `switchStatus`
/// (`Notice_ClipBoard`) or `clipboard_state_active` (`state`).
pub fn clipboard_enabled_from(body: &[u8]) -> Option<bool> {
    let start = payload_json_start(body)?;
    let outer: serde_json::Value = serde_json::from_slice(&body[start..]).ok()?;
    let payload = outer["dbs_msg_payload"].as_str()?;
    let kv: serde_json::Value = serde_json::from_str(payload).ok()?;
    let key = kv["key"].as_str()?;
    let value = kv["value"].as_str().unwrap_or("");
    let inner: serde_json::Value = serde_json::from_str(value)
        .ok()
        .unwrap_or(serde_json::Value::Null);
    let flag = match key {
        "switchStatus" => inner["Notice_ClipBoard"].clone(),
        "clipboard_state_active" => inner["state"].clone(),
        _ => return None,
    };
    Some(flag == 1 || flag == "1" || flag == true)
}

/// Extracts the phone notification's key, label, title, content, and optional icon.
pub fn notification_fields(
    body: &[u8],
) -> Option<(String, String, String, String, Option<String>)> {
    let start = body.iter().position(|b| *b == b'{')?;
    let outer: serde_json::Value = serde_json::from_slice(&body[start..]).ok()?;
    let payload = outer["dbs_msg_payload"].as_str()?;
    let kv: serde_json::Value = serde_json::from_str(payload).ok()?;
    if kv["key"].as_str()? != "HeySynergy_Notification_Add" {
        return None;
    }
    let notification: serde_json::Value = serde_json::from_str(kv["value"].as_str()?).ok()?;
    let notification_key = notification["key"].as_str()?.to_string();
    let details = notification.get("contractedChild").unwrap_or(&notification);
    let label = notification["label"]
        .as_str()
        .or_else(|| notification["headerView"]["label"].as_str())
        .or_else(|| notification["pkgName"].as_str())
        .unwrap_or_default()
        .to_string();
    let text = |name: &str| {
        details[name]
            .as_str()
            .or_else(|| notification[name].as_str())
            .unwrap_or_default()
            .to_string()
    };
    let icon = notification["headerView"]["smallIconByteStr"]
        .as_str()
        .filter(|icon| !icon.is_empty())
        .map(str::to_string);
    Some((
        notification_key,
        label,
        text("contractedTitle"),
        text("contractedContent"),
        icon,
    ))
}

fn notification_icon_file(encoded: &str) -> Option<tempfile::NamedTempFile> {
    use base64::Engine as _;
    use std::io::Write;

    if encoded.len() > 1_000_000 {
        return None;
    }
    let compact = encoded
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect::<Vec<_>>();
    let image = base64::engine::general_purpose::STANDARD
        .decode(compact)
        .ok()?;
    if !image.starts_with(b"\x89PNG\r\n\x1a\n") {
        return None;
    }

    let mut file = tempfile::Builder::new().suffix(".png").tempfile().ok()?;
    file.write_all(&image).ok()?;
    Some(file)
}

#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
struct NotificationSnapshot {
    label: String,
    title: String,
    content: String,
    icon_base64: Option<String>,
}

#[cfg(unix)]
trait NotificationBackend {
    type Handle;

    fn show(&mut self, snapshot: &NotificationSnapshot) -> Option<Self::Handle>;
    fn update(
        &mut self,
        handle: &mut Self::Handle,
        previous: &NotificationSnapshot,
        snapshot: &NotificationSnapshot,
    ) -> bool;
    fn close(&mut self, handle: Self::Handle);
}

#[cfg(unix)]
struct DesktopNotificationState<H> {
    handle: H,
    snapshot: NotificationSnapshot,
    pending: Option<NotificationSnapshot>,
    last_update: std::time::Instant,
}

#[cfg(unix)]
const NOTIFICATION_UPDATE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3);

/// One owner serializes native calls and deadlines. Replacing a pending snapshot
/// or deleting its entry also cancels its deadline: no detached timer can revive it.
#[cfg(unix)]
struct DesktopNotifications<B: NotificationBackend> {
    backend: B,
    entries: std::collections::HashMap<String, DesktopNotificationState<B::Handle>>,
}

#[cfg(unix)]
impl<B: NotificationBackend> DesktopNotifications<B> {
    fn new(backend: B) -> Self {
        Self {
            backend,
            entries: std::collections::HashMap::new(),
        }
    }

    fn show(&mut self, key: String, snapshot: NotificationSnapshot, now: std::time::Instant) {
        if let Some(previous) = self.entries.get_mut(&key) {
            if previous.snapshot == snapshot {
                // The latest state reverted to what is already on screen.
                previous.pending = None;
            } else if now < previous.last_update + NOTIFICATION_UPDATE_INTERVAL {
                previous.pending = Some(snapshot);
            } else {
                previous.pending = None;
                Self::update(&mut self.backend, previous, snapshot, now);
            }
        } else if let Some(handle) = self.backend.show(&snapshot) {
            self.entries.insert(
                key,
                DesktopNotificationState {
                    handle,
                    snapshot,
                    pending: None,
                    last_update: now,
                },
            );
        }
    }

    fn update(
        backend: &mut B,
        state: &mut DesktopNotificationState<B::Handle>,
        snapshot: NotificationSnapshot,
        now: std::time::Instant,
    ) {
        if backend.update(&mut state.handle, &state.snapshot, &snapshot) {
            state.snapshot = snapshot;
        }
        state.last_update = now;
    }

    fn close(&mut self, key: &str) {
        if let Some(state) = self.entries.remove(key) {
            self.backend.close(state.handle);
            info!("desktop notification closed: {}", key);
        }
    }

    fn next_deadline(&self) -> Option<std::time::Instant> {
        self.entries
            .values()
            .filter(|state| state.pending.is_some())
            .map(|state| state.last_update + NOTIFICATION_UPDATE_INTERVAL)
            .min()
    }

    fn flush_due(&mut self, now: std::time::Instant) {
        for state in self.entries.values_mut() {
            if now >= state.last_update + NOTIFICATION_UPDATE_INTERVAL {
                if let Some(snapshot) = state.pending.take() {
                    Self::update(&mut self.backend, state, snapshot, now);
                }
            }
        }
    }
}

#[cfg(unix)]
struct NativeNotification {
    handle: notify_rust::NotificationHandle,
    icon_file: Option<tempfile::NamedTempFile>,
}

#[cfg(unix)]
struct NativeNotificationBackend;

#[cfg(unix)]
impl NotificationBackend for NativeNotificationBackend {
    type Handle = NativeNotification;

    fn show(&mut self, snapshot: &NotificationSnapshot) -> Option<Self::Handle> {
        let icon_file = snapshot
            .icon_base64
            .as_deref()
            .and_then(notification_icon_file);
        let mut notification = notify_rust::Notification::new();
        notification
            .appname(&snapshot.label)
            .summary(&snapshot.title)
            .body(&snapshot.content);
        if let Some(icon) = &icon_file {
            notification.icon(&icon.path().to_string_lossy());
        }
        match notification.show() {
            Ok(handle) => {
                info!("desktop notification shown: {}", snapshot.title);
                Some(NativeNotification { handle, icon_file })
            }
            Err(error) => {
                warn!("desktop notification failed: {}", error);
                None
            }
        }
    }

    fn update(
        &mut self,
        native: &mut Self::Handle,
        previous: &NotificationSnapshot,
        snapshot: &NotificationSnapshot,
    ) -> bool {
        let replacement_icon = snapshot
            .icon_base64
            .as_deref()
            .and_then(notification_icon_file);
        native
            .handle
            .appname(&snapshot.label)
            .summary(&snapshot.title)
            .body(&snapshot.content);
        if let Some(icon) = &replacement_icon {
            native.handle.icon(&icon.path().to_string_lossy());
        }
        match native.handle.update() {
            Ok(()) => {
                if replacement_icon.is_some() {
                    native.icon_file = replacement_icon;
                }
                info!("desktop notification updated: {}", snapshot.title);
                true
            }
            Err(error) => {
                // The old notification still owns its image. Restore the builder
                // before dropping the failed replacement's temporary file.
                native
                    .handle
                    .appname(&previous.label)
                    .summary(&previous.title)
                    .body(&previous.content);
                if let Some(icon) = &native.icon_file {
                    native.handle.icon(&icon.path().to_string_lossy());
                } else {
                    native.handle.icon("");
                }
                warn!("desktop notification update failed: {}", error);
                false
            }
        }
    }

    fn close(&mut self, native: Self::Handle) {
        let NativeNotification { handle, icon_file } = native;
        handle.close();
        drop(icon_file);
    }
}

#[cfg(unix)]
enum DesktopNotificationCommand {
    Show(String, NotificationSnapshot),
    Close(String),
}

#[cfg(unix)]
fn desktop_notification_worker(receiver: std::sync::mpsc::Receiver<DesktopNotificationCommand>) {
    use std::sync::mpsc::RecvTimeoutError;

    let mut notifications = DesktopNotifications::new(NativeNotificationBackend);
    loop {
        let command = match notifications.next_deadline() {
            Some(deadline) => {
                receiver.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
            }
            None => receiver.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        match command {
            Ok(DesktopNotificationCommand::Show(key, snapshot)) => {
                notifications.show(key, snapshot, std::time::Instant::now());
            }
            Ok(DesktopNotificationCommand::Close(key)) => notifications.close(&key),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        notifications.flush_due(std::time::Instant::now());
    }
}

#[cfg(unix)]
static DESKTOP_NOTIFICATIONS: std::sync::LazyLock<
    Option<std::sync::mpsc::Sender<DesktopNotificationCommand>>,
> = std::sync::LazyLock::new(|| {
    let (sender, receiver) = std::sync::mpsc::channel();
    match std::thread::Builder::new()
        .name("desktop-notifications".to_string())
        .spawn(move || desktop_notification_worker(receiver))
    {
        Ok(_) => Some(sender),
        Err(error) => {
            warn!("cannot start desktop notification worker: {}", error);
            None
        }
    }
});

#[cfg(unix)]
fn enqueue_desktop_notification(command: DesktopNotificationCommand) {
    if let Some(sender) = &*DESKTOP_NOTIFICATIONS {
        if let Err(error) = sender.send(command) {
            warn!("desktop notification worker unavailable: {}", error);
        }
    }
}

/// Queues the latest snapshot without blocking the protocol runtime on D-Bus or
/// icon I/O. Updates within three seconds are coalesced and sent at the deadline.
#[cfg(unix)]
pub fn show_desktop_notification(
    notification_key: &str,
    label: &str,
    title: &str,
    content: &str,
    icon_base64: Option<&str>,
) {
    enqueue_desktop_notification(DesktopNotificationCommand::Show(
        notification_key.to_owned(),
        NotificationSnapshot {
            label: label.to_owned(),
            title: title.to_owned(),
            content: content.to_owned(),
            icon_base64: icon_base64.map(str::to_owned),
        },
    ));
}

#[cfg(unix)]
pub fn close_desktop_notification(notification_key: &str) {
    enqueue_desktop_notification(DesktopNotificationCommand::Close(
        notification_key.to_owned(),
    ));
}

fn notification_control_key(body: &[u8], expected_key: &str) -> Option<String> {
    let start = body.iter().position(|byte| *byte == b'{')?;
    let outer: serde_json::Value = serde_json::from_slice(&body[start..]).ok()?;
    let payload = outer["dbs_msg_payload"].as_str()?;
    let kv: serde_json::Value = serde_json::from_str(payload).ok()?;
    (kv["key"].as_str()? == expected_key)
        .then(|| kv["value"].as_str().map(str::to_string))
        .flatten()
}

#[cfg(windows)]
static WINDOWS_NOTIFICATION_IMAGES: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, tempfile::NamedTempFile>>,
> = std::sync::LazyLock::new(|| std::sync::Mutex::new(std::collections::HashMap::new()));

#[cfg(windows)]
fn windows_notification_tag(notification_key: &str) -> String {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    notification_key.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

#[cfg(windows)]
pub fn close_desktop_notification(notification_key: &str) {
    use winrt_toast_reborn::ToastManager;

    let manager = ToastManager::new(ToastManager::POWERSHELL_AUM_ID);
    let tag = windows_notification_tag(notification_key);
    match manager.remove_grouped_tag("oppo-connect", &tag) {
        Ok(()) => info!("Windows toast closed: {}", notification_key),
        Err(error) => warn!(
            "Windows toast close failed: {} ({})",
            notification_key, error
        ),
    }
    WINDOWS_NOTIFICATION_IMAGES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(notification_key);
}

#[cfg(windows)]
pub fn show_desktop_notification(
    notification_key: &str,
    _label: &str,
    title: &str,
    content: &str,
    icon_base64: Option<&str>,
) {
    use winrt_toast_reborn::{
        Image, Toast, ToastManager,
        content::image::{ImageHintCrop, ImagePlacement},
    };

    let image = icon_base64.and_then(notification_icon_file);
    let mut has_app_logo = false;
    let mut toast = Toast::new();
    toast
        .text1(title)
        .text2(content)
        .tag(windows_notification_tag(notification_key))
        .group("oppo-connect");
    if let Some(image) = &image {
        match Image::new_local(image.path()) {
            Ok(app_logo) => {
                toast.image(
                    1,
                    app_logo
                        .with_placement(ImagePlacement::AppLogoOverride)
                        .with_hint_crop(ImageHintCrop::Circle),
                );
                has_app_logo = true;
            }
            Err(error) => warn!("Windows toast app-logo path rejected: {}", error),
        }
    }

    let manager = ToastManager::new(ToastManager::POWERSHELL_AUM_ID);
    match manager.show(&toast) {
        Ok(()) => {
            let mut images = WINDOWS_NOTIFICATION_IMAGES
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(image) = image.filter(|_| has_app_logo) {
                images.insert(notification_key.to_string(), image);
            } else {
                images.remove(notification_key);
            }
            drop(images);
            info!(
                "Windows toast shown ({}): {}",
                if has_app_logo {
                    "phone app icon"
                } else {
                    "default app icon"
                },
                title
            );
        }
        Err(error) => warn!("Windows toast notification failed: {}", error),
    }
}

/// Extracts `(topic, key)` from a decrypted `msg_normal_topic_data` frame.
pub fn kv_message(body: &[u8]) -> Option<(String, String)> {
    let start = body.iter().position(|b| *b == b'{')?;
    let value: serde_json::Value = serde_json::from_slice(&body[start..]).ok()?;
    if value["dbs_msg_type"] != "msg_normal_topic_data" {
        return None;
    }
    let topic = value["dbs_msg_topic"]["dbs_topic_name"]
        .as_str()?
        .to_string();
    let payload = value["dbs_msg_payload"].as_str()?;
    let inner: serde_json::Value = serde_json::from_str(payload).ok()?;
    let key = inner["key"].as_str()?.to_string();
    Some((topic, key))
}

/// `msg_normal_topic_data` envelope the official PC uses for KV messages on a
/// topic (route head `0x52`; the transport session varies per connection).
pub fn topic_data_json(topic: &str, payload_json: &str) -> String {
    let value = serde_json::json!({
        "dbs_msg_topic": { "dbs_topic_name": topic, "dbs_topic_tag": 0 },
        "dbs_msg_to_device": "",
        "dbs_msg_priority": 1,
        "dbs_msg_type": "msg_normal_topic_data",
        "dbs_msg_id": "default",
        "dbs_msg_encrypt": true,
        "dbs_msg_payload": payload_json,
    });
    value.to_string()
}

/// Capability advertisement (`pcAbility`) matching the official PC's shape;
/// `Notice_ClipBoard` / `Notice_CrossDevice` must be 1 for the phone to enable
/// clipboard and notification sync.
pub fn pc_ability_json() -> String {
    let value = serde_json::json!({
        "PCVersion": "17.20.3",
        "CastAbility": 1000,
        "GameCastAbility": 1000,
        "Synergy_CastUnlock_Version": 2000,
        "crossdeviceClipboardVersion": 1200,
        "ScreenShareAbility": 1000,
        "Notice_ClipBoard": 1,
        "Notice_CrossDevice": 1,
        "Notice_MaterialFlow": PC_NOTICE_MATERIAL_FLOW,
        "ScreenShareArrange": 0,
        "Synergy_CastUnlock_Mode": "auto",
        "OSType": PC_OS_TYPE,
        "OSVersion": PC_OS_VERSION,
    });
    value.to_string()
}

fn pc_device_name() -> &'static str {
    PC_DEVICE_NAME
}

/// Agent ids for `app:oshare.win.agent` (sharing/clipboard). These handles are
/// local namespaces, not transport sessions. The peer's value is learned from
/// its `0x0008 sub=0x02` registration; this is only the current fallback.
const PEER_OSHARE_AGENT: u16 = 0x1d3a;
/// This must match the `WinChannelService` handle in `PROFILE_REGISTRATION`.
const LOCAL_OSHARE_AGENT: u16 = 0x1e98;

/// Agent ids for `app:unite.oplus.onet` (protocol doc §5.2): the phone's
/// registered profile agent, the phone's local initiator, and our two ids.
const PEER_SERVICE_AGENT: u16 = 0x8357;
const LOCAL_INITIATOR_AGENT: u16 = 0x7c88;
const LOCAL_SERVICE_AGENT: u16 = 0x18be;

/// The official PC also registers the oshare (sharing/clipboard) profile and its
/// `WinChannelService` agent after the initial profile bind; clipboard traffic is
/// addressed to that agent. Verbatim from the capture.
const OSHARE_SERVICE_LIST: &str = "0008010200000000016170703a6f73686172652e77696e2e6167656e743b";
const OSHARE_REGISTER: &str = "0008020200000000000157696e4368616e6e656c536572766963653b3b00011e986170703a6f73686172652e77696e2e6167656e743b010001000a";

/// Persistent RSA-1024 public identity carried by the official PC's
/// `app:oshare.win.agent` `0x0004 sub=0x06` frame. The phone waits for this
/// certificate before it confirms the oshare service connection.
const OSHARE_RSA_PUBLIC_KEY_DER: &str = "30819f300d06092a864886f70d010101050003818d0030818902818100972f699b7e67426d561c81f1fbcb8621a08a107f4bd518432d187313e043ade2ee1519bdf6cb1b34cf8a0b593d8b0c82f1d77dd7238b6f6129433c84fa6d7050fa43b664ab5d3718844b660971bd569607aed0a591ac5a3d881513c73551d168566ea4b704493a73d4957db14c75fa9e04bf3ae3bdd130826882e4269daeb2910203010001";

/// Keep-alive the official PC sends every few seconds (`0x01fc`, route head `0x51`).
pub const HEARTBEAT_JSON: &str = r#"{"data_sync_msg_encrypt":false,"data_sync_msg_id":"","data_sync_msg_module":16005,"data_sync_msg_payload":"{\"type\":\"heartbeat\",\"timestamp\":\"ping\"}","data_sync_msg_priority":-1,"data_sync_msg_type":"heart_data_query"}"#;

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

fn protocol_log_path(filename: &str) -> std::path::PathBuf {
    if let Ok(dir) = std::env::var("OPLUS_PROTOCOL_LOG_DIR") {
        return std::path::PathBuf::from(dir).join(filename);
    }
    if cfg!(windows) {
        std::env::temp_dir().join(filename)
    } else {
        std::path::PathBuf::from("/tmp").join(filename)
    }
}

fn append_protocol_log(filename: &str, line: &str) {
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(protocol_log_path(filename))
    {
        let _ = writeln!(file, "{line}");
    }
}

async fn write_frame(sock: &mut TcpStream, body: &[u8]) -> std::io::Result<()> {
    let mut frame = Vec::with_capacity(body.len() + 2);
    frame.extend_from_slice(&(body.len() as u16).to_be_bytes());
    frame.extend_from_slice(body);
    // Full transcript of what we send, for diffing against a successful
    // Windows session (`/tmp/oppo-bus-tx.log`).
    append_protocol_log("oppo-bus-tx.log", &hex(body));
    sock.write_all(&frame).await
}

/// Splits the head of `buf` once a whole `u16 length + body` frame is present.
fn take_frame(buf: &mut Vec<u8>) -> Option<Vec<u8>> {
    if buf.len() < 2 {
        return None;
    }
    let len = u16::from_be_bytes([buf[0], buf[1]]) as usize;
    if buf.len() < 2 + len {
        return None;
    }
    let body = buf[2..2 + len].to_vec();
    buf.drain(..2 + len);
    Some(body)
}

/// Runs the PC side of the data channel on an accepted connection.
///
/// `ksc_for_device` resolves the 16-byte key established for this pairing; the
/// auth request carries the phone's device id so the caller can look it up.
pub async fn serve<F, Fut>(
    sock: TcpStream,
    peer: std::net::SocketAddr,
    hostname: &str,
    service_count: u8,
    ksc_for_device: F,
) where
    F: Fn(String) -> Fut,
    Fut: Future<Output = Option<Vec<u8>>>,
{
    serve_with_events(
        sock,
        peer,
        hostname,
        service_count,
        ksc_for_device,
        None,
        None,
    )
    .await;
}

struct DataSessionGuard {
    state: Option<crate::state::AppState>,
    attached: Option<(String, u64)>,
}

impl Drop for DataSessionGuard {
    fn drop(&mut self) {
        if let (Some(state), Some((device_id, token))) = (self.state.take(), self.attached.take()) {
            // Cancellation skips the async epilogue. Only this session's token
            // is removed, so delayed cleanup cannot detach a reconnect.
            tokio::spawn(async move {
                state.detach_data_session(&device_id, token).await;
            });
        }
    }
}

pub async fn serve_with_events<F, Fut>(
    sock: TcpStream,
    peer: std::net::SocketAddr,
    hostname: &str,
    service_count: u8,
    ksc_for_device: F,
    event_broadcaster: Option<std::sync::Arc<dyn Fn(&str) + Send + Sync>>,
    lifecycle: Option<crate::state::AppState>,
) where
    F: Fn(String) -> Fut,
    Fut: Future<Output = Option<Vec<u8>>>,
{
    // The data channel, not the short-lived SCP handshake socket, owns the
    // device connection lifetime. Attach only after auth confirms, detach when
    // this connection ends.
    let mut session = DataSessionGuard {
        state: lifecycle.clone(),
        attached: None,
    };
    run_session(
        sock,
        peer,
        hostname,
        service_count,
        ksc_for_device,
        event_broadcaster,
        lifecycle.as_ref(),
        &mut session.attached,
    )
    .await;
    if let (Some(state), Some((device_id, token))) = (lifecycle, session.attached.take()) {
        // Finish the token removal even if the serving task is cancelled while
        // the cleanup is waiting for state locks.
        let _ = tokio::spawn(async move {
            state.detach_data_session(&device_id, token).await;
        })
        .await;
    }
}

async fn run_session<F, Fut>(
    mut sock: TcpStream,
    peer: std::net::SocketAddr,
    hostname: &str,
    service_count: u8,
    ksc_for_device: F,
    event_broadcaster: Option<std::sync::Arc<dyn Fn(&str) + Send + Sync>>,
    lifecycle: Option<&crate::state::AppState>,
    attached_device: &mut Option<(String, u64)>,
) where
    F: Fn(String) -> Fut,
    Fut: Future<Output = Option<Vec<u8>>>,
{
    let offer = build_offer(hostname, service_count, process_endpoint_id());
    if let Err(error) = write_frame(&mut sock, &offer).await {
        warn!("panta[{}]: cannot send EPI offer: {}", peer, error);
        return;
    }
    info!("panta[{}]: sent EPI offer ({} bytes)", peer, offer.len());

    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = vec![0u8; 8192];
    let mut ksc: Option<Vec<u8>> = None;
    let mut peer_device_id: Option<[u8; 6]> = None;
    let mut authenticated_device_id: Option<[u8; 6]> = None;
    let mut pending_auth_device_id: Option<[u8; 6]> = None;
    let mut link_info_sent = false;
    let mut agent_answered = false;
    let mut oshare_session_opened = false;
    // Each endpoint contributes its own local session in `sub=01`; the peer
    // confirms it in `sub=02`. Every confirmed session is connection-scoped and
    // may carry traffic in either direction.
    let local_session = configured_session("OPLUS_BUS_SESSION").unwrap_or_else(allocate_session_id);
    let oshare_session = configured_session("OPLUS_OSHARE_SESSION").unwrap_or_else(|| {
        loop {
            let candidate = allocate_session_id();
            if candidate != local_session {
                break candidate;
            }
        }
    });
    let mut peer_sessions: Vec<u16> = Vec::new();
    let mut peer_service_agent = PEER_SERVICE_AGENT;
    let mut peer_oshare_agent = PEER_OSHARE_AGENT;
    let mut kms_controller: Option<crate::kms::control::Controller> = None;
    let mut cast_controller: Option<crate::cast::control::Controller> = None;
    let mut bus_topics = BusTopics::default();
    let mut epi_name = String::new();
    let mut epi_type = 0u8;

    let mut local_session_requested = false;
    let mut local_session_confirmed = false;
    let mut oshare_session_confirmed = false;
    let mut oshare_certificate_sent = false;
    let mut profiles_answered = false;
    let mut registration_sent = false;
    let our_id = {
        let hex_id = crate::net_utils::pdid();
        let mut id = [0u8; 6];
        for (slot, chunk) in id.iter_mut().zip(hex_id.as_bytes().chunks(2)) {
            *slot = u8::from_str_radix(std::str::from_utf8(chunk).unwrap_or("00"), 16).unwrap_or(0);
        }
        id
    };
    let mut client_rc2 = [0u8; 8];
    let mut client_fc = 0u64;
    let mut server_rc2 = [0u8; 8];

    // Remember the clipboard we last pushed so idle ticks only send changes.
    let mut last_clipboard = read_clipboard_text_async().await.unwrap_or_default();
    // Mirrors the official renderer: clipboard data is only sent once the phone
    // has activated the channel (`clipboard_state_active` state=1) or reported
    // `switchStatus.Notice_ClipBoard == 1`.
    let mut clipboard_enabled = false;
    let mut last_heartbeat = std::time::Instant::now();
    let inject_text = std::env::var("OPLUS_CLIPBOARD_INJECT").ok();
    let inject_session = configured_session("OPLUS_CLIPBOARD_SESSION").unwrap_or(local_session);
    let inject_delay = std::env::var("OPLUS_CLIPBOARD_INJECT_DELAY_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(8);
    let inject_at = std::time::Instant::now() + std::time::Duration::from_secs(inject_delay);
    let mut injected = false;
    let inject_text_2 = std::env::var("OPLUS_CLIPBOARD_INJECT_2").ok();
    let inject_session_2 = std::env::var("OPLUS_CLIPBOARD_SESSION_2")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .filter(|value| *value > 4 && *value < 1024)
        .unwrap_or(777);
    let inject_delay_2 = std::env::var("OPLUS_CLIPBOARD_INJECT_2_DELAY_SECS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(20);
    let inject_at_2 = std::time::Instant::now() + std::time::Duration::from_secs(inject_delay_2);
    let mut injected_2 = false;
    let allow_unconfirmed =
        std::env::var("OPLUS_CLIPBOARD_ALLOW_UNCONFIRMED").as_deref() == Ok("1");

    loop {
        if let (Some(controller), Some(peer_id)) =
            (kms_controller.as_mut(), authenticated_device_id)
        {
            if local_session_confirmed && link_info_sent {
                for _ in 0..64 {
                    let Some(command) = controller.try_next() else {
                        break;
                    };
                    match command {
                        crate::kms::control::BusCommand::Start { config, reply } => {
                            if reply.is_closed() {
                                continue;
                            }
                            match controller.start(config).await {
                                Ok(publication) => {
                                    bus_topics.kms = true;
                                    for (route_head, json) in [
                                        (0x2a, bus_topics.sync_json()),
                                        (0x52, publication.device_info),
                                    ] {
                                        let frame = build_data_bus_with(
                                            local_session,
                                            our_id,
                                            peer_id,
                                            route_head,
                                            &json,
                                        );
                                        if !matches!(
                                            tokio::time::timeout(
                                                crate::kms::server::WRITE_TIMEOUT,
                                                write_frame(&mut sock, &frame)
                                            )
                                            .await,
                                            Ok(Ok(()))
                                        ) {
                                            controller.abort().await;
                                            return;
                                        }
                                    }
                                    if reply.send(Ok(publication.info)).is_err() {
                                        let _ = controller.stop().await;
                                        bus_topics.kms = false;
                                        let frame = build_data_bus_with(
                                            local_session,
                                            our_id,
                                            peer_id,
                                            0x2a,
                                            &bus_topics.sync_json(),
                                        );
                                        if !matches!(
                                            tokio::time::timeout(
                                                crate::kms::server::WRITE_TIMEOUT,
                                                write_frame(&mut sock, &frame)
                                            )
                                            .await,
                                            Ok(Ok(()))
                                        ) {
                                            return;
                                        }
                                    }
                                }
                                Err(error) => {
                                    let _ = reply.send(Err(error));
                                }
                            }
                        }
                        crate::kms::control::BusCommand::Stop { reply } => {
                            if reply.is_closed() {
                                continue;
                            }
                            match controller.stop().await {
                                Ok(()) => {
                                    bus_topics.kms = false;
                                    let frame = build_data_bus_with(
                                        local_session,
                                        our_id,
                                        peer_id,
                                        0x2a,
                                        &bus_topics.sync_json(),
                                    );
                                    if !matches!(
                                        tokio::time::timeout(
                                            crate::kms::server::WRITE_TIMEOUT,
                                            write_frame(&mut sock, &frame)
                                        )
                                        .await,
                                        Ok(Ok(()))
                                    ) {
                                        return;
                                    }
                                    let _ = reply.send(Ok(()));
                                }
                                Err(error) => {
                                    let _ = reply.send(Err(error));
                                }
                            }
                        }
                        crate::kms::control::BusCommand::Status { reply } => {
                            let _ = reply.send(controller.status());
                        }
                        crate::kms::control::BusCommand::Input {
                            generation,
                            event,
                            reply,
                        } => {
                            if reply.is_closed() {
                                continue;
                            }
                            let result = controller.send_input(generation, event).await;
                            let _ = reply.send(result);
                        }
                        crate::kms::control::BusCommand::Release { generation, reply } => {
                            if reply.is_closed() {
                                continue;
                            }
                            let result = controller.release_input(generation).await;
                            let _ = reply.send(result);
                        }
                        crate::kms::control::BusCommand::Receive { generation, reply } => {
                            if reply.is_closed() {
                                continue;
                            }
                            let _ = reply.send(controller.receive(generation).await);
                        }
                    }
                }
            }
        }
        if let (Some(controller), Some(peer_id)) =
            (cast_controller.as_mut(), authenticated_device_id)
        {
            if local_session_confirmed && link_info_sent {
                for _ in 0..16 {
                    let Some(command) = controller.try_next() else {
                        break;
                    };
                    match command {
                        crate::cast::control::BusCommand::Start { reply } => {
                            if reply.is_closed() {
                                continue;
                            }
                            match controller.start().await {
                                Ok(publication) => {
                                    if reply.is_closed() {
                                        let _ = controller.stop().await;
                                        continue;
                                    }
                                    bus_topics.cast = true;
                                    let messages = std::iter::once((0x2a, bus_topics.sync_json()))
                                        .chain(
                                            publication
                                                .messages
                                                .into_iter()
                                                .map(|json| (0x52, json)),
                                        );
                                    for (route_head, json) in messages {
                                        let frame = build_data_bus_with(
                                            local_session,
                                            our_id,
                                            peer_id,
                                            route_head,
                                            &json,
                                        );
                                        if !matches!(
                                            tokio::time::timeout(
                                                crate::cast::control::BUS_WRITE_TIMEOUT,
                                                write_frame(&mut sock, &frame)
                                            )
                                            .await,
                                            Ok(Ok(()))
                                        ) {
                                            let _ = controller.stop().await;
                                            return;
                                        }
                                    }
                                    if reply.send(Ok(publication.info)).is_err() {
                                        let _ = controller.stop().await;
                                        bus_topics.cast = false;
                                        for (route_head, json) in [
                                            (0x52, crate::cast::control::disconnect_message()),
                                            (0x2a, bus_topics.sync_json()),
                                        ] {
                                            let frame = build_data_bus_with(
                                                local_session,
                                                our_id,
                                                peer_id,
                                                route_head,
                                                &json,
                                            );
                                            if !matches!(
                                                tokio::time::timeout(
                                                    crate::cast::control::BUS_WRITE_TIMEOUT,
                                                    write_frame(&mut sock, &frame)
                                                )
                                                .await,
                                                Ok(Ok(()))
                                            ) {
                                                return;
                                            }
                                        }
                                    }
                                }
                                Err(error) => {
                                    let _ = reply.send(Err(error));
                                }
                            }
                        }
                        crate::cast::control::BusCommand::Stop { reply } => {
                            if reply.is_closed() {
                                continue;
                            }
                            match controller.stop().await {
                                Ok(()) => {
                                    bus_topics.cast = false;
                                    for (route_head, json) in [
                                        (0x52, crate::cast::control::disconnect_message()),
                                        (0x2a, bus_topics.sync_json()),
                                    ] {
                                        let frame = build_data_bus_with(
                                            local_session,
                                            our_id,
                                            peer_id,
                                            route_head,
                                            &json,
                                        );
                                        if !matches!(
                                            tokio::time::timeout(
                                                crate::cast::control::BUS_WRITE_TIMEOUT,
                                                write_frame(&mut sock, &frame)
                                            )
                                            .await,
                                            Ok(Ok(()))
                                        ) {
                                            return;
                                        }
                                    }
                                    let _ = reply.send(Ok(()));
                                }
                                Err(error) => {
                                    let _ = reply.send(Err(error));
                                }
                            }
                        }
                        crate::cast::control::BusCommand::Status { reply } => {
                            let _ = reply.send(Ok(controller.status()));
                        }
                        crate::cast::control::BusCommand::Send {
                            connection_id,
                            payload,
                            reply,
                        } => {
                            if reply.is_closed() {
                                continue;
                            }
                            let result = controller.send(connection_id, payload).await;
                            let _ = reply.send(result);
                        }
                        crate::cast::control::BusCommand::ConfigureVideo {
                            connection_id,
                            width,
                            fps,
                            reply,
                        } => {
                            if reply.is_closed() {
                                continue;
                            }
                            let result =
                                controller.configure_video(connection_id, width, fps).await;
                            let _ = reply.send(result);
                        }
                        crate::cast::control::BusCommand::Receive {
                            connection_id,
                            max,
                            reply,
                        } => {
                            if reply.is_closed() {
                                continue;
                            }
                            let _ = reply.send(controller.receive(connection_id, max));
                        }
                    }
                }
            } else {
                // No DBR HELLO yet: the phone's data bus has not finished the
                // handshake, so publishing a cast topic is impossible. Fail the
                // request immediately instead of letting the caller wait for the
                // 15 s control timeout.
                let reason = "phone data channel is not ready; reconnect the device".to_string();
                for _ in 0..16 {
                    let Some(command) = controller.try_next() else {
                        break;
                    };
                    match command {
                        crate::cast::control::BusCommand::Status { reply } => {
                            let _ = reply.send(Ok(controller.status()));
                        }
                        crate::cast::control::BusCommand::Start { reply } => {
                            let _ = reply.send(Err(reason.clone()));
                        }
                        crate::cast::control::BusCommand::Stop { reply } => {
                            let _ = reply.send(Err(reason.clone()));
                        }
                        crate::cast::control::BusCommand::Send { reply, .. }
                        | crate::cast::control::BusCommand::ConfigureVideo { reply, .. } => {
                            let _ = reply.send(Err(reason.clone()));
                        }
                        crate::cast::control::BusCommand::Receive { reply, .. } => {
                            let _ = reply.send(Err(reason.clone()));
                        }
                    }
                }
            }
        }
        if let (Some(state), Some((_, token))) = (lifecycle, attached_device.as_ref()) {
            if state.take_disconnect_request(*token).await {
                info!("panta[{}]: closing session on user request", peer);
                return;
            }
        }
        // Idle for 700 ms = a chance to mirror local clipboard changes; a
        // cancelled read loses nothing, so the timeout is safe.
        let read = match tokio::select! {
            _ = async {
                match cast_controller.as_mut() {
                    Some(controller) => controller.wait_command().await,
                    None => std::future::pending().await,
                }
            } => continue,
            _ = async {
                match kms_controller.as_mut() {
                    Some(controller) => controller.wait_command().await,
                    None => std::future::pending().await,
                }
            }, if local_session_confirmed && link_info_sent => continue,
            result = tokio::time::timeout(
                std::time::Duration::from_millis(700),
                sock.read(&mut chunk),
            ) => result.map_err(|_| ()),
        } {
            Err(_) => {
                if let Some(peer_id) = peer_device_id {
                    if !injected
                        && inject_text.is_some()
                        && std::time::Instant::now() >= inject_at
                        && (local_session_confirmed || allow_unconfirmed)
                    {
                        let text = inject_text.as_deref().unwrap_or_default();
                        if let Some(frame) = clipboard_data_frame(
                            clipboard_enabled,
                            inject_session,
                            our_id,
                            peer_id,
                            text,
                        ) {
                            let header = encode_transport_header(inject_session, 0);
                            if let Err(error) = write_frame(&mut sock, &frame).await {
                                warn!("panta[{}]: cannot inject clipboard: {}", peer, error);
                                return;
                            }
                            injected = true;
                            info!(
                                "panta[{}]: injected clipboard session={} wire={} ({}B)",
                                peer,
                                inject_session,
                                hex(&header),
                                text.len()
                            );
                        }
                    }
                    if !injected_2
                        && inject_text_2.is_some()
                        && std::time::Instant::now() >= inject_at_2
                        && (local_session_confirmed || allow_unconfirmed)
                    {
                        let text = inject_text_2.as_deref().unwrap_or_default();
                        if let Some(frame) = clipboard_data_frame(
                            clipboard_enabled,
                            inject_session_2,
                            our_id,
                            peer_id,
                            text,
                        ) {
                            let header = encode_transport_header(inject_session_2, 0);
                            if let Err(error) = write_frame(&mut sock, &frame).await {
                                warn!("panta[{}]: cannot inject clipboard 2: {}", peer, error);
                                return;
                            }
                            injected_2 = true;
                            info!(
                                "panta[{}]: injected clipboard 2 session={} wire={} ({}B)",
                                peer,
                                inject_session_2,
                                hex(&header),
                                text.len()
                            );
                        }
                    }
                    if local_session_confirmed
                        && last_heartbeat.elapsed() >= std::time::Duration::from_secs(5)
                    {
                        last_heartbeat = std::time::Instant::now();
                        let frame = build_data_bus_with(
                            local_session,
                            our_id,
                            peer_id,
                            0x51,
                            HEARTBEAT_JSON,
                        );
                        if let Err(error) = write_frame(&mut sock, &frame).await {
                            warn!("panta[{}]: cannot send heartbeat: {}", peer, error);
                            return;
                        }
                    }
                }
                if clipboard_enabled && local_session_confirmed {
                    if let (Some(peer_id), Some(text)) =
                        (peer_device_id, read_clipboard_text_async().await)
                    {
                        if text != last_clipboard && !text.is_empty() {
                            let session_id = std::env::var("OPLUS_CLIPBOARD_SESSION")
                                .ok()
                                .and_then(|value| {
                                    let value = value.trim();
                                    u16::from_str_radix(value.trim_start_matches("0x"), 16).ok()
                                })
                                .unwrap_or(local_session);
                            // Clipboard/notification data use the unite session;
                            // the separate oshare session is for file/share APIs.
                            if let Some(frame) = clipboard_data_frame(
                                clipboard_enabled,
                                session_id,
                                our_id,
                                peer_id,
                                &text,
                            ) {
                                if let Err(error) = write_frame(&mut sock, &frame).await {
                                    warn!("panta[{}]: cannot push clipboard: {}", peer, error);
                                    return;
                                }
                                info!(
                                    "panta[{}]: pushed clipboard on unite={} ({}B)",
                                    peer,
                                    session_id,
                                    text.len()
                                );
                                last_clipboard = text;
                            }
                        }
                    }
                }
                continue;
            }
            Ok(Ok(0)) => {
                info!("panta[{}]: peer closed", peer);
                return;
            }
            Ok(Ok(n)) => n,
            Ok(Err(error)) => {
                warn!("panta[{}]: read failed: {}", peer, error);
                return;
            }
        };
        buf.extend_from_slice(&chunk[..read]);

        while let Some(body) = take_frame(&mut buf) {
            match body.first().copied() {
                Some(MSG_EPI_ANSWER) => match parse_answer(&body) {
                    Some(answer) => {
                        epi_name = answer.friendly_name.clone();
                        epi_type = answer.device_category;
                        info!(
                            "panta[{}]: EPI answer id={} name={} product={} category={} profiles={}",
                            peer,
                            answer.endpoint_id,
                            answer.friendly_name,
                            answer.product,
                            answer.device_category,
                            answer.service_profile_count
                        );
                    }
                    None => warn!("panta[{}]: malformed EPI answer", peer),
                },
                Some(MSG_AUTH_REQUEST) => {
                    let Some(request) = parse_auth_request(&body) else {
                        warn!("panta[{}]: malformed auth request", peer);
                        continue;
                    };
                    let device_id = hex(&request.device_id);
                    info!(
                        "panta[{}]: auth request device_id={} alias={} fc={}",
                        peer,
                        device_id,
                        hex(&request.alias),
                        request.client_fc
                    );
                    peer_device_id = Some(request.device_id);
                    let Some(key) = ksc_for_device(device_id.clone()).await else {
                        warn!(
                            "panta[{}]: no KSC for device {} (SCP SyncSecure not seen)",
                            peer, device_id
                        );
                        continue;
                    };
                    let expected = mac_client(&key, &request.client_rc2, request.client_fc);
                    if expected != request.mac {
                        warn!(
                            "panta[{}]: auth MAC mismatch (got {}, expected {})",
                            peer,
                            hex(&request.mac),
                            hex(&expected)
                        );
                        continue;
                    }
                    info!("panta[{}]: auth request MAC verified", peer);
                    pending_auth_device_id = Some(request.device_id);
                    ksc = Some(key);
                    client_rc2 = request.client_rc2;
                    client_fc = request.client_fc;
                    server_rc2 = random_digital_string();
                    let server_time = unix_time();
                    let key = ksc.as_ref().expect("set above");
                    let mac = mac_server(key, &client_rc2, &server_rc2, server_time);
                    let response = build_auth_response(&server_rc2, server_time, &mac);
                    if let Err(error) = write_frame(&mut sock, &response).await {
                        warn!("panta[{}]: cannot send auth response: {}", peer, error);
                        return;
                    }
                    info!(
                        "panta[{}]: sent auth response (rc2 digits, time={})",
                        peer, server_time
                    );
                }
                Some(MSG_AUTH_CONFIRM) => {
                    let Some(mac) = parse_auth_confirm(&body) else {
                        warn!("panta[{}]: malformed auth confirm", peer);
                        continue;
                    };
                    let Some(key) = ksc.as_ref() else {
                        warn!("panta[{}]: confirm before request", peer);
                        continue;
                    };
                    let expected = mac_confirm(key, &server_rc2, client_fc);
                    if expected == mac {
                        info!(
                            "panta[{}]: auth confirm verified, data channel authenticated",
                            peer
                        );
                        if authenticated_device_id.is_none() {
                            if let (Some(state), Some(device_id)) =
                                (lifecycle, pending_auth_device_id)
                            {
                                authenticated_device_id = Some(device_id);
                                let device_id = hex(&device_id);
                                let token = state.attach_data_session(&device_id).await;
                                if let Ok(local_addr) = sock.local_addr() {
                                    if let Some(registration) = state.kms.register_bus(
                                        &device_id,
                                        peer.ip(),
                                        local_addr.ip(),
                                    ) {
                                        kms_controller = Some(registration.into_controller(
                                            device_id.clone(),
                                            epi_name.clone(),
                                            epi_type,
                                            hostname.to_string(),
                                            crate::net_utils::pdid(),
                                        ));
                                    }
                                    cast_controller = state.cast.register_bus(
                                        &device_id,
                                        peer.ip(),
                                        local_addr.ip(),
                                    );
                                }
                                *attached_device = Some((device_id, token));
                            }
                        }
                        let list = build_service_list(&SERVICE_PROFILES);
                        if let Err(error) = write_frame(&mut sock, &list).await {
                            warn!("panta[{}]: cannot send service list: {}", peer, error);
                            return;
                        }
                        info!(
                            "panta[{}]: sent service list ({} profiles)",
                            peer,
                            SERVICE_PROFILES.len()
                        );
                    } else {
                        warn!(
                            "panta[{}]: auth confirm MAC mismatch (got {}, expected {})",
                            peer,
                            hex(&mac),
                            hex(&expected)
                        );
                    }
                }
                Some(0x00) if body.get(1) == Some(&0x08) && body.get(2) == Some(&0x02) => {
                    info!(
                        "panta[{}]: phone registered its agent profiles hex={}",
                        peer,
                        hex(&body[..body.len().min(400)])
                    );
                    if let Some(agent) = agent_for_profile(&body, "app:unite.oplus.onet") {
                        peer_service_agent = agent;
                        info!(
                            "panta[{}]: phone unite acceptor agent={:#06x}",
                            peer, peer_service_agent
                        );
                    }
                    if let Some(agent) = agent_for_profile(&body, "app:oshare.win.agent") {
                        peer_oshare_agent = agent;
                        info!(
                            "panta[{}]: phone oshare acceptor agent={:#06x}",
                            peer, peer_oshare_agent
                        );
                    }
                }
                Some(0x00) if body.get(1) == Some(&0x08) && body.get(2) == Some(&0x01) => {
                    if profiles_answered {
                        info!("panta[{}]: repeated service list ignored", peer);
                        continue;
                    }
                    if !registration_sent {
                        let registration = profile_registration();
                        if let Err(error) = write_frame(&mut sock, &registration).await {
                            warn!(
                                "panta[{}]: cannot send profile registration: {}",
                                peer, error
                            );
                            return;
                        }
                        registration_sent = true;
                        info!("panta[{}]: sent profile registration", peer);
                    }
                    profiles_answered = true;
                    info!(
                        "panta[{}]: phone service list ({} bytes); selecting our profile",
                        peer,
                        body.len()
                    );
                    for frame in [
                        profile_selection(),
                        profile_bind(),
                        hex_to_vec(OSHARE_SERVICE_LIST),
                        hex_to_vec(OSHARE_REGISTER),
                    ] {
                        if let Err(error) = write_frame(&mut sock, &frame).await {
                            warn!("panta[{}]: cannot send profile frame: {}", peer, error);
                            return;
                        }
                    }
                    info!("panta[{}]: sent profile selection + bind", peer);
                }
                Some(0x00) if body.get(1) == Some(&0x08) && body.get(2) == Some(&0x04) => {
                    info!("panta[{}]: phone acknowledged the profile bind", peer);
                    if !local_session_requested {
                        let request = create_service_request(
                            peer_service_agent,
                            LOCAL_INITIATOR_AGENT,
                            "app:unite.oplus.onet",
                            local_session,
                            104,
                        );
                        if let Err(error) = write_frame(&mut sock, &request).await {
                            warn!("panta[{}]: cannot create local session: {}", peer, error);
                            return;
                        }
                        local_session_requested = true;
                        info!(
                            "panta[{}]: requested local unite session={} after profile bind agent={:#06x}->{:#06x}",
                            peer, local_session, LOCAL_INITIATOR_AGENT, peer_service_agent
                        );
                    }
                }
                Some(0x00) if body.get(1) == Some(&0x10) && body.get(2) == Some(&0x01) => {
                    let Some(entries) = parse_multichannel_request(&body) else {
                        warn!("panta[{}]: malformed multichannel request", peer);
                        continue;
                    };
                    let mut reply = Vec::with_capacity(entries.len());
                    for (channel_type, name) in entries {
                        match open_channel_listener(channel_type, name.clone()).await {
                            Ok(port) => {
                                info!(
                                    "panta[{}]: channel {} ({}) listening on {}",
                                    peer, channel_type, name, port
                                );
                                reply.push((channel_type, 0u8, port));
                            }
                            Err(error) => {
                                warn!(
                                    "panta[{}]: channel {} bind failed: {}",
                                    peer, channel_type, error
                                );
                                reply.push((channel_type, 1u8, 0));
                            }
                        }
                    }
                    let response = build_multichannel_reply(&reply);
                    if let Err(error) = write_frame(&mut sock, &response).await {
                        warn!("panta[{}]: cannot send multichannel reply: {}", peer, error);
                        return;
                    }
                    info!("panta[{}]: sent multichannel reply", peer);
                    if !registration_sent {
                        let registration = profile_registration();
                        if let Err(error) = write_frame(&mut sock, &registration).await {
                            warn!(
                                "panta[{}]: cannot send profile registration: {}",
                                peer, error
                            );
                            return;
                        }
                        registration_sent = true;
                        info!(
                            "panta[{}]: sent profile registration after multichannel reply",
                            peer
                        );
                    }
                }
                Some(_)
                    if body.get(2).is_some_and(|flags| flags & 0x04 != 0)
                        && body.len() > 11
                        && decode_transport_header([body[0], body[1]]).is_some_and(
                            |(session, _)| {
                                // Both endpoints' confirmed sessions are valid
                                // receive keys; direction does not own the id.
                                accepts_business_session(
                                    session,
                                    &peer_sessions,
                                    local_session,
                                    local_session_confirmed,
                                    oshare_session,
                                    oshare_session_confirmed,
                                )
                            },
                        ) =>
                {
                    // AES-128-GCM; key/iv/aad derived from the KSC plus the
                    // per-frame 8-byte sequence (protocol doc §7.3). Reserved
                    // sessions 1/2/4 are the plaintext OAF control frames.
                    let (session_id, _) =
                        decode_transport_header([body[0], body[1]]).expect("guard checked");
                    match business_key(ksc.as_deref(), &client_rc2, &server_rc2, &body[3..11]) {
                        Some((key, iv, aad)) => {
                            let cipher = Aes128Gcm::new_from_slice(&key).expect("AES-128 key");
                            let payload = aes_gcm::aead::Payload {
                                msg: &body[11..],
                                aad: &aad,
                            };
                            match cipher.decrypt(&Nonce::from(iv), payload) {
                                Ok(plain) => {
                                    if !link_info_sent && is_data_bus_hello(&plain) {
                                        if let Some(peer_id) = peer_device_id {
                                            link_info_sent = true;
                                            if let Err(error) = send_data_bus_handshake(
                                                &mut sock,
                                                local_session,
                                                our_id,
                                                peer_id,
                                            )
                                            .await
                                            {
                                                warn!(
                                                    "panta[{}]: cannot complete DBR handshake: {}",
                                                    peer, error
                                                );
                                                return;
                                            }
                                            info!(
                                                "panta[{}]: replied to phone DBR HELLO with link info, \
                                                 topic sync, and capability data",
                                                peer
                                            );
                                        }
                                    }
                                    if let Some((topic, key)) = kv_message(&plain) {
                                        info!("panta[{}]: kv {}.{}", peer, topic, key);
                                        if topic
                                            == "Oplus_HeySynergy_PcConnect_SyncNotification_Publisher"
                                            && key == "HeySynergy_Notification_Add"
                                        {
                                            if let Some((
                                                notification_key,
                                                label,
                                                title,
                                                content,
                                                icon,
                                            )) = notification_fields(&plain)
                                            {
                                                info!(
                                                    "panta[{}]: notification [{}] {} | {}",
                                                    peer, label, title, content
                                                );
                                                if !title.is_empty() || !content.is_empty() {
                                                    show_desktop_notification(
                                                        &notification_key,
                                                        &label,
                                                        &title,
                                                        &content,
                                                        icon.as_deref(),
                                                    );
                                                }
                                                if let Some(broadcaster) = &event_broadcaster {
                                                    let now = std::time::SystemTime::now()
                                                        .duration_since(std::time::UNIX_EPOCH)
                                                        .unwrap_or_default()
                                                        .as_secs();
                                                    let notif_evt = serde_json::json!({
                                                        "event": "notification_callback",
                                                        "data": {
                                                            "key": notification_key,
                                                            "label": label,
                                                            "title": title,
                                                            "content": content,
                                                            "icon": icon,
                                                            "timestamp": now,
                                                        }
                                                    });
                                                    broadcaster(&notif_evt.to_string());
                                                }
                                            }
                                        }
                                        if topic
                                            == "Oplus_HeySynergy_PcConnect_SyncNotification_Publisher"
                                            && key == "HeySynergy_Notification_Delete"
                                        {
                                            if let Some(notification_key) = notification_control_key(
                                                &plain,
                                                "HeySynergy_Notification_Delete",
                                            ) {
                                                #[cfg(any(unix, windows))]
                                                close_desktop_notification(&notification_key);
                                                if let Some(broadcaster) = &event_broadcaster {
                                                    let del_evt = serde_json::json!({
                                                        "event": "notification_deleted",
                                                        "data": {
                                                            "key": notification_key
                                                        }
                                                    });
                                                    broadcaster(&del_evt.to_string());
                                                }
                                            }
                                        }
                                        if key == "clipboard_data" {
                                            if let Some(text) = clipboard_text_from(
                                                &String::from_utf8_lossy(&plain),
                                            ) {
                                                let preview = text
                                                    .char_indices()
                                                    .nth(120)
                                                    .map_or(text.as_str(), |(index, _)| {
                                                        &text[..index]
                                                    });
                                                info!(
                                                    "panta[{}]: phone clipboard ({}B): {}",
                                                    peer,
                                                    text.len(),
                                                    preview
                                                );
                                                if let Some(broadcaster) = &event_broadcaster {
                                                    let clip_evt = serde_json::json!({
                                                        "event": "clipboard_callback",
                                                        "type": "text",
                                                        "source": "remote",
                                                        "content": text,
                                                    });
                                                    broadcaster(&clip_evt.to_string());
                                                }
                                                if write_clipboard_text_async(&text).await {
                                                    last_clipboard = text;
                                                }
                                            }
                                        }
                                        if key == "switchStatus" || key == "clipboard_state_active"
                                        {
                                            if let Some(enabled) = clipboard_enabled_from(&plain) {
                                                clipboard_enabled = enabled;
                                                info!(
                                                    "panta[{}]: clipboard channel {}",
                                                    peer,
                                                    if enabled { "enabled" } else { "disabled" }
                                                );
                                            }
                                        }
                                        if key == "clipboard_state_active" {
                                            if clipboard_enabled && local_session_confirmed {
                                                if let (Some(peer_id), Some(text)) = (
                                                    peer_device_id,
                                                    read_clipboard_text_async().await,
                                                ) {
                                                    if let Some(frame) = clipboard_data_frame(
                                                        clipboard_enabled,
                                                        local_session,
                                                        our_id,
                                                        peer_id,
                                                        &text,
                                                    ) {
                                                        if let Err(error) =
                                                            write_frame(&mut sock, &frame).await
                                                        {
                                                            warn!(
                                                                "panta[{}]: cannot send clipboard: {}",
                                                                peer, error
                                                            );
                                                            return;
                                                        }
                                                        info!(
                                                            "panta[{}]: sent platform clipboard ({}B)",
                                                            peer,
                                                            text.len()
                                                        );
                                                        last_clipboard = text;
                                                    }
                                                }
                                            }
                                        }
                                        if key == "QueryPcVersion" {
                                            if let Some(peer_id) = peer_device_id {
                                                let payload =
                                                    "{\"key\":\"PcVersion\",\"value\":\"17.20.3\"}";
                                                let json =
                                                    topic_data_json("Oplus_PcConnect", payload);
                                                let reply = build_data_bus_with(
                                                    local_session,
                                                    our_id,
                                                    peer_id,
                                                    0x52,
                                                    &json,
                                                );
                                                if let Err(error) =
                                                    write_frame(&mut sock, &reply).await
                                                {
                                                    warn!(
                                                        "panta[{}]: cannot reply PcVersion: {}",
                                                        peer, error
                                                    );
                                                    return;
                                                }
                                                info!("panta[{}]: replied PcVersion", peer);
                                            }
                                        }
                                    }
                                    append_protocol_log(
                                        "oppo-business.log",
                                        &format!(
                                            "[{}] session={} {}",
                                            peer,
                                            session_id,
                                            String::from_utf8_lossy(&plain)
                                        ),
                                    );
                                    let json_start = plain.iter().position(|b| *b == b'{');
                                    match json_start {
                                        Some(offset) => info!(
                                            "panta[{}]: session={} decrypted header={} json={}",
                                            peer,
                                            session_id,
                                            hex(&plain[..offset.min(58)]),
                                            String::from_utf8_lossy(
                                                &plain[offset..plain.len().min(offset + 400)]
                                            )
                                        ),
                                        None => info!(
                                            "panta[{}]: session={} decrypted {}B (no JSON): {}",
                                            peer,
                                            session_id,
                                            plain.len(),
                                            hex(&plain[..plain.len().min(80)])
                                        ),
                                    }
                                }
                                Err(_) => warn!(
                                    "panta[{}]: session={} GCM tag mismatch ({}B)",
                                    peer,
                                    session_id,
                                    body.len() - 11
                                ),
                            }
                        }
                        None => warn!("panta[{}]: no KSC yet, cannot derive business key", peer),
                    }
                }
                Some(0x00) if body.get(1) == Some(&0x04) => {
                    let sub = body.get(2).copied().unwrap_or(0);
                    if sub == 0x01 {
                        if let Some(request) = parse_service_connection_request(&body) {
                            info!(
                                "panta[{}]: service request initiator={:#06x} acceptor={:#06x} \
                                 profile={:?} session/channel={:?}",
                                peer,
                                request.initiator_id,
                                request.acceptor_id,
                                request.profile,
                                request
                                    .infos
                                    .iter()
                                    .map(|(session, channel, _, _, _)| (*session, *channel))
                                    .collect::<Vec<_>>()
                            );
                        }
                    }
                    // The phone repeats these frames; the official PC answers each
                    // subtype exactly once, so duplicates do not confuse its agent
                    // state machine.
                    // The peer tells us its agent ids in the create-service request;
                    // we answer with our own session so it can demux our frames
                    // (protocol doc §17.4).
                    let peer_request = parse_create_service_request(&body);
                    let peer_confirm = parse_create_service_confirm(&body);
                    let replies: Vec<Vec<u8>> = match sub {
                        0x01 if !agent_answered => {
                            agent_answered = true;
                            let mut frames = Vec::new();
                            if let Some(request) = &peer_request {
                                info!(
                                    "panta[{}]: peer create-service acceptor={:#06x} initiator={:#06x} profile={} sessions={:?}",
                                    peer,
                                    request.acceptor,
                                    request.initiator,
                                    request.profile,
                                    request.sessions
                                );
                                for (session, _) in &request.sessions {
                                    if !peer_sessions.contains(session) {
                                        peer_sessions.push(*session);
                                    }
                                }
                                frames.push(create_service_bind(
                                    request.initiator,
                                    request.acceptor,
                                    &request.profile,
                                ));
                                if let Some(peer_session) =
                                    request.sessions.first().map(|(session, _)| *session)
                                {
                                    frames.push(create_service_confirm(
                                        request.initiator,
                                        request.acceptor,
                                        &request.profile,
                                        peer_session,
                                    ));
                                }
                            } else {
                                frames.push(create_service_bind(
                                    PEER_SERVICE_AGENT,
                                    LOCAL_SERVICE_AGENT,
                                    "app:unite.oplus.onet",
                                ));
                            }
                            frames
                        }
                        0x05 if !oshare_certificate_sent
                            && service_frame_profile(&body) == Some("app:oshare.win.agent") =>
                        {
                            oshare_certificate_sent = true;
                            let der = hex_to_vec(OSHARE_RSA_PUBLIC_KEY_DER);
                            vec![create_service_certificate(
                                LOCAL_OSHARE_AGENT,
                                peer_oshare_agent,
                                "app:oshare.win.agent",
                                0x0002,
                                &der,
                            )]
                        }
                        _ => Vec::new(),
                    };
                    if let Some(confirm) = &peer_confirm {
                        info!(
                            "panta[{}]: peer accepted profile={} status={} sessions={:?}",
                            peer, confirm.profile, confirm.status, confirm.sessions
                        );
                        if confirm.status == 0 && confirm.sessions.contains(&local_session) {
                            local_session_confirmed = true;
                            info!(
                                "panta[{}]: local unite session={} confirmed",
                                peer, local_session
                            );
                        }
                        if confirm.status == 0 && confirm.sessions.contains(&oshare_session) {
                            oshare_session_confirmed = true;
                            info!(
                                "panta[{}]: local oshare session={} confirmed",
                                peer, oshare_session
                            );
                        }
                    } else if sub == 0x02 {
                        info!(
                            "panta[{}]: peer accepted sub=0x02 hex={}",
                            peer,
                            hex(&body[..body.len().min(48)])
                        );
                    }
                    info!("panta[{}]: agent frame sub={:#04x}", peer, sub);
                    for frame in &replies {
                        if let Err(error) = write_frame(&mut sock, frame).await {
                            warn!("panta[{}]: cannot send agent frame: {}", peer, error);
                            return;
                        }
                    }
                    if !replies.is_empty() {
                        info!("panta[{}]: replied to agent frame sub={:#04x}", peer, sub);
                        if sub == 0x05 && oshare_certificate_sent {
                            info!(
                                "panta[{}]: sent oshare sub=06 certificate agent={:#06x}->{:#06x}",
                                peer, LOCAL_OSHARE_AGENT, peer_oshare_agent
                            );
                        }
                    }
                    // The official client opens a dedicated session for the sharing agent.
                    if !oshare_session_opened && matches!(sub, 0x03 | 0x06) {
                        oshare_session_opened = true;
                        let frame = create_service_request(
                            peer_oshare_agent,
                            LOCAL_OSHARE_AGENT,
                            "app:oshare.win.agent",
                            oshare_session,
                            104,
                        );
                        if let Err(error) = write_frame(&mut sock, &frame).await {
                            warn!("panta[{}]: cannot open oshare session: {}", peer, error);
                            return;
                        }
                        info!(
                            "panta[{}]: opened oshare session {} agent={:#06x}->{:#06x}",
                            peer, oshare_session, LOCAL_OSHARE_AGENT, peer_oshare_agent
                        );
                    }
                }
                Some(_other) => {
                    let session_id = decode_transport_header([body[0], *body.get(1).unwrap_or(&0)])
                        .map(|(session, _)| session)
                        .unwrap_or(0xffff);
                    info!(
                        "panta[{}]: service frame session={} sub={:#04x} len={} hex={}",
                        peer,
                        session_id,
                        body.get(2).copied().unwrap_or(0),
                        body.len(),
                        hex(&body[..body.len().min(320)])
                    );
                }
                None => warn!("panta[{}]: empty frame", peer),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cast_and_kms_topic_ownership_compose_in_both_stop_orders() {
        for stop_cast_first in [true, false] {
            let mut topics = BusTopics {
                kms: true,
                cast: true,
            };
            for step in 0..3 {
                let value: serde_json::Value = serde_json::from_str(&topics.sync_json()).unwrap();
                for role in ["pub", "sub"] {
                    let names: Vec<&str> = value["dbs_msg_payload"][role]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|topic| topic["dbs_topic_name"].as_str().unwrap())
                        .collect();
                    assert_eq!(names.contains(&"Oplus_KeyMouseLink"), topics.kms);
                    assert_eq!(names.contains(&"Oplus_Cast"), topics.cast);
                    assert!(names.contains(&"Oplus_MacConnect"));
                    assert_eq!(
                        names.len(),
                        2 + usize::from(topics.kms) + usize::from(topics.cast)
                    );
                }
                match (step, stop_cast_first) {
                    (0, true) | (1, false) => topics.cast = false,
                    (0, false) | (1, true) => topics.kms = false,
                    _ => {}
                }
            }
        }
    }

    /// Verbatim official offer (Windows 17.20.3, first successful connection).
    const OFFICIAL_OFFER: &str = "0100024f4146505f4d303030303030303030303030373935413144323633433039423241393633200d01001000000303fe04271002fffb05000a0b020d010e000000000000080759754b6f6e6741070b6d616e7566612d74657374060c70726f647563742d746573740a060c03";

    /// Official `app:unite.oplus.onet` agent-exchange fixture.
    const AGENT_ANNOUNCE: &str =
        "00040183571ee46170703a756e6974652e6f706c75732e6f6e65743b000102660068000100";
    const AGENT_BIND: &str = "00040553f518be6170703a756e6974652e6f706c75732e6f6e65743b";
    const AGENT_CONFIRM: &str =
        "00040253f518be6170703a756e6974652e6f706c75732e6f6e65743b000001017d";

    fn hex_to_vec(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn offer_matches_the_official_capture() {
        let official = hex_to_vec(OFFICIAL_OFFER);
        assert_eq!(official.len(), 109);
        let ours = build_offer("YuKongA", 3, "OAFP_M000000000000795A1D263C09B2A963");
        assert_eq!(ours, official, "offer must be byte identical");
    }

    #[test]
    fn answer_parses_the_official_capture() {
        let official = hex_to_vec(
            "02000a4f4146505f42434533413833304437423631434432314432333646314230413932443341200d010010000002fff50303fe04271005000a0606504c5a31313007074f6e65506c7573080ae4b880e58aa0203135540a080b020c510d010e030000000000ff34",
        );
        let answer = parse_answer(&official).expect("official answer must parse");
        assert_eq!(answer.endpoint_id, "OAFP_BCE3A830D7B61CD21D236F1B0A92D3A");
        assert_eq!(answer.product, "PLZ110");
        assert_eq!(answer.manufacturer, "OnePlus");
        assert_eq!(answer.friendly_name, "一加 15T");
        assert_eq!(answer.device_category, 0x08);
        assert_eq!(answer.service_profile_count, 0x51);
    }

    #[test]
    fn tablet_answer_parses_the_official_capture() {
        // First frame of OPD2515's direction stream (105-byte answer).
        let official = hex_to_vec(
            "02000a4f4146505f42383733453645313530313841394631424537414141393032364538423341200d010010000002fff50303fe04271005000a06074f50443235313507044f50504f080d4f50504f20506164204d696e690a0a0b020c4e0d010e030000000000ffff",
        );
        let answer = parse_answer(&official).expect("tablet answer must parse");
        assert_eq!(answer.endpoint_id, "OAFP_B873E6E15018A9F1BE7AAA9026E8B3A");
        assert_eq!(answer.product, "OPD2515");
        assert_eq!(answer.manufacturer, "OPPO");
        assert_eq!(answer.friendly_name, "OPPO Pad Mini");
        assert_eq!(answer.device_category, 0x0a);
        assert_eq!(answer.service_profile_count, 0x4e);
    }

    #[test]
    fn auth_frames_round_trip_with_a_matching_mac() {
        let ksc = [0x5au8; 16];
        let client_rc2 = [1, 2, 3, 4, 5, 6, 7, 8];
        let client_fc = 128_296u64;

        let mut request = Vec::with_capacity(AUTH_REQUEST_LEN);
        request.push(MSG_AUTH_REQUEST);
        request.push(0x02);
        request.extend_from_slice(&client_rc2);
        request.extend_from_slice(&client_fc.to_be_bytes());
        request.extend_from_slice(&mac_client(&ksc, &client_rc2, client_fc));
        request.extend_from_slice(&[0xc7, 0x7c, 0x70, 0x2e, 0xc0, 0x21]);
        request.extend_from_slice(&[0xa7, 0xe2, 0xe8, 0x8f, 0x6b, 0x30]);

        let parsed = parse_auth_request(&request).expect("request must parse");
        assert_eq!(parsed.client_fc, client_fc);
        assert_eq!(parsed.device_id, [0xc7, 0x7c, 0x70, 0x2e, 0xc0, 0x21]);
        assert_eq!(
            mac_client(&ksc, &parsed.client_rc2, parsed.client_fc),
            parsed.mac
        );

        let server_rc2 = *b"40166000";
        let server_time = 1_793_256_466u64;
        let response = build_auth_response(
            &server_rc2,
            server_time,
            &mac_server(&ksc, &client_rc2, &server_rc2, server_time),
        );
        assert_eq!(response.len(), AUTH_RESPONSE_LEN);
        assert_eq!(&response[..3], &[0x11, 0x01, 0x00]);
        assert_eq!(&response[3..11], &server_rc2);
        assert_eq!(
            u64::from_be_bytes(response[11..19].try_into().unwrap()),
            server_time
        );

        let mut confirm = vec![MSG_AUTH_CONFIRM, 0x02, 0x00];
        confirm.extend_from_slice(&mac_confirm(&ksc, &server_rc2, client_fc));
        let mac = parse_auth_confirm(&confirm).expect("confirm must parse");
        assert_eq!(mac, mac_confirm(&ksc, &server_rc2, client_fc));
    }

    #[test]
    fn mac_uses_hmac_sha512_truncated_to_eight_bytes() {
        // RFC 4231 test case 1 (key = 20 x 0x0b, data = "Hi There").
        let key = [0x0bu8; 20];
        let mut mac = <HmacSha512 as HmacKeyInit>::new_from_slice(&key).unwrap();
        mac.update(b"Hi There");
        let digest = mac.finalize().into_bytes();
        assert_eq!(
            hex(&digest),
            "87aa7cdea5ef619d4ff0b4241a1d6cb02379f4e2ce4ec2787ad0b30545e17cdedaa833b7d6b8a702038b274eaea3f4e4be9d914eeb61f1702e696c203a126854"
        );
        assert_eq!(hmac_truncated(&key, b"Hi There"), digest[..8]);
    }

    #[test]
    fn service_list_matches_the_official_capture() {
        let official = hex_to_vec(
            "00080102000000000473797374656d3a73747265616d7472616e736665723b73797374656d3a66696c657472616e736665723b6170703a756e6974652e6f706c75732e6f6e65743b6170703a6f73686172652e77696e2e6167656e743b",
        );
        assert_eq!(build_service_list(&SERVICE_PROFILES), official);
        assert_eq!(official.len(), 93);
    }

    #[test]
    fn multichannel_frames_match_the_official_capture() {
        let request = hex_to_vec("001001020273747265616d696e673b0066696c657472616e736665723b");
        assert_eq!(
            parse_multichannel_request(&request),
            Some(vec![
                (2u8, "streaming".to_string()),
                (0u8, "filetransfer".to_string())
            ])
        );

        let reply = hex_to_vec("00100202020035363236373b000035363236383b");
        assert_eq!(
            build_multichannel_reply(&[(2, 0, 56267), (0, 0, 56268)]),
            reply
        );
    }

    #[test]
    fn profile_frames_match_the_official_capture() {
        assert_eq!(profile_registration().len(), 299);
        assert_eq!(profile_selection().len(), 30);
        let bind = profile_bind();
        assert_eq!(bind.len(), 63);
        assert_eq!(&profile_selection()[..4], &[0x00, 0x08, 0x01, 0x02]);
        assert_eq!(&bind[..4], &[0x00, 0x08, 0x03, 0x02]);
        assert_eq!(&bind[35..37], &LOCAL_INITIATOR_AGENT.to_be_bytes());
    }

    #[test]
    fn agent_frames_match_the_official_capture() {
        let announce = hex_to_vec(AGENT_ANNOUNCE);
        let bind = hex_to_vec(AGENT_BIND);
        let confirm = hex_to_vec(AGENT_CONFIRM);
        assert_eq!((announce.len(), bind.len(), confirm.len()), (37, 28, 33));
        assert_eq!(&announce[..4], &[0x00, 0x04, 0x01, 0x83]);
        assert_eq!(&bind[..4], &[0x00, 0x04, 0x05, 0x53]);
        assert_eq!(&confirm[..4], &[0x00, 0x04, 0x02, 0x53]);
    }

    #[test]
    fn oshare_certificate_matches_the_official_capture() {
        let certificate = create_service_certificate(
            0x1d3a,
            0x6295,
            "app:oshare.win.agent",
            0x0002,
            &hex_to_vec(OSHARE_RSA_PUBLIC_KEY_DER),
        );
        assert_eq!(certificate.len(), 194);
        assert_eq!(
            &certificate[..7],
            &[0x00, 0x04, 0x06, 0x1d, 0x3a, 0x62, 0x95]
        );
        assert_eq!(
            &certificate[28..32],
            &[0x00, 0x02, 0x00, 0xa2],
            "key kind and DER length"
        );
        assert_eq!(
            service_frame_profile(&certificate),
            Some("app:oshare.win.agent")
        );
    }

    #[test]
    fn data_bus_matches_the_official_data_route() {
        // Official topic-sync routing block (frame 12 of
        // stream-12-pc-to-phone.bin).
        let official = hex_to_vec(
            "099800e1c0dde98ace000000006ae2e05be1c0dde98acec77c702ec0212a050000ff000000ff000000ff000000ff0100000001c77c702ec02100000000",
        );
        let json = "{}";
        let ours = build_data_bus_with(
            614,
            [0xa0, 0x69, 0xc3, 0xec, 0xe4, 0x87],
            [0xc7, 0x7c, 0x70, 0x2e, 0xc0, 0x21],
            0x2a,
            json,
        );
        assert_eq!(&ours[..3], &official[..3]);
        assert_eq!(&ours[13..17].len(), &official[13..17].len());
        assert_eq!(&ours[23..29], &official[23..29], "peer id");
        assert_eq!(
            &ours[29..61],
            &official[29..61],
            "routing block must match the capture"
        );
        assert_eq!(ours.len(), 61 + json.len());
    }

    #[test]
    fn data_bus_header_layout_matches_findings() {
        let ours = [0xa0, 0x69, 0xc3, 0xec, 0xe4, 0x87];
        let phone = [0xc7, 0x7c, 0x70, 0x2e, 0xc0, 0x21];
        let frame = build_data_bus_with(614, ours, phone, 0x03, "{}");
        assert_eq!(&frame[..2], &[0x09, 0x98]);
        assert_eq!(frame[2], 0x00, "flags: plaintext");
        assert_eq!(&frame[3..9], &ours);
        assert_eq!(&frame[9..13], &[0u8; 4]);
        assert_eq!(&frame[17..23], &ours);
        assert_eq!(&frame[23..29], &phone);
        assert_eq!(frame[29], 0x03, "route head");
        assert_eq!(
            &frame[29..61],
            &hex_to_vec("0305000000000000ff000000ff000000ff0100000001c77c702ec02100000000"),
            "control route block"
        );
        assert_eq!(&frame[51..57], &phone, "peer id inside the routing block");
        assert_eq!(frame.len(), 61 + 2);
    }

    #[test]
    fn data_bus_sequence_advances_for_every_frame() {
        let ours = [0xa0, 0x69, 0xc3, 0xec, 0xe4, 0x87];
        let phone = [0xc7, 0x7c, 0x70, 0x2e, 0xc0, 0x21];
        let first = build_data_bus_with(614, ours, phone, 0x03, "{}");
        let second = build_data_bus_with(614, ours, phone, 0x2a, "{}");
        let first_sequence = u64::from_be_bytes(first[9..17].try_into().unwrap());
        let second_sequence = u64::from_be_bytes(second[9..17].try_into().unwrap());
        assert!(second_sequence > first_sequence);
    }

    #[test]
    fn business_key_splits_the_hmac_digest() {
        let ksc = [0x11u8; 16];
        let client_rc2 = [0x22u8; 8];
        let server_rc2 = [0x33u8; 8];
        let sequence = [0, 0, 0, 0, 0, 0, 0, 1];
        let (key, iv, aad) =
            business_key(Some(&ksc), &client_rc2, &server_rc2, &sequence).expect("key material");

        let mut mac = <HmacSha512 as HmacKeyInit>::new_from_slice(&ksc).unwrap();
        mac.update(&client_rc2);
        mac.update(&server_rc2);
        mac.update(&sequence);
        let digest = mac.finalize().into_bytes();
        assert_eq!(key, digest[..16]);
        assert_eq!(iv, digest[16..28]);
        assert_eq!(aad, digest[28..48]);
        assert!(business_key(None, &client_rc2, &server_rc2, &sequence).is_none());
    }

    #[test]
    fn topic_data_envelope_matches_the_official_shape() {
        let json = topic_data_json(
            "Oplus_MacConnect",
            "{\"key\":\"MacClientVersion\",\"value\":\"17.20.3\"}",
        );
        let value: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(value["dbs_msg_type"], "msg_normal_topic_data");
        assert_eq!(value["dbs_msg_topic"]["dbs_topic_name"], "Oplus_MacConnect");
        assert_eq!(value["dbs_msg_topic"]["dbs_topic_tag"], 0);
        assert_eq!(value["dbs_msg_id"], "default");
        assert_eq!(value["dbs_msg_priority"], 1);
        assert_eq!(value["dbs_msg_encrypt"], true);
        assert!(
            value["dbs_msg_payload"]
                .as_str()
                .unwrap()
                .contains("MacClientVersion")
        );

        let ability: serde_json::Value = serde_json::from_str(&pc_ability_json()).unwrap();
        assert_eq!(ability["Notice_ClipBoard"], 1);
        assert_eq!(ability["Notice_CrossDevice"], 1);
        assert_eq!(ability["Notice_MaterialFlow"], PC_NOTICE_MATERIAL_FLOW);
        assert_eq!(ability["OSType"], PC_OS_TYPE);
        assert_eq!(ability["OSVersion"], PC_OS_VERSION);
        assert_eq!(pc_device_name(), PC_DEVICE_NAME);
    }

    #[test]
    fn topic_sync_matches_the_official_windows_registration() {
        let value: serde_json::Value =
            serde_json::from_str(OFFICIAL_WINDOWS_TOPIC_SYNC_JSON).unwrap();
        let payload = &value["dbs_msg_payload"];
        assert_eq!(value["dbs_msg_type"], "msg_topic_sync");
        assert_eq!(payload["pub"].as_array().unwrap().len(), 2);
        assert_eq!(payload["sub"].as_array().unwrap().len(), 2);
        assert!(payload["pub"].as_array().unwrap().iter().any(|topic| {
            topic["dbs_topic_name"] == "Oplus_HeySynergy_PcConnect_SyncNotification_Subscriber"
        }));
        assert!(payload["sub"].as_array().unwrap().iter().any(|topic| {
            topic["dbs_topic_name"] == "Oplus_HeySynergy_PcConnect_SyncNotification_Publisher"
        }));
    }

    #[test]
    fn notification_bootstrap_matches_official_startup_sequence() {
        let messages = notification_startup_messages();
        let keys: Vec<String> = messages
            .iter()
            .map(|message| {
                let envelope: serde_json::Value = serde_json::from_str(message).unwrap();
                assert_eq!(
                    envelope["dbs_msg_topic"]["dbs_topic_name"],
                    "Oplus_HeySynergy_PcConnect_SyncNotification_Subscriber"
                );
                let payload: serde_json::Value =
                    serde_json::from_str(envelope["dbs_msg_payload"].as_str().unwrap()).unwrap();
                assert_eq!(payload["value"], "");
                payload["key"].as_str().unwrap().to_string()
            })
            .collect();
        assert_eq!(
            keys,
            [
                "HeySynergy_Notification_Delete",
                "HeySynergy_Notification_Add",
                "HeySynergy_Notification_Error",
            ]
        );
    }

    #[test]
    fn notification_fields_extract_title_and_content() {
        let notification = serde_json::json!({
            "key": "0|top.yukonga.mishka|1|null|10415",
            "label": "Mishka",
            "contractedTitle": "ATRI-offline",
            "contractedContent": "0 B/s"
        });
        let kv = serde_json::json!({
            "key": "HeySynergy_Notification_Add",
            "value": notification.to_string(),
        });
        let outer = serde_json::json!({ "dbs_msg_payload": kv.to_string() });
        let (notification_key, label, title, content, icon) =
            notification_fields(outer.to_string().as_bytes()).expect("must parse");
        assert_eq!(notification_key, "0|top.yukonga.mishka|1|null|10415");
        assert_eq!(icon, None);
        assert_eq!(
            (label.as_str(), title.as_str(), content.as_str()),
            ("Mishka", "ATRI-offline", "0 B/s")
        );
    }

    #[test]
    fn notification_fields_extract_official_contracted_child() {
        let notification = serde_json::json!({
            "key": "0|com.tencent.mobileqq|521|null|10430",
            "pkgName": "com.tencent.mobileqq",
            "headerView": {
                "label": "QQ",
                "smallIconByteStr": "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+j7V8AAAAASUVORK5CYII="
            },
            "contractedChild": {
                "contractedTitle": "YuKongA(3条新消息)",
                "contractedContent": "YuKongZ: 111"
            }
        });
        let kv = serde_json::json!({
            "key": "HeySynergy_Notification_Add",
            "value": notification.to_string(),
        });
        let outer = serde_json::json!({ "dbs_msg_payload": kv.to_string() });
        let (notification_key, label, title, content, icon) =
            notification_fields(outer.to_string().as_bytes()).expect("must parse");
        assert_eq!(notification_key, "0|com.tencent.mobileqq|521|null|10430");
        assert_eq!(
            icon.as_deref(),
            Some(
                "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+j7V8AAAAASUVORK5CYII="
            )
        );
        assert_eq!(
            (label.as_str(), title.as_str(), content.as_str()),
            ("QQ", "YuKongA(3条新消息)", "YuKongZ: 111")
        );
    }

    #[cfg(unix)]
    #[test]
    fn notification_delete_extracts_stable_notification_key() {
        let kv = serde_json::json!({
            "key": "HeySynergy_Notification_Delete",
            "value": "0|com.tencent.mobileqq|521|null|10430",
        });
        let outer = serde_json::json!({ "dbs_msg_payload": kv.to_string() });
        let body = outer.to_string();
        assert_eq!(
            notification_control_key(body.as_bytes(), "HeySynergy_Notification_Delete").as_deref(),
            Some("0|com.tencent.mobileqq|521|null|10430")
        );
        assert!(notification_control_key(body.as_bytes(), "HeySynergy_Notification_Add").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn phone_base64_png_icon_is_decoded_for_notify_send() {
        let encoded = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+j7V8AAAAASUVORK5CYII=";
        let icon = notification_icon_file(encoded).expect("valid PNG icon");
        let bytes = std::fs::read(icon.path()).expect("read temporary PNG");
        assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
        let path = icon.path().to_owned();
        drop(icon);
        assert!(
            !path.exists(),
            "retiring an icon must remove its temporary file"
        );
    }

    #[cfg(unix)]
    mod notification_updates {
        use super::*;
        use std::time::{Duration, Instant};

        #[derive(Debug, PartialEq, Eq)]
        enum Event {
            Show(usize, String),
            Update(usize, String),
            Close(usize),
        }

        #[derive(Default)]
        struct RecordingBackend {
            next_handle: usize,
            events: Vec<Event>,
        }

        impl NotificationBackend for RecordingBackend {
            type Handle = usize;

            fn show(&mut self, snapshot: &NotificationSnapshot) -> Option<usize> {
                self.next_handle += 1;
                self.events
                    .push(Event::Show(self.next_handle, snapshot.content.clone()));
                Some(self.next_handle)
            }

            fn update(
                &mut self,
                handle: &mut usize,
                _previous: &NotificationSnapshot,
                snapshot: &NotificationSnapshot,
            ) -> bool {
                self.events
                    .push(Event::Update(*handle, snapshot.content.clone()));
                true
            }

            fn close(&mut self, handle: usize) {
                self.events.push(Event::Close(handle));
            }
        }

        fn snapshot(content: &str) -> NotificationSnapshot {
            NotificationSnapshot {
                label: "Downloads".to_string(),
                title: "Download".to_string(),
                content: content.to_string(),
                icon_base64: None,
            }
        }

        #[test]
        fn final_snapshot_is_sent_at_the_trailing_deadline_without_another_add() {
            let now = Instant::now();
            let mut notifications = DesktopNotifications::new(RecordingBackend::default());
            notifications.show("download".into(), snapshot("0%"), now);
            notifications.show(
                "download".into(),
                snapshot("50%"),
                now + Duration::from_secs(1),
            );
            let mut final_snapshot = snapshot("Complete");
            final_snapshot.label = "Completed downloads".into();
            final_snapshot.title = "Saved".into();
            final_snapshot.icon_base64 = Some("final icon".into());
            notifications.show(
                "download".into(),
                final_snapshot,
                now + Duration::from_secs(2),
            );
            assert_eq!(
                notifications.next_deadline(),
                Some(now + NOTIFICATION_UPDATE_INTERVAL)
            );
            notifications.flush_due(now + NOTIFICATION_UPDATE_INTERVAL - Duration::from_millis(1));
            assert_eq!(
                notifications.backend.events,
                vec![Event::Show(1, "0%".into())]
            );
            notifications.flush_due(now + NOTIFICATION_UPDATE_INTERVAL);
            notifications.flush_due(now + Duration::from_secs(30));
            assert_eq!(
                notifications.backend.events,
                vec![
                    Event::Show(1, "0%".into()),
                    Event::Update(1, "Complete".into()),
                ]
            );
            let displayed = &notifications.entries["download"].snapshot;
            assert_eq!(displayed.label, "Completed downloads");
            assert_eq!(displayed.title, "Saved");
            assert_eq!(displayed.icon_base64.as_deref(), Some("final icon"));
            assert_eq!(notifications.next_deadline(), None);
        }

        #[test]
        fn delete_cancels_the_pending_final_snapshot() {
            let now = Instant::now();
            let mut notifications = DesktopNotifications::new(RecordingBackend::default());
            notifications.show("download".into(), snapshot("0%"), now);
            notifications.show(
                "download".into(),
                snapshot("Complete"),
                now + Duration::from_secs(1),
            );
            notifications.close("download");
            assert_eq!(notifications.next_deadline(), None);
            notifications.flush_due(now + Duration::from_secs(30));
            assert!(notifications.entries.is_empty());
            assert_eq!(
                notifications.backend.events,
                vec![Event::Show(1, "0%".into()), Event::Close(1),]
            );
        }

        #[test]
        fn reusing_a_deleted_key_does_not_revive_the_old_update_or_deadline() {
            let now = Instant::now();
            let mut notifications = DesktopNotifications::new(RecordingBackend::default());
            notifications.show("download".into(), snapshot("Old"), now);
            notifications.show(
                "download".into(),
                snapshot("Stale"),
                now + Duration::from_secs(1),
            );
            notifications.close("download");
            notifications.show(
                "download".into(),
                snapshot("New"),
                now + Duration::from_secs(2),
            );
            notifications.show(
                "download".into(),
                snapshot("New complete"),
                now + Duration::from_millis(2500),
            );
            notifications.flush_due(now + NOTIFICATION_UPDATE_INTERVAL);
            assert_eq!(notifications.backend.events.len(), 3);
            assert_eq!(
                notifications.next_deadline(),
                Some(now + Duration::from_secs(5))
            );
            notifications.flush_due(now + Duration::from_secs(5));
            assert_eq!(
                notifications.backend.events,
                vec![
                    Event::Show(1, "Old".into()),
                    Event::Close(1),
                    Event::Show(2, "New".into()),
                    Event::Update(2, "New complete".into()),
                ]
            );
        }

        #[test]
        fn replacement_at_the_deadline_discards_the_superseded_snapshot() {
            let now = Instant::now();
            let mut notifications = DesktopNotifications::new(RecordingBackend::default());
            notifications.show("download".into(), snapshot("Initial"), now);
            notifications.show(
                "download".into(),
                snapshot("Stale"),
                now + Duration::from_secs(1),
            );
            notifications.show(
                "download".into(),
                snapshot("Latest"),
                now + NOTIFICATION_UPDATE_INTERVAL,
            );
            notifications.flush_due(now + Duration::from_secs(30));
            assert_eq!(
                notifications.backend.events,
                vec![
                    Event::Show(1, "Initial".into()),
                    Event::Update(1, "Latest".into()),
                ]
            );
        }

        #[test]
        fn reverting_to_the_displayed_snapshot_cancels_an_older_pending_update() {
            let now = Instant::now();
            let mut notifications = DesktopNotifications::new(RecordingBackend::default());
            notifications.show("download".into(), snapshot("Initial"), now);
            notifications.show(
                "download".into(),
                snapshot("Stale"),
                now + Duration::from_secs(1),
            );
            notifications.show(
                "download".into(),
                snapshot("Initial"),
                now + Duration::from_secs(2),
            );
            notifications.flush_due(now + Duration::from_secs(30));
            assert_eq!(notifications.next_deadline(), None);
            assert_eq!(
                notifications.backend.events,
                vec![Event::Show(1, "Initial".into())]
            );
        }

        #[test]
        fn throttling_deadlines_are_independent_per_key() {
            let now = Instant::now();
            let mut notifications = DesktopNotifications::new(RecordingBackend::default());
            notifications.show("first".into(), snapshot("First"), now);
            notifications.show(
                "second".into(),
                snapshot("Second"),
                now + Duration::from_secs(1),
            );
            notifications.show(
                "first".into(),
                snapshot("First done"),
                now + Duration::from_secs(2),
            );
            notifications.show(
                "second".into(),
                snapshot("Second done"),
                now + Duration::from_secs(2),
            );
            notifications.flush_due(now + Duration::from_secs(3));
            assert_eq!(
                notifications.backend.events.last(),
                Some(&Event::Update(1, "First done".into()))
            );
            notifications.flush_due(now + Duration::from_secs(4));
            assert_eq!(
                notifications.backend.events.last(),
                Some(&Event::Update(2, "Second done".into()))
            );
        }
    }

    #[test]
    fn disabled_clipboard_switches_block_every_data_session() {
        for (key, value) in [
            ("clipboard_state_active", serde_json::json!({"state": 0})),
            ("clipboard_state_active", serde_json::json!({"state": "0"})),
            ("switchStatus", serde_json::json!({"Notice_ClipBoard": 0})),
        ] {
            let body = topic_data_json(
                "Oplus_MacConnect",
                &serde_json::json!({
                    "key": key, "value": value.to_string(),
                })
                .to_string(),
            );
            let enabled = clipboard_enabled_from(body.as_bytes()).unwrap();
            for session in [614, 777, 817] {
                assert!(
                    clipboard_data_frame(enabled, session, [1; 6], [2; 6], "PRIVATE").is_none()
                );
            }
        }
    }

    #[test]
    fn clipboard_can_be_reenabled_without_changing_its_wire_payload() {
        for active in [false, true, false, true] {
            let body = topic_data_json(
                "Oplus_MacConnect",
                &serde_json::json!({
                    "key": "clipboard_state_active",
                    "value": serde_json::json!({"state": u8::from(active)}).to_string(),
                })
                .to_string(),
            );
            let enabled = clipboard_enabled_from(body.as_bytes()).unwrap();
            let frame = clipboard_data_frame(enabled, 614, [1; 6], [2; 6], "LOCAL");
            assert_eq!(frame.is_some(), active);
            if let Some(frame) = frame {
                assert_eq!(
                    decode_transport_header([frame[0], frame[1]]),
                    Some((614, 0))
                );
                assert_eq!(
                    clipboard_text_from(&String::from_utf8_lossy(&frame)).as_deref(),
                    Some("LOCAL")
                );
            }
        }
    }

    #[test]
    fn json_payloads_ignore_braces_in_the_binary_header() {
        let frame = clipboard_data_frame(true, 614, [0x7b; 6], [0x7b; 6], "LOCAL").unwrap();
        let mut body = b"{header noise".to_vec();
        body.extend(frame);
        let text = String::from_utf8_lossy(&body);
        assert_eq!(clipboard_text_from(&text).as_deref(), Some("LOCAL"));

        let enabled = topic_data_json(
            "Oplus_MacConnect",
            &serde_json::json!({
                "key": "clipboard_state_active",
                "value": serde_json::json!({"state": 1}).to_string(),
            })
            .to_string(),
        );
        let mut enabled_body = b"{binary".to_vec();
        enabled_body.extend(enabled.as_bytes());
        assert_eq!(clipboard_enabled_from(&enabled_body), Some(true));
    }

    #[test]
    fn clipboard_enabled_reads_state_and_switch_status() {
        let switch = serde_json::json!({
            "dbs_msg_payload": serde_json::json!({
                "key": "switchStatus",
                "value": serde_json::json!({"Notice_ClipBoard": 1}).to_string()
            }).to_string()
        });
        assert_eq!(
            clipboard_enabled_from(switch.to_string().as_bytes()),
            Some(true)
        );

        let active_off = serde_json::json!({
            "dbs_msg_payload": serde_json::json!({
                "key": "clipboard_state_active",
                "value": serde_json::json!({"state": 0}).to_string()
            }).to_string()
        });
        assert_eq!(
            clipboard_enabled_from(active_off.to_string().as_bytes()),
            Some(false)
        );
    }

    #[test]
    fn clipboard_text_skips_the_binary_data_bus_header() {
        let clipboard = serde_json::json!({
            "crossdeviceClipboardVersion": "1200",
            "data": {"text": {"content": "PHONE-CLIP", "encoding": "UTF-8"}},
            "source": "pad"
        });
        let outer = serde_json::json!({
            "dbs_msg_payload": serde_json::json!({
                "key": "clipboard_data",
                "value": clipboard.to_string()
            }).to_string()
        });
        let body = format!("\0\x01binary-header{}", outer);
        assert_eq!(clipboard_text_from(&body).as_deref(), Some("PHONE-CLIP"));
    }

    #[test]
    fn data_bus_hello_is_recognized_before_topic_sync() {
        let hello = br#"{"mPayLoad":{"mDeviceIdBondMap":{"1":"20108139A722"},"mMprMode":0,"mOnetId":"20108139A722","mDeviceName":"OPPO Pad Mini","mLinkType":16,"mLinkCost":2,"mProtocol":1},"mPayLoadType":1}"#;
        assert!(is_data_bus_hello(hello));
        assert!(!is_data_bus_hello(
            br#"{"dbs_msg_type":"msg_topic_sync","dbs_msg_payload":{"pub":[]}}"#
        ));
    }

    #[test]
    fn link_info_uses_uppercase_device_ids() {
        let value: serde_json::Value =
            serde_json::from_str(&link_info_json("e1c0dde99002", "PC_LINUX")).expect("valid JSON");
        assert_eq!(value["mPayLoad"]["mDeviceIdBondMap"]["1"], "E1C0DDE99002");
        assert_eq!(value["mPayLoad"]["mOnetId"], "E1C0DDE99002");
    }

    #[test]
    fn screen_status_embeds_valid_json() {
        let value: serde_json::Value =
            serde_json::from_str(SCREEN_STATUS_JSON).expect("outer JSON must parse");
        let payload: serde_json::Value =
            serde_json::from_str(value["data_sync_msg_payload"].as_str().unwrap())
                .expect("inner JSON must parse");
        assert_eq!(payload["device_screen_status"], true);
    }

    #[test]
    fn create_service_request_round_trips() {
        let request = create_service_request(0x8357, 0x1ee4, "app:unite.oplus.onet", 614, 104);
        assert_eq!(request, hex_to_vec(AGENT_ANNOUNCE));
        let parsed = parse_create_service_request(&request).expect("must parse");
        assert_eq!(parsed.acceptor, 0x8357);
        assert_eq!(parsed.initiator, 0x1ee4);
        assert_eq!(parsed.profile, "app:unite.oplus.onet");
        assert_eq!(parsed.sessions, vec![(614u16, 104u16)]);
    }

    #[test]
    fn create_service_bind_matches_the_official_shape() {
        let bind = create_service_bind(0x53f5, 0x18be, "app:unite.oplus.onet");
        assert_eq!(bind, hex_to_vec(AGENT_BIND));
    }

    #[test]
    fn transport_header_packs_the_session() {
        assert_eq!(encode_transport_header(614, 0), [0x09, 0x98]);
        assert_eq!(encode_transport_header(127, 0), [0x01, 0xfc]);
        assert_eq!(decode_transport_header([0x09, 0x98]), Some((614, 0)));
        assert_eq!(
            decode_transport_header([0x20, 0x00]),
            None,
            "version must be zero"
        );
    }

    #[test]
    fn receive_filter_accepts_every_confirmed_session() {
        let peer_sessions = [323, 441];
        assert!(accepts_business_session(
            323,
            &peer_sessions,
            862,
            true,
            817,
            true
        ));
        assert!(accepts_business_session(
            862,
            &peer_sessions,
            862,
            true,
            817,
            true
        ));
        assert!(accepts_business_session(
            817,
            &peer_sessions,
            862,
            true,
            817,
            true
        ));
        assert!(!accepts_business_session(
            862,
            &peer_sessions,
            862,
            false,
            817,
            true
        ));
        assert!(!accepts_business_session(
            777,
            &peer_sessions,
            862,
            true,
            817,
            true
        ));
    }

    #[test]
    fn create_service_confirm_matches_the_official_shape() {
        // PC[9] of stream-12: 00 04 02 53f5 18be "app:unite.oplus.onet;" 0000 01 017d
        let confirm = create_service_confirm(0x53f5, 0x18be, "app:unite.oplus.onet", 0x017d);
        assert_eq!(
            confirm,
            hex_to_vec("00040253f518be6170703a756e6974652e6f706c75732e6f6e65743b000001017d")
        );
    }

    #[test]
    fn create_service_confirm_parses_the_official_response() {
        let confirm =
            hex_to_vec("00040253f518be6170703a756e6974652e6f706c75732e6f6e65743b00000101c6");
        let parsed = parse_create_service_confirm(&confirm).expect("must parse");
        assert_eq!(parsed.acceptor, 0x53f5);
        assert_eq!(parsed.initiator, 0x18be);
        assert_eq!(parsed.profile, "app:unite.oplus.onet");
        assert_eq!(parsed.status, 0);
        assert_eq!(parsed.sessions, vec![454]);
    }

    #[test]
    fn registration_exposes_the_phone_unite_acceptor() {
        let registration = hex_to_vec(
            "00080203b9b9defb0002636f6d2e6865797461702e6163636573736f72793b30363839413645433335393142423542464132314344344237464638313036453b0006eb0373797374656d3a73747265616d7472616e736665723b010014001efc4773797374656d3a73747265616d7472616e736665723b010015001eef2973797374656d3a66696c657472616e736665723b010015001e949373797374656d3a66696c657472616e736665723b010014001e83576170703a756e6974652e6f706c75732e6f6e65743b010014000053f56170703a756e6974652e6f706c75732e6f6e65743b0100150000636f6d2e636f6c6f726f732e6f73686172653b44354632443731343730303238413237463034303233344442453742363242303b00021d3a6170703a6f73686172652e77696e2e6167656e743b010004000acd176170703a6f73686172652e77696e2e6167656e743b010005000a",
        );
        assert_eq!(
            agent_for_profile(&registration, "app:unite.oplus.onet"),
            Some(0x8357)
        );
        assert_eq!(
            agent_for_profile(&registration, "app:oshare.win.agent"),
            Some(0x1d3a)
        );
    }

    #[test]
    fn local_registration_matches_the_handles_used_by_later_requests() {
        let registration = profile_registration();
        assert_eq!(
            agent_for_profile(&registration, "app:unite.oplus.onet"),
            Some(LOCAL_INITIATOR_AGENT)
        );
        assert_eq!(
            agent_for_profile(&registration, "app:oshare.win.agent"),
            Some(LOCAL_OSHARE_AGENT)
        );
    }

    #[test]
    fn endpoint_ids_differ_per_call() {
        assert_ne!(generate_endpoint_id(), generate_endpoint_id());
        assert!(generate_endpoint_id().starts_with("OAFP_M000000000000"));
        assert_eq!(generate_endpoint_id().len(), 36);
    }

    #[test]
    fn allocated_sessions_are_in_the_data_range() {
        for _ in 0..256 {
            let session = allocate_session_id();
            assert!((5..=1023).contains(&session));
        }
    }

    #[test]
    fn process_endpoint_id_is_stable() {
        assert_eq!(process_endpoint_id(), process_endpoint_id());
    }

    #[test]
    fn transport_header_round_trips_known_wire_values() {
        for (wire, session) in [
            ([0x01, 0xfc], 127u16),
            ([0x04, 0x6c], 283),
            ([0x09, 0x98], 614),
            ([0x0c, 0x24], 777),
        ] {
            assert_eq!(decode_transport_header(wire), Some((session, 0)));
            assert_eq!(encode_transport_header(session, 0), wire);
        }
        assert_eq!(decode_transport_header([0x20, 0x00]), None);
    }

    #[test]
    fn service_connection_request_parses_official_unite_session() {
        let body = hex_to_vec(AGENT_ANNOUNCE);
        let request = parse_service_connection_request(&body).expect("official request parses");
        assert_eq!(request.acceptor_id, 0x8357);
        assert_eq!(request.initiator_id, 0x1ee4);
        assert_eq!(request.profile, "app:unite.oplus.onet");
        assert_eq!(request.infos, vec![(614, 104, 0, 1, 0)]);
    }
}
