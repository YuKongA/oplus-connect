//! SCP application-layer messages.
//!
//! Recovered from the macOS daemon's `prost` implementations
//! (`connection::protocol::scp_data::*`) by disassembling the generated
//! `merge_field` functions, which compare the incoming protobuf field number
//! and name the field in their error strings:
//!
//! ```text
//! ScpWrapData   { 1: msg_type(int32), 2: data(bytes) }
//! ScpHelloData  { 1: sequence_no(int32), 2: service_name(string),
//! ScpSyncData   { 1: device_name(string), 2: key_type, 3: custom_data(bytes),
//!                 4: device_type, 5: active_trigger_mode, 6: model_id(string), 7: link_type }
//! ScpDisconnectData { 1: disconnect_code(int32), 2: disconnect_reason(string) }
//! ```
//!
//! `ScpWrapData.msg_type` values (from the enum name table):
//! `Hello = 0, SyncSecure = 1, Ukey2Init = 2, Ukey2Finished = 3,
//!  OobkeyBuild = 4, OobkeyProcess = 5, OobkeyFinished = 6, Disconnect = 7`,
//! and `0xFFFF` marks an error (observed in the phone's timeout packet).

use super::codec::{read_varint, write_len_delimited, write_varint_field};

pub const MSG_HELLO: i32 = 0;
/// Device-info exchange the phone sends immediately after the Hello exchange.
/// Captured on the wire (tablet -> PC, 31 bytes of `data`):
/// i.e. `{ f1: name, f2: key_type=1, f4: device_type(10 = PAD),
///         f5: active_trigger_mode=1, f6: model_id, f7: link_type=1 }`.
pub const MSG_SYNC: i32 = 1;
pub const MSG_SYNC_SECURE: i32 = 2;
pub const MSG_UKEY2_INIT: i32 = 3;
pub const MSG_UKEY2_FINISHED: i32 = 4;
pub const MSG_DISCONNECT: i32 = 7;
pub const MSG_ERROR: i32 = 0xFFFF;

/// `ScpWrapData` — the envelope every SCP payload uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScpWrapData {
    pub msg_type: i32,
    pub data: Vec<u8>,
}

impl ScpWrapData {
    pub fn new(msg_type: i32, data: Vec<u8>) -> Self {
        Self { msg_type, data }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.data.len() + 8);
        if self.msg_type != 0 {
            write_varint_field(&mut out, 1, self.msg_type as u64);
        }
        write_len_delimited(&mut out, 2, &self.data);
        out
    }

    pub fn decode(buf: &[u8]) -> Option<Self> {
        let mut buf = buf;
        let mut wrap = Self {
            msg_type: 0,
            data: Vec::new(),
        };
        while let Some(key) = read_varint(&mut buf) {
            let field = (key >> 3) as u32;
            let wire = (key & 7) as u8;
            match wire {
                0 => {
                    let v = read_varint(&mut buf)?;
                    if field == 1 {
                        wrap.msg_type = v as i32;
                    }
                }
                2 => {
                    let len = read_varint(&mut buf)? as usize;
                    if buf.len() < len {
                        return None;
                    }
                    let (chunk, rest) = buf.split_at(len);
                    buf = rest;
                    if field == 2 {
                        wrap.data = chunk.to_vec();
                    }
                }
                _ => return None,
            }
        }
        Some(wrap)
    }
}

/// `ScpHelloData` — first message a phone sends when it wants to talk.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScpHelloData {
    pub sequence_no: i32,
    pub service_name: String,
    pub namespace: String,
    pub channel: String,
}

impl ScpHelloData {
    pub fn decode(buf: &[u8]) -> Option<Self> {
        let mut buf = buf;
        let mut hello = Self::default();
        while let Some(key) = read_varint(&mut buf) {
            let field = (key >> 3) as u32;
            let wire = (key & 7) as u8;
            match wire {
                0 => {
                    let v = read_varint(&mut buf)?;
                    if field == 1 {
                        hello.sequence_no = v as i32;
                    }
                }
                2 => {
                    let len = read_varint(&mut buf)? as usize;
                    if buf.len() < len {
                        return None;
                    }
                    let (chunk, rest) = buf.split_at(len);
                    buf = rest;
                    let text = String::from_utf8_lossy(chunk).into_owned();
                    match field {
                        2 => hello.service_name = text,
                        3 => hello.namespace = text,
                        4 => hello.channel = text,
                        _ => {}
                    }
                }
                _ => return None,
            }
        }
        Some(hello)
    }
}

/// `ScpSyncData` — device identity and options sent after Hello.
///
/// The official PC encodes field 3 from `ScpOptions.custom_sync_data` and field
/// 6 from `ScpOptions.model_id`. The captured tablet Sync omits field 3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScpSyncData {
    pub device_name: String,
    pub key_type: u64,
    pub custom_data: Vec<u8>,
    pub device_type: u64,
    pub active_trigger_mode: u64,
    pub model_id: String,
    pub link_type: u64,
}

impl ScpSyncData {
    pub fn new(
        device_name: impl Into<String>,
        custom_data: Vec<u8>,
        device_type: u64,
        model_id: impl Into<String>,
    ) -> Self {
        Self {
            device_name: device_name.into(),
            key_type: 0,
            custom_data,
            device_type,
            active_trigger_mode: 0,
            model_id: model_id.into(),
            link_type: 2,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        // Field order matches the captured phone sync and PC response shape.
        let mut out = Vec::new();
        if !self.device_name.is_empty() {
            write_len_delimited(&mut out, 1, self.device_name.as_bytes());
        }
        if self.key_type != 0 {
            write_varint_field(&mut out, 2, self.key_type);
        }
        if !self.custom_data.is_empty() {
            write_len_delimited(&mut out, 3, &self.custom_data);
        }
        if self.device_type != 0 {
            write_varint_field(&mut out, 4, self.device_type);
        }
        if self.active_trigger_mode != 0 {
            write_varint_field(&mut out, 5, self.active_trigger_mode);
        }
        if !self.model_id.is_empty() {
            write_len_delimited(&mut out, 6, self.model_id.as_bytes());
        }
        if self.link_type != 0 {
            write_varint_field(&mut out, 7, self.link_type);
        }
        out
    }

    pub fn decode(buf: &[u8]) -> Option<Self> {
        let mut buf = buf;
        let mut msg = Self::new(String::new(), Vec::new(), 0, String::new());
        while let Some(key) = read_varint(&mut buf) {
            let field = (key >> 3) as u32;
            match (key & 7) as u8 {
                0 => {
                    let v = read_varint(&mut buf)?;
                    match field {
                        2 => msg.key_type = v,
                        4 => msg.device_type = v,
                        5 => msg.active_trigger_mode = v,
                        7 => msg.link_type = v,
                        _ => {}
                    }
                }
                2 => {
                    let len = read_varint(&mut buf)? as usize;
                    if buf.len() < len {
                        return None;
                    }
                    let (chunk, rest) = buf.split_at(len);
                    buf = rest;
                    match field {
                        1 => msg.device_name = String::from_utf8_lossy(chunk).into_owned(),
                        3 => msg.custom_data = chunk.to_vec(),
                        6 => msg.model_id = String::from_utf8_lossy(chunk).into_owned(),
                        _ => {}
                    }
                }
                _ => return None,
            }
        }
        Some(msg)
    }
}

/// KSC identity exchanged in the encrypted SCP `SyncSecure` stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScpSyncSecureData {
    pub device_id: String,
    pub ksc_alias: Vec<u8>,
    pub ksc: Vec<u8>,
}

impl ScpSyncSecureData {
    pub fn encode(&self) -> Vec<u8> {
        let mut out =
            Vec::with_capacity(self.device_id.len() + self.ksc_alias.len() + self.ksc.len() + 12);
        if !self.device_id.is_empty() {
            write_len_delimited(&mut out, 1, self.device_id.as_bytes());
        }
        if !self.ksc.is_empty() {
            write_len_delimited(&mut out, 2, &self.ksc);
        }
        if !self.ksc_alias.is_empty() {
            write_len_delimited(&mut out, 3, &self.ksc_alias);
        }
        out
    }

    pub fn decode(mut input: &[u8]) -> Option<Self> {
        let mut data = Self {
            device_id: String::new(),
            ksc_alias: Vec::new(),
            ksc: Vec::new(),
        };
        while !input.is_empty() {
            let key = read_varint(&mut input)?;
            let field = (key >> 3) as u32;
            if key & 7 != 2 {
                return None;
            }
            let len = read_varint(&mut input)? as usize;
            if len > input.len() {
                return None;
            }
            let (value, rest) = input.split_at(len);
            input = rest;
            match field {
                1 => data.device_id = String::from_utf8(value.to_vec()).ok()?,
                2 => data.ksc = value.to_vec(),
                3 => data.ksc_alias = value.to_vec(),
                _ => {}
            }
        }
        Some(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verbatim wrap bytes of the phone's first packet on TCP 10152.
    const PHONE_HELLO: &str = "122f08a6cdb6a80412116163746976655f636f6e6e656374696f6e1a14636f6d2e6865797461702e6163636573736f7279";
    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn decodes_captured_phone_hello() {
        let raw = hex(PHONE_HELLO);
        let wrap = ScpWrapData::decode(&raw).expect("wrap data must decode");
        assert_eq!(wrap.msg_type, MSG_HELLO);

        let hello = ScpHelloData::decode(&wrap.data).expect("hello must decode");
        assert_eq!(hello.service_name, "active_connection");
        assert_eq!(hello.namespace, "com.heytap.accessory");
        assert_ne!(hello.sequence_no, 0);

        assert_eq!(wrap.encode(), raw, "re-encode must be byte identical");
    }

    /// Verbatim `data` bytes of the phone's post-Hello sync message.
    const PHONE_SYNC: &str = "0a0d4f50504f20506164204d696e691001200a280132063631344142313801";

    #[test]
    fn decodes_captured_phone_sync() {
        let raw = hex(PHONE_SYNC);
        let sync = ScpSyncData::decode(&raw).expect("phone sync must decode");
        assert_eq!(sync.device_name, "OPPO Pad Mini");
        assert_eq!(sync.device_type, 10);
        assert_eq!(sync.model_id, "614AB1");
        assert_eq!(sync.key_type, 1);
        assert_eq!(sync.active_trigger_mode, 1);
        assert_eq!(sync.link_type, 1);
        assert_eq!(sync.encode(), raw, "re-encode must be byte identical");
    }

    #[test]
    fn sync_reply_round_trip() {
        let ours = ScpSyncData::new("yukonga-msi", Vec::new(), 6, "1");
        let encoded = ours.encode();
        assert_eq!(encoded, hex("0a0b79756b6f6e67612d6d736920063201313802"));
        assert_eq!(ScpSyncData::decode(&encoded).unwrap(), ours);

        let wrap = ScpWrapData::new(MSG_SYNC, encoded);
        let back = ScpWrapData::decode(&wrap.encode()).unwrap();
        assert_eq!(back.msg_type, MSG_SYNC);
        assert_eq!(ScpSyncData::decode(&back.data).unwrap(), ours);
    }
    #[test]
    fn sync_secure_preserves_device_id_alias_and_key_fields() {
        let message = ScpSyncSecureData {
            device_id: "A069C3ECE487".to_string(),
            ksc_alias: vec![1, 2, 3, 4, 5, 6],
            ksc: vec![0x5a; 16],
        };
        let encoded = message.encode();
        assert_eq!(
            encoded,
            [
                vec![0x0a, 12],
                b"A069C3ECE487".to_vec(),
                vec![0x12, 16],
                vec![0x5a; 16],
                vec![0x1a, 6, 1, 2, 3, 4, 5, 6]
            ]
            .concat()
        );
        assert_eq!(ScpSyncSecureData::decode(&encoded), Some(message));
        assert!(ScpSyncSecureData::decode(&encoded[..encoded.len() - 1]).is_none());
    }
}
