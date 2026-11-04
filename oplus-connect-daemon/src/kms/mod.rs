//! Official KMS framing and AES-128-GCM body protection.
//!
//! The codec itself owns no sockets. The explicit server owns connection
//! lifetimes and timeouts; Panta owns topic negotiation. No input is captured.
//! Header fields are not authenticated by the official protocol (empty AAD).

pub mod input;
pub mod server;
pub use input::InputEvent;
pub mod control;

use aes_gcm::{
    Aes128Gcm, Nonce, Tag,
    aead::{AeadInOut, KeyInit},
};
use getrandom::SysRng;
use rand_core::{Rng, UnwrapErr};
use sha2::{Digest, Sha256};
use std::io::{self, ErrorKind};
use tokio::io::{AsyncRead, AsyncReadExt};

pub const HEADER_LEN: usize = 24;
pub const MAX_SEQUENCE: u64 = (1 << 48) - 1;
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const ENCRYPTION_OVERHEAD: usize = NONCE_LEN + TAG_LEN;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Metadata {
    /// Kept numeric so unrecognized events can be dispatched by the caller.
    pub event_type: u8,
    pub sequence: u64,
    pub timestamp_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub metadata: Metadata,
    pub encrypted: bool,
    /// Wire length, including nonce and tag when encrypted.
    pub body_len: u32,
}

#[derive(Debug)]
pub struct Frame<'a> {
    pub header: Header,
    /// Authenticated plaintext for encrypted frames; borrowed from the input buffer.
    pub body: &'a [u8],
}

/// Reuses the caller's frame buffer; does not retain plaintext or packet history.
/// `max_body_len` bounds the wire body allocation before reading from TCP.
pub struct Codec {
    cipher: Aes128Gcm,
    max_body_len: u32,
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, message)
}

impl Codec {
    pub fn new(password: &str, max_body_len: u32) -> Self {
        let digest = Sha256::digest(password.as_bytes());
        Self {
            cipher: Aes128Gcm::new_from_slice(&digest[..16]).expect("SHA-256 prefix is 16 bytes"),
            max_body_len,
        }
    }

    /// Encode a complete frame. Generates a fresh random nonce for every encrypted
    /// body. The buffer must not be sent unless this function returns success.
    pub fn encode(
        &self,
        metadata: Metadata,
        encrypted: bool,
        body: &[u8],
        output: &mut Vec<u8>,
    ) -> io::Result<()> {
        if metadata.sequence > MAX_SEQUENCE {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "KMS sequence exceeds 48 bits",
            ));
        }
        let overhead = if encrypted { ENCRYPTION_OVERHEAD } else { 0 };
        let wire_len = body
            .len()
            .checked_add(overhead)
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| *n <= self.max_body_len)
            .ok_or_else(|| {
                io::Error::new(ErrorKind::InvalidInput, "KMS body exceeds configured limit")
            })?;
        let total = HEADER_LEN
            .checked_add(wire_len as usize)
            .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "KMS frame length overflow"))?;
        output.clear();
        output.reserve(total);
        output.extend_from_slice(&[0xab, 0x12, 1, metadata.event_type]);
        output.extend_from_slice(&u16::from(encrypted).to_be_bytes());
        output.extend_from_slice(&wire_len.to_be_bytes());
        output.extend_from_slice(&metadata.sequence.to_be_bytes()[2..]);
        output.extend_from_slice(&metadata.timestamp_ms.to_be_bytes());
        if encrypted {
            let mut nonce = [0u8; NONCE_LEN];
            UnwrapErr(SysRng).fill_bytes(&mut nonce);
            output.extend_from_slice(&nonce);
            output.extend_from_slice(body);
            let tag = self
                .cipher
                .encrypt_inout_detached(
                    &Nonce::from(nonce),
                    &[],
                    output[HEADER_LEN + NONCE_LEN..].as_mut().into(),
                )
                .map_err(|_| invalid("KMS encryption failed"))?;
            output.extend_from_slice(&tag);
        } else {
            output.extend_from_slice(body);
        }
        Ok(())
    }

    fn parse_header(&self, bytes: &[u8]) -> io::Result<Header> {
        if bytes.len() < HEADER_LEN {
            return Err(io::Error::new(
                ErrorKind::UnexpectedEof,
                "Truncated KMS header",
            ));
        }
        if bytes[..2] != [0xab, 0x12] || bytes[2] != 1 {
            return Err(invalid("Unsupported KMS magic or version"));
        }
        let encrypted = match u16::from_be_bytes(bytes[4..6].try_into().unwrap()) {
            0 => false,
            1 => true,
            _ => return Err(invalid("Unsupported KMS encryption flag")),
        };
        let body_len = u32::from_be_bytes(bytes[6..10].try_into().unwrap());
        if body_len > self.max_body_len {
            return Err(invalid("KMS body exceeds configured limit"));
        }
        if encrypted && body_len < ENCRYPTION_OVERHEAD as u32 {
            return Err(invalid("KMS encrypted body lacks nonce or tag"));
        }
        let mut sequence = [0u8; 8];
        sequence[2..].copy_from_slice(&bytes[10..16]);
        Ok(Header {
            metadata: Metadata {
                event_type: bytes[3],
                sequence: u64::from_be_bytes(sequence),
                timestamp_ms: u64::from_be_bytes(bytes[16..24].try_into().unwrap()),
            },
            encrypted,
            body_len,
        })
    }

    /// Decode exactly one frame in place. On authentication failure, no frame is
    /// returned and the unauthenticated payload is cleared. Do not decode twice.
    pub fn decode<'a>(&self, bytes: &'a mut [u8]) -> io::Result<Frame<'a>> {
        let header = self.parse_header(bytes)?;
        self.decode_body(header, bytes)
    }

    fn decode_body<'a>(&self, header: Header, bytes: &'a mut [u8]) -> io::Result<Frame<'a>> {
        let total = HEADER_LEN
            .checked_add(header.body_len as usize)
            .ok_or_else(|| invalid("KMS frame length overflow"))?;
        if bytes.len() != total {
            return Err(invalid("KMS body length does not match frame"));
        }
        let body = &mut bytes[HEADER_LEN..];
        let plaintext = if header.encrypted {
            let nonce: [u8; NONCE_LEN] = body[..NONCE_LEN].try_into().unwrap();
            let tag_start = body.len() - TAG_LEN;
            let tag: [u8; TAG_LEN] = body[tag_start..].try_into().unwrap();
            let payload = &mut body[NONCE_LEN..tag_start];
            if self
                .cipher
                .decrypt_inout_detached(
                    &Nonce::from(nonce),
                    &[],
                    payload.as_mut().into(),
                    &Tag::from(tag),
                )
                .is_err()
            {
                payload.fill(0);
                return Err(invalid("KMS authentication failed"));
            }
            payload
        } else {
            body
        };
        Ok(Frame {
            header,
            body: plaintext,
        })
    }

    /// Read exactly one frame, tolerating TCP fragmentation and coalescing.
    /// Returns `None` only for EOF between frames. Partial EOF is an error.
    /// Any error (including cancellation mid-read) requires closing the stream;
    /// do not attempt magic-byte resynchronization on untrusted ciphertext.
    pub async fn read<'a, R: AsyncRead + Unpin>(
        &self,
        reader: &mut R,
        buffer: &'a mut Vec<u8>,
    ) -> io::Result<Option<Frame<'a>>> {
        buffer.clear();
        let mut header_bytes = [0u8; HEADER_LEN];
        if reader.read(&mut header_bytes[..1]).await? == 0 {
            return Ok(None);
        }
        reader.read_exact(&mut header_bytes[1..]).await?;
        let header = self.parse_header(&header_bytes)?;
        let total = HEADER_LEN
            .checked_add(header.body_len as usize)
            .ok_or_else(|| invalid("KMS frame length overflow"))?;
        buffer.resize(total, 0);
        buffer[..HEADER_LEN].copy_from_slice(&header_bytes);
        reader.read_exact(&mut buffer[HEADER_LEN..]).await?;
        self.decode_body(header, buffer).map(Some)
    }
}

#[cfg(test)]
mod tests;
