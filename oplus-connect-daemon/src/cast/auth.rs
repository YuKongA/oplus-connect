//! ODMF near-field authentication, bootstrapped over authenticated Panta.
//! Raw P-256 ECDH bytes are the AES-256-GCM handshake key (no KDF).
//! The peer proves knowledge of the advertised ID; AuthSuccess distributes a
//! fresh transport key. This is not verification of a device certificate.
use super::{Codec, invalid, timestamp};
use aes_gcm::{
    Aes256Gcm, Nonce, Tag,
    aead::{AeadInOut, KeyInit},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use getrandom::SysRng;
use p256::{PublicKey, SecretKey, ecdh::diffie_hellman, elliptic_curve::sec1::ToSec1Point};
use rand_core::{Rng, UnwrapErr};
use serde::Deserialize;
use serde_json::{Value, json};
use std::io;

// DER SubjectPublicKeyInfo: id-ecPublicKey, prime256v1, 65-byte SEC1 point.
const SPKI_PREFIX: &[u8] = &[
    0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a,
    0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
];

pub struct Identity {
    secret: SecretKey,
    id: [u8; 8],
    transport_key: [u8; 32],
}

#[derive(Deserialize)]
struct Proof {
    auth_version: u8,
    public_key: String,
    iv: String,
    timestamp: String,
    encrypt_cipher: String,
}

impl Default for Identity {
    fn default() -> Self {
        Self::new()
    }
}

impl Identity {
    pub fn new() -> Self {
        let mut rng = UnwrapErr(SysRng);
        let secret = loop {
            let mut candidate = [0u8; 32];
            rng.fill_bytes(&mut candidate);
            if let Ok(secret) = SecretKey::from_slice(&candidate) {
                break secret;
            }
        };
        let mut transport_key = [0u8; 32];
        rng.fill_bytes(&mut transport_key);
        // Keep the observed eight ASCII-digit identity shape, but never replay
        // the captured Windows host identity. The ECDH key provides entropy.
        let number = rng.next_u32() % 90_000_000 + 10_000_000;
        let mut id = [0u8; 8];
        id.copy_from_slice(number.to_string().as_bytes());
        Self {
            secret,
            id,
            transport_key,
        }
    }

    pub fn advertisement(&self) -> Value {
        let point = self.secret.public_key().to_sec1_point(false);
        let mut der = Vec::with_capacity(91);
        der.extend_from_slice(SPKI_PREFIX);
        der.extend_from_slice(point.as_bytes());
        // This field is an observed near-field protocol parameter, not wall
        // clock milliseconds. Fresh identity acceptance still needs a device.
        json!({"auth_version":1, "id":STANDARD.encode(self.id),
            "public_key":STANDARD.encode(der), "timestamp":"28703"})
    }

    pub fn codec(&self) -> Codec {
        Codec::new(&self.transport_key)
    }

    /// Accept only a GCM proof of this identity under this ephemeral keypair.
    /// Neither IP address nor the peer-provided model/name is authentication.
    pub fn authenticate(&self, response: &Value) -> io::Result<Value> {
        if response.get("cmd").and_then(Value::as_str) != Some("OdmfAuthResponse") {
            return Err(invalid("Expected ODMF authentication response"));
        }
        let proof: Proof = serde_json::from_str(
            response
                .get("auth_info")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("Missing ODMF authentication info"))?,
        )
        .map_err(|_| invalid("Invalid ODMF authentication info"))?;
        if proof.auth_version != 1 {
            return Err(invalid("Unsupported ODMF auth version"));
        }
        super::validate_timestamp(proof.timestamp.as_bytes())?;
        let der = STANDARD
            .decode(&proof.public_key)
            .map_err(|_| invalid("Invalid ODMF public key encoding"))?;
        if der.len() != 91 || !der.starts_with(SPKI_PREFIX) {
            return Err(invalid("Expected ODMF P-256 SPKI public key"));
        }
        let peer = PublicKey::from_sec1_bytes(&der[SPKI_PREFIX.len()..])
            .map_err(|_| invalid("Invalid ODMF P-256 point"))?;
        let shared = diffie_hellman(&self.secret.to_nonzero_scalar(), peer.as_affine());
        let cipher = Aes256Gcm::new_from_slice(shared.raw_secret_bytes())
            .map_err(|_| invalid("Invalid ODMF shared key"))?;
        let nonce: [u8; 12] = STANDARD
            .decode(&proof.iv)
            .map_err(|_| invalid("Invalid ODMF proof nonce"))?
            .try_into()
            .map_err(|_| invalid("Invalid ODMF proof nonce length"))?;
        let mut encrypted = STANDARD
            .decode(&proof.encrypt_cipher)
            .map_err(|_| invalid("Invalid ODMF proof ciphertext"))?;
        if encrypted.len() != 24 {
            return Err(invalid("Invalid ODMF identity proof length"));
        }
        let tag = Tag::from(<[u8; 16]>::try_from(&encrypted[8..]).unwrap());
        if cipher
            .decrypt_inout_detached(
                &Nonce::from(nonce),
                proof.timestamp.as_bytes(),
                encrypted[..8].as_mut().into(),
                &tag,
            )
            .is_err()
        {
            encrypted.fill(0);
            return Err(invalid("ODMF identity authentication failed"));
        }
        let matches = encrypted[..8]
            .iter()
            .zip(self.id)
            .fold(0u8, |v, (a, b)| v | (a ^ b))
            == 0;
        encrypted.fill(0);
        if !matches {
            return Err(invalid("ODMF identity mismatch"));
        }

        let aad = timestamp()?;
        let mut nonce = [0u8; 12];
        UnwrapErr(SysRng).fill_bytes(&mut nonce);
        let mut payload = serde_json::to_vec(&json!({"id":STANDARD.encode(self.id),
            "key":STANDARD.encode(self.transport_key)}))
        .map_err(|_| invalid("Invalid ODMF key payload"))?;
        let tag = cipher
            .encrypt_inout_detached(
                &Nonce::from(nonce),
                aad.as_bytes(),
                payload.as_mut_slice().into(),
            )
            .map_err(|_| invalid("ODMF key distribution failed"))?;
        payload.extend_from_slice(&tag);
        Ok(
            json!({"auth_version":1,"cmd":"AuthSuccess","iv":STANDARD.encode(nonce),
            "timestamp":aad,"encrypt_cipher":STANDARD.encode(payload)}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proof(identity: &Identity, id: &[u8]) -> Value {
        let peer = SecretKey::from_slice(&[3u8; 32]).unwrap();
        let shared = diffie_hellman(
            &peer.to_nonzero_scalar(),
            identity.secret.public_key().as_affine(),
        );
        let cipher = Aes256Gcm::new_from_slice(shared.raw_secret_bytes()).unwrap();
        let mut body = id.to_vec();
        let nonce = [4u8; 12];
        let aad = "1793630000000";
        let tag = cipher
            .encrypt_inout_detached(
                &Nonce::from(nonce),
                aad.as_bytes(),
                body.as_mut_slice().into(),
            )
            .unwrap();
        body.extend_from_slice(&tag);
        let mut der = SPKI_PREFIX.to_vec();
        der.extend_from_slice(peer.public_key().to_sec1_point(false).as_bytes());
        json!({"cmd":"OdmfAuthResponse", "auth_info":json!({
            "auth_version":1,"public_key":STANDARD.encode(der),"iv":STANDARD.encode(nonce),
            "timestamp":aad,"encrypt_cipher":STANDARD.encode(body)
        }).to_string()})
    }

    #[test]
    fn rejects_valid_gcm_proof_for_wrong_advertised_identity() {
        let identity = Identity::new();
        let mut wrong_id = identity.id;
        wrong_id[0] ^= 1;
        assert!(identity.authenticate(&proof(&identity, &wrong_id)).is_err());
        assert_eq!(
            identity
                .authenticate(&proof(&identity, &identity.id))
                .unwrap()["cmd"],
            "AuthSuccess"
        );
    }

    #[test]
    fn rejects_previous_session_proof_and_wrong_curve_encoding() {
        let first = Identity::new();
        let response = proof(&first, &first.id);
        assert!(Identity::new().authenticate(&response).is_err());
        let mut invalid_curve = response;
        let mut inner: Value =
            serde_json::from_str(invalid_curve["auth_info"].as_str().unwrap()).unwrap();
        let mut der = STANDARD
            .decode(inner["public_key"].as_str().unwrap())
            .unwrap();
        der[22] ^= 1;
        inner["public_key"] = STANDARD.encode(der).into();
        invalid_curve["auth_info"] = inner.to_string().into();
        assert!(first.authenticate(&invalid_curve).is_err());
    }
}
