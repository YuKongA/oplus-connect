//! SCP (Smart Connect Protocol) wire codec.
//!
//! Reconstructed from packets captured off a real ColorOS 17 tablet while it
//! dialled our announcement listener on TCP 10152:
//!
//! ```text
//! 40 30 | LL LL | 00 02 | 00 00 | 7b 08 ad 12 | <payload: protobuf, LL bytes>
//! ^magic  ^len BE                          ^tail (constant so far)
//! ```
//!
//! * `magic`  = `0x4030`
//! * `len`    = big-endian u16, payload size *after* the 12-byte header
//! * bytes 2..4 of the tail (`08 ad 12`) decode as protobuf field 1 = 2349,
//!   the `active_connection` command id.
//! * payload is a protobuf message; the observed `active_connection` event is
//!   `{ f2: { f1: timestamp, f2: "active_connection", f3: "com.heytap.accessory" } }`
//!
//! Captured samples (hex, verbatim):
//! ```text
//! 40300030000200007b08ad12122e08e88b9d4d12116163746976655f636f6e6e6563
//! 74696f6e1a14636f6d2e6865797461702e6163636573736f7279
//! ```

pub const SCP_MAGIC: u16 = 0x4030;
pub const SCP_HEADER_LEN: usize = 12;
pub const CMD_ACTIVE_CONNECTION: u64 = 2349;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScpFrame {
    /// Bit 2 of the second header byte marks response direction.
    pub is_response: bool,
    /// Bit 3 marks an encrypted payload.
    pub is_encrypted: bool,
    /// Bit 0 selects raw business data instead of an SCP protocol message.
    pub is_business: bool,
    pub flags: u16,
    pub reserved: u16,
    pub tail: [u8; 4],
    pub payload: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ScpError {
    TooShort,
    BadMagic(u16),
    LengthMismatch { declared: usize, actual: usize },
}

impl std::fmt::Display for ScpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScpError::TooShort => write!(f, "scp: packet shorter than header"),
            ScpError::BadMagic(m) => write!(f, "scp: bad magic 0x{:04x}", m),
            ScpError::LengthMismatch { declared, actual } => {
                write!(f, "scp: declared {} bytes, got {}", declared, actual)
            }
        }
    }
}

impl std::error::Error for ScpError {}

impl ScpFrame {
    pub fn new(cmd: u64, payload: Vec<u8>) -> Self {
        Self {
            is_response: false,
            is_encrypted: false,
            is_business: false,
            flags: 0x0002,
            reserved: 0,
            tail: tail_for_cmd(cmd),
            payload,
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(SCP_HEADER_LEN + self.payload.len());
        let mut head = SCP_MAGIC.to_be_bytes();
        if self.is_response {
            head[1] |= 0x04;
        }
        if self.is_encrypted {
            head[1] |= 0x08;
        }
        if self.is_business {
            head[1] |= 0x01;
        }
        out.extend_from_slice(&head);
        out.extend_from_slice(&(self.payload.len() as u16).to_be_bytes());
        out.extend_from_slice(&self.flags.to_be_bytes());
        out.extend_from_slice(&self.reserved.to_be_bytes());
        out.extend_from_slice(&self.tail);
        out.extend_from_slice(&self.payload);
        out
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ScpError> {
        if buf.len() < SCP_HEADER_LEN {
            return Err(ScpError::TooShort);
        }
        let raw = u16::from_be_bytes([buf[0], buf[1]]);
        let is_response = raw & 0x04 != 0;
        let is_encrypted = raw & 0x08 != 0;
        let is_business = raw & 0x01 != 0;
        if raw & !0x0d != SCP_MAGIC {
            return Err(ScpError::BadMagic(raw));
        }
        let declared = u16::from_be_bytes([buf[2], buf[3]]) as usize;
        let body = &buf[SCP_HEADER_LEN..];
        if body.len() != declared {
            return Err(ScpError::LengthMismatch {
                declared,
                actual: body.len(),
            });
        }
        Ok(Self {
            is_response,
            is_encrypted,
            is_business,
            flags: u16::from_be_bytes([buf[4], buf[5]]),
            reserved: u16::from_be_bytes([buf[6], buf[7]]),
            tail: [buf[8], buf[9], buf[10], buf[11]],
            payload: body.to_vec(),
        })
    }

    /// Command id carried in the fixed tail (`7b 08 <varint>`).
    pub fn command(&self) -> Option<u64> {
        let mut slice = &self.tail[2..];
        read_varint(&mut slice)
    }
}

fn tail_for_cmd(cmd: u64) -> [u8; 4] {
    let mut tail = [0u8; 4];
    tail[0] = 0x7b; // constant marker observed on every phone packet
    tail[1] = 0x08; // protobuf tag: field 1, varint
    let mut tmp = [0u8; 3];
    let mut n = 0;
    let mut v = cmd;
    loop {
        let mut byte = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            byte |= 0x80;
        }
        tmp[n] = byte;
        n += 1;
        if v == 0 || n == 3 {
            break;
        }
    }
    tail[2..2 + n].copy_from_slice(&tmp[..n]);
    tail
}

// ---------------- minimal protobuf primitives ----------------

pub fn read_varint(buf: &mut &[u8]) -> Option<u64> {
    let mut result: u64 = 0;
    let mut shift = 0u32;
    while let Some((&byte, rest)) = buf.split_first() {
        *buf = rest;
        result |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(result);
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
    None
}

pub fn write_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let mut byte = (v & 0x7f) as u8;
        v >>= 7;
        if v != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if v == 0 {
            break;
        }
    }
}

pub fn write_len_delimited(out: &mut Vec<u8>, field: u32, data: &[u8]) {
    write_varint(out, (u64::from(field) << 3) | 2);
    write_varint(out, data.len() as u64);
    out.extend_from_slice(data);
}

pub fn write_varint_field(out: &mut Vec<u8>, field: u32, value: u64) {
    write_varint(out, u64::from(field) << 3);
    write_varint(out, value);
}

/// A decoded `f2` sub-message of the `active_connection` flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopicEvent {
    pub timestamp: u64,
    pub topic: String,
    pub package: String,
}

impl TopicEvent {
    pub fn encode(&self) -> Vec<u8> {
        let mut inner = Vec::new();
        write_varint_field(&mut inner, 1, self.timestamp);
        write_len_delimited(&mut inner, 2, self.topic.as_bytes());
        write_len_delimited(&mut inner, 3, self.package.as_bytes());

        let mut outer = Vec::new();
        write_len_delimited(&mut outer, 2, &inner);
        outer
    }

    /// Decodes the payload of an `active_connection` frame.
    pub fn decode(payload: &[u8]) -> Option<Self> {
        let mut buf = payload;
        while let Some(key) = read_varint(&mut buf) {
            let field = (key >> 3) as u32;
            let wire = (key & 7) as u8;
            match wire {
                0 => {
                    read_varint(&mut buf)?;
                }
                2 => {
                    let len = read_varint(&mut buf)? as usize;
                    if buf.len() < len {
                        return None;
                    }
                    let (data, rest) = buf.split_at(len);
                    buf = rest;
                    if field == 2 {
                        return parse_topic_event(data);
                    }
                }
                _ => return None,
            }
        }
        None
    }
}

fn parse_topic_event(data: &[u8]) -> Option<TopicEvent> {
    let mut buf = data;
    let mut ev = TopicEvent {
        timestamp: 0,
        topic: String::new(),
        package: String::new(),
    };
    while let Some(key) = read_varint(&mut buf) {
        let field = (key >> 3) as u32;
        let wire = (key & 7) as u8;
        match (field, wire) {
            (1, 0) => ev.timestamp = read_varint(&mut buf)?,
            (_, 2) => {
                let len = read_varint(&mut buf)? as usize;
                if buf.len() < len {
                    return None;
                }
                let (text, rest) = buf.split_at(len);
                buf = rest;
                match field {
                    2 => ev.topic = String::from_utf8_lossy(text).into_owned(),
                    3 => ev.package = String::from_utf8_lossy(text).into_owned(),
                    _ => {}
                }
            }
            _ => return None,
        }
    }
    Some(ev)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_ACTIVE_CONNECTION: &str = "40300030000200007b08ad12122e08e88b9d4d12116163746976655f636f6e6e656374696f6e1a14636f6d2e6865797461702e6163636573736f7279";
    const SAMPLE_TIMEOUT_ERROR: &str = "4030004c000000007b08ad1208ffff03124608e98b9d4d10d2131a3c54696d6564206f75742077616974696e6720666f722035303030206d73205b54696d656f757443616e63656c6c6174696f6e457863657074696f6e5d";

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn decodes_and_round_trips_captured_frames() {
        for sample in [SAMPLE_ACTIVE_CONNECTION, SAMPLE_TIMEOUT_ERROR] {
            let raw = hex(sample);
            let frame = ScpFrame::decode(&raw).expect("captured frame must decode");
            assert_eq!(frame.encode(), raw, "re-encoding must be byte identical");
            assert_eq!(frame.command(), Some(CMD_ACTIVE_CONNECTION));
        }
    }

    #[test]
    fn encodes_and_decodes_direction_encryption_and_business_bits() {
        for (is_response, is_encrypted, is_business, header) in [
            (false, false, false, 0x30),
            (true, false, false, 0x34),
            (false, true, false, 0x38),
            (true, true, false, 0x3c),
            (false, true, true, 0x39),
            (true, true, true, 0x3d),
        ] {
            let mut frame = ScpFrame::new(CMD_ACTIVE_CONNECTION, vec![1, 2, 3]);
            frame.is_response = is_response;
            frame.is_encrypted = is_encrypted;
            frame.is_business = is_business;
            let encoded = frame.encode();
            assert_eq!(&encoded[..2], &[0x40, header]);
            assert_eq!(ScpFrame::decode(&encoded).unwrap(), frame);
        }
    }

    #[test]
    fn parses_active_connection_topic_event() {
        let raw = hex(SAMPLE_ACTIVE_CONNECTION);
        let frame = ScpFrame::decode(&raw).unwrap();
        let ev = TopicEvent::decode(&frame.payload).expect("topic event must parse");
        assert_eq!(ev.topic, "active_connection");
        assert_eq!(ev.package, "com.heytap.accessory");
        assert_eq!(
            ev.encode(),
            frame.payload,
            "topic event re-encode must match"
        );
    }

    #[test]
    fn rejects_truncated_and_foreign_packets() {
        let raw = hex(SAMPLE_ACTIVE_CONNECTION);
        assert_eq!(ScpFrame::decode(&raw[..8]), Err(ScpError::TooShort));
        assert_eq!(
            ScpFrame::decode(&raw[..raw.len() - 1]).unwrap_err(),
            ScpError::LengthMismatch {
                declared: 48,
                actual: 47
            }
        );
        let mut bad = raw.clone();
        bad[0] = 0x41;
        assert_eq!(
            ScpFrame::decode(&bad).unwrap_err(),
            ScpError::BadMagic(0x4130)
        );
    }
}
