//! UKEY2 responder messages carried by SCP authentication envelopes.
//!
//! The phone's ClientInit is captured before user approval. The responder
//! negotiates P-256 and a next-protocol token offered by that ClientInit.

use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, KeyInit},
};
use getrandom::SysRng;
use hkdf::Hkdf;
use p256::{PublicKey, SecretKey, ecdh::diffie_hellman, elliptic_curve::sec1::ToSec1Point};
use rand_core::{Rng, UnwrapErr};
use sha2::{Digest, Sha256, Sha512};
use x25519_dalek::{EphemeralSecret, PublicKey as X25519PublicKey};

use super::codec::{read_varint, write_len_delimited, write_varint_field};

const CURVE25519_SHA512: u64 = 200;
const P256_SHA512: u64 = 100;
const NEXT_PROTOCOL_GCM_SIV: &[u8] = b"AES_256_GCM_SIV";
const NEXT_PROTOCOL_CBC_HMAC: &[u8] = b"AES_256_CBC-HMAC_SHA256";
// The phone's SCP server only accepts CBC-HMAC as the client next-protocol.
const CLIENT_NEXT_PROTOCOL: &[u8] = NEXT_PROTOCOL_CBC_HMAC;
const UKEY2_CLIENT_INIT: u64 = 2;
const UKEY2_SERVER_INIT: u64 = 3;
const UKEY2_CLIENT_FINISH: u64 = 4;
const UKEY2_VERSION: u64 = 1;
const EC_P256: u64 = 1;

/// Per-connection UKEY2 initiator state.
pub struct ClientHandshake {
    x25519_secret: Option<EphemeralSecret>,
    p256_secret: Option<SecretKey>,
    x25519_client_finished: Vec<u8>,
    p256_client_finished: Vec<u8>,
    shared_secret: Option<[u8; 32]>,
}

impl ClientHandshake {
    /// Creates fresh X25519 and P-256 initiator keys and their ClientInit.
    pub fn start() -> std::io::Result<(Self, Vec<u8>)> {
        let mut rng = UnwrapErr(SysRng);
        let x25519_secret = EphemeralSecret::random_from_rng(&mut rng);
        let x25519_public = X25519PublicKey::from(&x25519_secret).to_bytes();
        let p256_secret = loop {
            let mut scalar = [0u8; 32];
            rng.fill_bytes(&mut scalar);
            if let Ok(secret) = SecretKey::from_slice(&scalar) {
                break secret;
            }
        };
        let mut random = [0u8; 32];
        rng.fill_bytes(&mut random);

        let (client_init, x25519_client_finished, p256_client_finished) =
            encode_client_init(&x25519_public, &p256_secret, &random);
        Ok((
            Self {
                x25519_secret: Some(x25519_secret),
                p256_secret: Some(p256_secret),
                x25519_client_finished,
                p256_client_finished,
                shared_secret: None,
            },
            client_init,
        ))
    }

    /// Validates the peer ServerInit, derives the selected ECDH secret, and
    /// returns the exact ClientFinished message committed in ClientInit.
    pub fn handle_server_init(&mut self, message: &[u8]) -> std::io::Result<Vec<u8>> {
        if self.shared_secret.is_some() {
            return Err(invalid_data("ServerInit received after UKEY2 completion"));
        }

        let (message_type, message_data) = decode_ukey2_message(message)?;
        if message_type != UKEY2_SERVER_INIT {
            return Err(invalid_data("expected UKEY2 ServerInit"));
        }
        let server_init = fields(&message_data)?;
        if varint_field(&server_init, 1)? != UKEY2_VERSION {
            return Err(invalid_data("unsupported UKEY2 version"));
        }
        if bytes_field(&server_init, 2)?.len() != 32 {
            return Err(invalid_data("UKEY2 ServerInit random must be 32 bytes"));
        }

        let cipher = varint_field(&server_init, 3)?;
        let server_public_key = bytes_field(&server_init, 4)?;
        let (shared_secret, client_finished) = match cipher {
            CURVE25519_SHA512 => {
                let peer_bytes: [u8; 32] = server_public_key
                    .try_into()
                    .map_err(|_| invalid_data("X25519 public key must be 32 bytes"))?;
                let peer_key = X25519PublicKey::from(peer_bytes);
                let secret = self
                    .x25519_secret
                    .take()
                    .ok_or_else(|| invalid_data("X25519 handshake key was already used"))?;
                let raw_secret = secret.diffie_hellman(&peer_key);
                if !raw_secret.was_contributory() {
                    return Err(invalid_data("non-contributory X25519 public key"));
                }
                (
                    Sha256::digest(raw_secret.as_bytes()).into(),
                    self.x25519_client_finished.clone(),
                )
            }
            P256_SHA512 => {
                let peer_key = decode_p256_public_key(server_public_key)?;
                let secret = self
                    .p256_secret
                    .take()
                    .ok_or_else(|| invalid_data("P-256 handshake key was already used"))?;
                let private_scalar = secret.to_nonzero_scalar();
                let raw_secret = diffie_hellman(&private_scalar, peer_key.as_affine());
                (
                    Sha256::digest(raw_secret.raw_secret_bytes()).into(),
                    self.p256_client_finished.clone(),
                )
            }
            _ => return Err(invalid_data("UKEY2 server selected an unsupported cipher")),
        };
        self.shared_secret = Some(shared_secret);
        Ok(client_finished)
    }

    /// SHA-256(ECDH), required by UKEY2's selected handshake cipher.
    pub fn shared_secret(&self) -> Option<&[u8; 32]> {
        self.shared_secret.as_ref()
    }
}

/// Per-connection UKEY2 responder state. The phone is the UKEY2 client on the
/// observed LAN path: it sends an outer `Ukey2Finished` envelope containing an
/// inner `ClientInit` before the desktop approval dialog is shown.
pub struct ServerHandshake {
    p256_secret: Option<SecretKey>,
    selected_cipher: Option<u64>,
    client_commitment: Option<Vec<u8>>,
    client_init: Vec<u8>,
    server_init: Vec<u8>,
    secure_channel: Option<SecureChannel>,
}

impl ServerHandshake {
    pub fn new() -> Self {
        Self {
            p256_secret: None,
            selected_cipher: None,
            client_commitment: None,
            client_init: Vec::new(),
            server_init: Vec::new(),
            secure_channel: None,
        }
    }

    /// Consume the peer's inner UKEY2 ClientInit and build ServerInit.
    pub fn handle_client_init(&mut self, message: &[u8]) -> std::io::Result<Vec<u8>> {
        if self.selected_cipher.is_some() {
            return Err(invalid_data("UKEY2 ClientInit received twice"));
        }
        let (message_type, message_data) = decode_ukey2_message(message)?;
        if message_type != UKEY2_CLIENT_INIT {
            return Err(invalid_data("expected UKEY2 ClientInit"));
        }
        let init = fields(&message_data)?;
        if varint_field(&init, 1)? != UKEY2_VERSION {
            return Err(invalid_data("unsupported UKEY2 version"));
        }
        if bytes_field(&init, 2)?.len() != 32 {
            return Err(invalid_data("UKEY2 ClientInit random must be 32 bytes"));
        }

        let mut p256_commitment = None;
        for (_, wire, encoded) in init.iter().filter(|(field, _, _)| *field == 3) {
            if *wire != 2 {
                return Err(invalid_data("invalid UKEY2 cipher commitment"));
            }
            let commitment = fields(encoded)?;
            if varint_field(&commitment, 1)? == P256_SHA512 {
                p256_commitment = Some(bytes_field(&commitment, 2)?.to_vec());
            }
        }
        let p256_commitment =
            p256_commitment.ok_or_else(|| invalid_data("phone did not offer P-256 SHA-512"))?;
        let offered_protocol = bytes_field(&init, 4)?;
        let _selected_protocol = if offered_protocol == NEXT_PROTOCOL_CBC_HMAC {
            NEXT_PROTOCOL_CBC_HMAC
        } else if offered_protocol == NEXT_PROTOCOL_GCM_SIV {
            NEXT_PROTOCOL_GCM_SIV
        } else {
            return Err(invalid_data(
                "phone offered an unsupported UKEY2 next protocol",
            ));
        };

        let mut rng = UnwrapErr(SysRng);
        let p256_secret = loop {
            let mut scalar = [0u8; 32];
            rng.fill_bytes(&mut scalar);
            if let Ok(secret) = SecretKey::from_slice(&scalar) {
                break secret;
            }
        };
        let mut random = [0u8; 32];
        rng.fill_bytes(&mut random);

        let mut server_init = Vec::with_capacity(192);
        write_varint_field(&mut server_init, 1, UKEY2_VERSION);
        write_len_delimited(&mut server_init, 2, &random);
        write_varint_field(&mut server_init, 3, P256_SHA512);
        write_len_delimited(&mut server_init, 4, &p256_server_public_key(&p256_secret));
        self.p256_secret = Some(p256_secret);
        self.selected_cipher = Some(P256_SHA512);
        self.client_commitment = Some(p256_commitment);
        let server_init = ukey2_message(UKEY2_SERVER_INIT, &server_init);
        self.client_init = message.to_vec();
        self.server_init = server_init.clone();
        Ok(server_init)
    }

    /// Verify the peer's inner ClientFinished and derive the shared secret.
    pub fn handle_client_finished(&mut self, message: &[u8]) -> std::io::Result<[u8; 32]> {
        if self.selected_cipher != Some(P256_SHA512) {
            return Err(invalid_data("UKEY2 ServerInit was not sent"));
        }
        let (message_type, message_data) = decode_ukey2_message(message)?;
        if message_type != UKEY2_CLIENT_FINISH {
            return Err(invalid_data("expected UKEY2 ClientFinished"));
        }
        let finished = fields(&message_data)?;
        let client_public_key = bytes_field(&finished, 1)?;
        let client_nonce: [u8; 16] = bytes_field(&finished, 2)?
            .try_into()
            .map_err(|_| invalid_data("UKEY2 ClientFinished nonce must be 16 bytes"))?;
        let commitment = Sha512::digest(message);
        if self.client_commitment.as_deref() != Some(&commitment[..]) {
            return Err(invalid_data("UKEY2 ClientFinished commitment mismatch"));
        }
        let peer_key = decode_p256_public_key(client_public_key)?;
        let secret = self
            .p256_secret
            .take()
            .ok_or_else(|| invalid_data("UKEY2 server key was already used"))?;
        let raw_secret = diffie_hellman(&secret.to_nonzero_scalar(), peer_key.as_affine());
        let shared_secret: [u8; 32] = Sha256::digest(raw_secret.raw_secret_bytes()).into();
        let channel = SecureChannel::derive(
            &shared_secret,
            &self.client_init,
            &self.server_init,
            &client_nonce,
        )?;
        self.secure_channel = Some(channel);
        Ok(shared_secret)
    }

    pub fn secure_channel(&self) -> Option<&SecureChannel> {
        self.secure_channel.as_ref()
    }
}

/// The SCP peer uses one AES-256-GCM key and the ClientFinished nonce in both directions.
pub struct SecureChannel {
    key: [u8; 32],
    nonce: [u8; 12],
}

impl SecureChannel {
    pub fn derive(
        shared: &[u8; 32],
        client_init: &[u8],
        server_init: &[u8],
        client_nonce: &[u8; 16],
    ) -> std::io::Result<Self> {
        let mut transcript = Vec::with_capacity(client_init.len() + server_init.len());
        transcript.extend_from_slice(client_init);
        transcript.extend_from_slice(server_init);
        let mut next = [0; 32];
        Hkdf::<Sha256>::new(Some(b"UKEY2 v1 next"), shared)
            .expand(&transcript, &mut next)
            .map_err(|_| invalid_data("UKEY2 next key derivation failed"))?;
        let mut key = [0; 32];
        Hkdf::<Sha256>::new(Some(&Sha256::digest(b"D2D")), &next)
            .expand(b"client", &mut key)
            .map_err(|_| invalid_data("SCP key derivation failed"))?;
        let mut nonce = [0; 12];
        nonce.copy_from_slice(&client_nonce[..12]);
        Ok(Self { key, nonce })
    }

    pub fn encrypt(&self, plaintext: &[u8]) -> std::io::Result<Vec<u8>> {
        Aes256Gcm::new_from_slice(&self.key)
            .expect("AES-256 key length")
            .encrypt(&Nonce::from(self.nonce), plaintext)
            .map_err(|_| invalid_data("SCP encryption failed"))
    }

    pub fn decrypt(&self, ciphertext: &[u8]) -> std::io::Result<Vec<u8>> {
        Aes256Gcm::new_from_slice(&self.key)
            .expect("AES-256 key length")
            .decrypt(&Nonce::from(self.nonce), ciphertext)
            .map_err(|_| invalid_data("SCP authentication failed"))
    }
}

fn p256_server_public_key(secret: &SecretKey) -> Vec<u8> {
    let point = secret.public_key().to_sec1_point(false);
    let x = positive_integer(point.x().expect("P-256 server public key x"));
    let y = positive_integer(point.y().expect("P-256 server public key y"));
    let mut ec_key = Vec::with_capacity(x.len() + y.len() + 4);
    write_len_delimited(&mut ec_key, 1, &x);
    write_len_delimited(&mut ec_key, 2, &y);
    let mut generic = Vec::with_capacity(ec_key.len() + 4);
    write_varint_field(&mut generic, 1, EC_P256);
    write_len_delimited(&mut generic, 2, &ec_key);
    generic
}

fn encode_client_init(
    x25519_public: &[u8; 32],
    p256_secret: &SecretKey,
    random: &[u8; 32],
) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let x25519_client_finished = client_finished_message(x25519_public);
    let p256_client_finished = p256_client_finished(p256_secret);
    let x25519_commitment = Sha512::digest(&x25519_client_finished);
    let p256_commitment = Sha512::digest(&p256_client_finished);

    let mut client_init_data = Vec::with_capacity(192);
    write_varint_field(&mut client_init_data, 1, UKEY2_VERSION);
    write_len_delimited(&mut client_init_data, 2, random);
    write_cipher_commitment(&mut client_init_data, CURVE25519_SHA512, &x25519_commitment);
    write_cipher_commitment(&mut client_init_data, P256_SHA512, &p256_commitment);
    write_len_delimited(&mut client_init_data, 4, CLIENT_NEXT_PROTOCOL);

    (
        ukey2_message(UKEY2_CLIENT_INIT, &client_init_data),
        x25519_client_finished,
        p256_client_finished,
    )
}

fn p256_client_finished(secret: &SecretKey) -> Vec<u8> {
    let public_key = secret.public_key().to_sec1_point(false);
    let x = positive_integer(public_key.x().expect("uncompressed P-256 key has x"));
    let y = positive_integer(public_key.y().expect("uncompressed P-256 key has y"));

    let mut ec_key = Vec::with_capacity(x.len() + y.len() + 4);
    write_len_delimited(&mut ec_key, 1, &x);
    write_len_delimited(&mut ec_key, 2, &y);
    let mut generic_key = Vec::with_capacity(ec_key.len() + 4);
    write_varint_field(&mut generic_key, 1, EC_P256);
    write_len_delimited(&mut generic_key, 2, &ec_key);

    client_finished_message(&generic_key)
}

fn client_finished_message(public_key: &[u8]) -> Vec<u8> {
    let mut finished_data = Vec::with_capacity(public_key.len() + 4);
    write_len_delimited(&mut finished_data, 1, public_key);
    write_len_delimited(&mut finished_data, 2, &[0x42; 16]);
    ukey2_message(UKEY2_CLIENT_FINISH, &finished_data)
}

fn write_cipher_commitment(out: &mut Vec<u8>, cipher: u64, commitment: &[u8]) {
    let mut encoded = Vec::with_capacity(commitment.len() + 8);
    write_varint_field(&mut encoded, 1, cipher);
    write_len_delimited(&mut encoded, 2, commitment);
    write_len_delimited(out, 3, &encoded);
}

fn decode_ukey2_message(message: &[u8]) -> std::io::Result<(u64, Vec<u8>)> {
    let message_fields = fields(message)?;
    Ok((
        varint_field(&message_fields, 1)?,
        bytes_field(&message_fields, 2)?.to_vec(),
    ))
}

pub fn message_type(message: &[u8]) -> std::io::Result<u64> {
    let (message_type, _) = decode_ukey2_message(message)?;
    Ok(message_type)
}

fn decode_p256_public_key(serialized: &[u8]) -> std::io::Result<PublicKey> {
    let generic = fields(serialized)?;
    if varint_field(&generic, 1)? != EC_P256 {
        return Err(invalid_data("UKEY2 public key is not EC_P256"));
    }
    let ec_key = fields(bytes_field(&generic, 2)?)?;
    let x = coordinate(bytes_field(&ec_key, 1)?)?;
    let y = coordinate(bytes_field(&ec_key, 2)?)?;

    let mut sec1 = [0u8; 65];
    sec1[0] = 4;
    sec1[1..33].copy_from_slice(&x);
    sec1[33..].copy_from_slice(&y);
    PublicKey::from_sec1_bytes(&sec1).map_err(|_| invalid_data("invalid P-256 public point"))
}

fn coordinate(encoded: &[u8]) -> std::io::Result<[u8; 32]> {
    let significant = if encoded.first() == Some(&0) {
        &encoded[1..]
    } else {
        encoded
    };
    if significant.len() > 32 {
        return Err(invalid_data("P-256 coordinate exceeds 32 bytes"));
    }
    let mut coordinate = [0u8; 32];
    coordinate[32 - significant.len()..].copy_from_slice(significant);
    Ok(coordinate)
}

fn positive_integer(coordinate: &[u8]) -> Vec<u8> {
    let significant = coordinate
        .iter()
        .position(|byte| *byte != 0)
        .map_or(&coordinate[coordinate.len() - 1..], |start| {
            &coordinate[start..]
        });
    let mut encoded = Vec::with_capacity(significant.len() + 1);
    encoded.push(0);
    encoded.extend_from_slice(significant);
    encoded
}

fn ukey2_message(message_type: u64, data: &[u8]) -> Vec<u8> {
    let mut message = Vec::with_capacity(data.len() + 8);
    write_varint_field(&mut message, 1, message_type);
    write_len_delimited(&mut message, 2, data);
    message
}

fn fields(mut bytes: &[u8]) -> std::io::Result<Vec<(u32, u8, Vec<u8>)>> {
    let mut result = Vec::new();
    while !bytes.is_empty() {
        let key = read_varint(&mut bytes).ok_or_else(|| invalid_data("truncated protobuf key"))?;
        let field = (key >> 3) as u32;
        let wire = (key & 7) as u8;
        if field == 0 {
            return Err(invalid_data("invalid protobuf field number"));
        }
        let value = match wire {
            0 => read_varint(&mut bytes)
                .ok_or_else(|| invalid_data("truncated protobuf varint"))?
                .to_le_bytes()
                .to_vec(),
            1 => take(&mut bytes, 8)?.to_vec(),
            2 => {
                let len = read_varint(&mut bytes)
                    .ok_or_else(|| invalid_data("truncated protobuf length"))?
                    as usize;
                take(&mut bytes, len)?.to_vec()
            }
            5 => take(&mut bytes, 4)?.to_vec(),
            _ => return Err(invalid_data("unsupported protobuf wire type")),
        };
        result.push((field, wire, value));
    }
    Ok(result)
}

fn take<'a>(bytes: &mut &'a [u8], len: usize) -> std::io::Result<&'a [u8]> {
    if bytes.len() < len {
        return Err(invalid_data("truncated protobuf field"));
    }
    let (value, rest) = bytes.split_at(len);
    *bytes = rest;
    Ok(value)
}

fn varint_field(fields: &[(u32, u8, Vec<u8>)], field: u32) -> std::io::Result<u64> {
    let value = fields
        .iter()
        .find(|(number, wire, _)| *number == field && *wire == 0)
        .ok_or_else(|| invalid_data("missing UKEY2 varint field"))?;
    let bytes: [u8; 8] = value
        .2
        .as_slice()
        .try_into()
        .map_err(|_| invalid_data("invalid UKEY2 varint field"))?;
    Ok(u64::from_le_bytes(bytes))
}

fn bytes_field(fields: &[(u32, u8, Vec<u8>)], field: u32) -> std::io::Result<&[u8]> {
    fields
        .iter()
        .find(|(number, wire, _)| *number == field && *wire == 2)
        .map(|(_, _, value)| value.as_slice())
        .ok_or_else(|| invalid_data("missing UKEY2 bytes field"))
}

fn invalid_data(message: &'static str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_expected_channel_key_and_authenticates_round_trip() {
        let shared = std::array::from_fn(|i| i as u8);
        let nonce = std::array::from_fn(|i| i as u8);
        let channel =
            SecureChannel::derive(&shared, b"client-init", b"server-init", &nonce).unwrap();
        assert_eq!(
            channel.key,
            [
                0x49, 0x85, 0xf1, 0x9d, 0x9f, 0xf2, 0xfb, 0xc3, 0x64, 0x52, 0xb9, 0x1e, 0x2a, 0x71,
                0x5a, 0xce, 0xaa, 0x47, 0xa1, 0xa7, 0x90, 0x9b, 0x5d, 0xee, 0x22, 0x8f, 0x48, 0xc9,
                0xf5, 0x08, 0xb3, 0x55,
            ]
        );
        assert_eq!(channel.nonce, nonce[..12]);
        let plaintext = b"serialized ScpWrapData";
        let mut ciphertext = channel.encrypt(plaintext).unwrap();
        assert_eq!(ciphertext.len(), plaintext.len() + 16);
        assert_eq!(channel.decrypt(&ciphertext).unwrap(), plaintext);
        ciphertext[0] ^= 1;
        assert!(channel.decrypt(&ciphertext).is_err());
    }

    fn p256_server_public_key(secret: &SecretKey) -> Vec<u8> {
        let point = secret.public_key().to_sec1_point(false);
        let x = positive_integer(point.x().expect("server x"));
        let y = positive_integer(point.y().expect("server y"));
        let mut ec_key = Vec::new();
        write_len_delimited(&mut ec_key, 1, &x);
        write_len_delimited(&mut ec_key, 2, &y);
        let mut generic = Vec::new();
        write_varint_field(&mut generic, 1, EC_P256);
        write_len_delimited(&mut generic, 2, &ec_key);
        generic
    }

    fn server_init(cipher: u64, public_key: &[u8], version: u64) -> Vec<u8> {
        let mut init = Vec::new();
        write_varint_field(&mut init, 1, version);
        write_len_delimited(&mut init, 2, &[0x5A; 32]);
        write_varint_field(&mut init, 3, cipher);
        write_len_delimited(&mut init, 4, public_key);
        ukey2_message(UKEY2_SERVER_INIT, &init)
    }

    fn use_next_protocol(client_init: &[u8], next_protocol: &[u8]) -> Vec<u8> {
        let envelope = fields(client_init).unwrap();
        let init = fields(bytes_field(&envelope, 2).unwrap()).unwrap();
        let mut data = Vec::new();
        for (field, wire, value) in init.iter().filter(|(field, _, _)| *field != 4) {
            match wire {
                0 => write_varint_field(
                    &mut data,
                    *field,
                    u64::from_le_bytes(value.as_slice().try_into().unwrap()),
                ),
                2 => write_len_delimited(&mut data, *field, value),
                _ => panic!("unsupported test field wire type"),
            }
        }
        write_len_delimited(&mut data, 4, next_protocol);
        ukey2_message(UKEY2_CLIENT_INIT, &data)
    }

    #[test]
    fn server_handshake_selects_p256_verifies_commitment_and_derives_secret() {
        let (mut client, init) = ClientHandshake::start().unwrap();
        let client_init = use_next_protocol(&init, NEXT_PROTOCOL_CBC_HMAC);
        let mut server = ServerHandshake::new();
        let server_init = server.handle_client_init(&client_init).unwrap();
        assert_eq!(message_type(&server_init).unwrap(), UKEY2_SERVER_INIT);
        let server_init_fields =
            fields(bytes_field(&fields(&server_init).unwrap(), 2).unwrap()).unwrap();
        assert!(server_init_fields.iter().all(|(field, _, _)| *field <= 4));

        let client_finished = client.p256_client_finished.clone();
        assert_eq!(message_type(&client_finished).unwrap(), UKEY2_CLIENT_FINISH);
        client.handle_server_init(&server_init).unwrap();
        let server_secret = server.handle_client_finished(&client_finished).unwrap();
        assert_eq!(client.shared_secret().unwrap(), &server_secret);
    }

    #[test]
    fn client_init_offers_both_official_cipher_commitments() {
        let (client, client_init) = ClientHandshake::start().unwrap();
        let envelope = fields(&client_init).expect("ClientInit envelope");
        assert_eq!(varint_field(&envelope, 1).unwrap(), UKEY2_CLIENT_INIT);
        let init = fields(bytes_field(&envelope, 2).unwrap()).expect("ClientInit data");
        assert_eq!(varint_field(&init, 1).unwrap(), UKEY2_VERSION);
        assert_eq!(bytes_field(&init, 2).unwrap().len(), 32);
        assert_eq!(bytes_field(&init, 4).unwrap(), CLIENT_NEXT_PROTOCOL);

        let commitments = init
            .iter()
            .filter(|(field, wire, _)| *field == 3 && *wire == 2)
            .collect::<Vec<_>>();
        assert_eq!(commitments.len(), 2);
        let curve_commitment = fields(&commitments[0].2).unwrap();
        let p256_commitment = fields(&commitments[1].2).unwrap();
        assert_eq!(
            varint_field(&curve_commitment, 1).unwrap(),
            CURVE25519_SHA512
        );
        assert_eq!(varint_field(&p256_commitment, 1).unwrap(), P256_SHA512);
        assert_eq!(bytes_field(&curve_commitment, 2).unwrap().len(), 64);
        assert_eq!(bytes_field(&p256_commitment, 2).unwrap().len(), 64);

        let x25519_client_finished = &client.x25519_client_finished;
        let p256_client_finished = &client.p256_client_finished;
        let x25519_digest: [u8; 64] = Sha512::digest(x25519_client_finished).into();
        let p256_digest: [u8; 64] = Sha512::digest(p256_client_finished).into();
        assert_eq!(bytes_field(&curve_commitment, 2).unwrap(), &x25519_digest);
        assert_eq!(bytes_field(&p256_commitment, 2).unwrap(), &p256_digest);
        assert!(client.shared_secret().is_none());
    }

    #[test]
    fn p256_server_init_returns_committed_finish_and_derives_secret() {
        let (mut client, _) = ClientHandshake::start().unwrap();
        let server_secret = SecretKey::from_slice(&[2u8; 32]).unwrap();
        let server_public_key = p256_server_public_key(&server_secret);
        let private_scalar = client.p256_secret.as_ref().unwrap().to_nonzero_scalar();
        let expected = diffie_hellman(&private_scalar, server_secret.public_key().as_affine());
        let expected_digest: [u8; 32] = Sha256::digest(expected.raw_secret_bytes()).into();
        let finish = client
            .handle_server_init(&server_init(P256_SHA512, &server_public_key, UKEY2_VERSION))
            .expect("valid P-256 ServerInit");
        assert_eq!(finish, client.p256_client_finished);
        assert_eq!(client.shared_secret(), Some(&expected_digest));
    }

    #[test]
    fn x25519_server_init_returns_committed_finish_and_derives_secret() {
        let (mut client, _) = ClientHandshake::start().unwrap();
        let mut rng = UnwrapErr(SysRng);
        let server_secret = EphemeralSecret::random_from_rng(&mut rng);
        let server_public_key = X25519PublicKey::from(&server_secret).to_bytes();
        let finish = client
            .handle_server_init(&server_init(
                CURVE25519_SHA512,
                &server_public_key,
                UKEY2_VERSION,
            ))
            .expect("valid X25519 ServerInit");
        assert_eq!(finish, client.x25519_client_finished);
        let (_, finish_data) = decode_ukey2_message(&finish).unwrap();
        let finish_fields = fields(&finish_data).unwrap();
        let client_public = X25519PublicKey::from(
            <[u8; 32]>::try_from(bytes_field(&finish_fields, 1).unwrap()).unwrap(),
        );
        let raw_secret = server_secret.diffie_hellman(&client_public);
        let expected_digest: [u8; 32] = Sha256::digest(raw_secret.as_bytes()).into();
        assert_eq!(client.shared_secret(), Some(&expected_digest));
    }

    #[test]
    fn server_init_rejects_invalid_version_cipher_key_and_type() {
        let (mut client, _) = ClientHandshake::start().unwrap();
        assert!(
            client
                .handle_server_init(&server_init(CURVE25519_SHA512, &[0; 32], 2))
                .is_err()
        );

        let (mut client, _) = ClientHandshake::start().unwrap();
        assert!(
            client
                .handle_server_init(&server_init(300, &[0; 32], UKEY2_VERSION))
                .is_err()
        );

        let (mut client, _) = ClientHandshake::start().unwrap();
        assert!(
            client
                .handle_server_init(&server_init(CURVE25519_SHA512, &[0; 31], UKEY2_VERSION))
                .is_err()
        );

        let (mut client, _) = ClientHandshake::start().unwrap();
        let wrong_type = ukey2_message(UKEY2_CLIENT_INIT, &[]);
        assert!(client.handle_server_init(&wrong_type).is_err());
    }

    #[test]
    fn p256_coordinate_encoding_accepts_prefixed_and_raw_integers() {
        assert_eq!(positive_integer(&[0, 0, 0x80, 1]), [0, 0x80, 1]);
        assert_eq!(positive_integer(&[0, 0, 0x7f, 1]), [0, 0x7f, 1]);
        assert_eq!(coordinate(&[0, 0x80, 1]).unwrap()[29..], [0, 0x80, 1]);
        assert_eq!(coordinate(&[0x80, 1]).unwrap()[30..], [0x80, 1]);
    }
}
