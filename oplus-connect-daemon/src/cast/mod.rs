//! Official ODMF signaling framing. Media negotiation is owned by the caller.
//! TCP payloads have a u16 big-endian length; encrypted payloads carry version,
//! timestamp AAD, nonce, ciphertext and a detached GCM tag.
pub mod auth;
pub mod control;
pub mod server;

use aes_gcm::{
    Aes256Gcm, Nonce, Tag,
    aead::{AeadInOut, KeyInit},
};
use getrandom::SysRng;
use rand_core::{Rng, UnwrapErr};
use std::{
    io,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const PREFIX_LEN: usize = 26;
const TAG_LEN: usize = 16;
const MAX_PAYLOAD: usize = u16::MAX as usize;

pub(crate) fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

pub(crate) fn timestamp() -> io::Result<String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| invalid("ODMF clock precedes Unix epoch"))?
        .as_millis()
        .to_string();
    validate_timestamp(now.as_bytes())?;
    Ok(now)
}

fn validate_timestamp(value: &[u8]) -> io::Result<()> {
    if value.len() != 13 || !value.iter().all(u8::is_ascii_digit) {
        return Err(invalid("ODMF timestamp must contain 13 decimal digits"));
    }
    Ok(())
}

/// Reuses the caller's buffers. Does not retain messages or expose key material.
pub struct Codec {
    cipher: Aes256Gcm,
}

impl Codec {
    pub fn new(key: &[u8; 32]) -> Self {
        Self {
            cipher: Aes256Gcm::new_from_slice(key).expect("32-byte AES-256 key"),
        }
    }

    /// Produces the complete length-prefixed frame, using a fresh nonce.
    pub fn encode(&self, body: &[u8], output: &mut Vec<u8>) -> io::Result<()> {
        let len = body
            .len()
            .checked_add(PREFIX_LEN + TAG_LEN)
            .filter(|len| *len <= MAX_PAYLOAD)
            .ok_or_else(|| invalid("ODMF payload exceeds u16 framing limit"))?;
        let aad = timestamp()?;
        let mut nonce = [0u8; 12];
        UnwrapErr(SysRng).fill_bytes(&mut nonce);
        output.clear();
        output.reserve(2 + len);
        output.extend_from_slice(&(len as u16).to_be_bytes());
        output.push(1);
        output.extend_from_slice(aad.as_bytes());
        output.extend_from_slice(&nonce);
        output.extend_from_slice(body);
        let tag = self
            .cipher
            .encrypt_inout_detached(
                &Nonce::from(nonce),
                aad.as_bytes(),
                output[2 + PREFIX_LEN..].as_mut().into(),
            )
            .map_err(|_| invalid("ODMF encryption failed"))?;
        output.extend_from_slice(&tag);
        Ok(())
    }

    /// Authenticate and decrypt one payload (without the two-byte length).
    /// On tag failure clear plaintext-in-progress before returning an error.
    pub fn decode<'a>(&self, payload: &'a mut [u8]) -> io::Result<&'a [u8]> {
        if payload.len() < PREFIX_LEN + TAG_LEN || payload.len() > MAX_PAYLOAD || payload[0] != 1 {
            return Err(invalid("Invalid ODMF encrypted frame"));
        }
        validate_timestamp(&payload[1..14])?;
        let tag_start = payload.len() - TAG_LEN;
        let tag = Tag::from(<[u8; 16]>::try_from(&payload[tag_start..]).unwrap());
        let (prefix, rest) = payload.split_at_mut(PREFIX_LEN);
        let nonce = Nonce::from(<[u8; 12]>::try_from(&prefix[14..26]).unwrap());
        let body = &mut rest[..tag_start - PREFIX_LEN];
        if self
            .cipher
            .decrypt_inout_detached(&nonce, &prefix[1..14], body.as_mut().into(), &tag)
            .is_err()
        {
            body.fill(0);
            return Err(invalid("ODMF authentication failed"));
        }
        Ok(body)
    }
}

/// A pending read must be pinned across concurrent writes, or canceled only
/// when closing the stream. Zero-length padding frames are consumed in-place.
pub async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
    buffer: &mut Vec<u8>,
) -> io::Result<bool> {
    loop {
        let mut length = [0u8; 2];
        if reader.read(&mut length[..1]).await? == 0 {
            return Ok(false);
        }
        reader.read_exact(&mut length[1..]).await?;
        let len = u16::from_be_bytes(length) as usize;
        if len == 0 {
            continue;
        }
        buffer.resize(len, 0);
        reader.read_exact(buffer).await?;
        return Ok(true);
    }
}

pub async fn write_plain<W: AsyncWrite + Unpin>(
    writer: &mut W,
    body: &serde_json::Value,
) -> io::Result<()> {
    let bytes = serde_json::to_vec(body).map_err(|_| invalid("Invalid ODMF JSON"))?;
    let len = u16::try_from(bytes.len()).map_err(|_| invalid("ODMF JSON exceeds framing limit"))?;
    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(&bytes).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authenticates_payload_and_timestamp_and_rejects_other_session() {
        let codec = Codec::new(&[7; 32]);
        let mut frame = Vec::new();
        codec
            .encode(br#"{"cmd":"PairRequest"}"#, &mut frame)
            .unwrap();
        let mut changed = frame[2..].to_vec();
        changed[13] = if changed[13] == b'0' { b'1' } else { b'0' };
        assert!(codec.decode(&mut changed).is_err());
        assert!(
            changed[PREFIX_LEN..changed.len() - TAG_LEN]
                .iter()
                .all(|b| *b == 0)
        );
        let mut wrong_session = frame[2..].to_vec();
        assert!(Codec::new(&[8; 32]).decode(&mut wrong_session).is_err());
        assert_eq!(
            codec.decode(&mut frame[2..]).unwrap(),
            br#"{"cmd":"PairRequest"}"#
        );
    }

    #[tokio::test]
    async fn distinguishes_clean_eof_from_truncated_frame() {
        let mut buffer = Vec::new();
        assert!(!read_frame(&mut &b""[..], &mut buffer).await.unwrap());
        assert_eq!(
            read_frame(&mut &b"\0"[..], &mut buffer)
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert_eq!(
            read_frame(&mut &b"\0\x03ab"[..], &mut buffer)
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
        assert!(
            read_frame(&mut &b"\0\0\0\x02{}"[..], &mut buffer)
                .await
                .unwrap()
        );
        assert_eq!(buffer, b"{}");
    }
}
