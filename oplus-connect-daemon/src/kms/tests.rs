use super::*;
use tokio::io::AsyncWriteExt;

// Independently generated with Python cryptography AESGCM; no capture secrets.
const FIXTURE: &str = "ab12010300010000005c010203040506000001a19c6b36fe000102030405060708090a0b41a137b404ec1dd51263e05875192253bbcc29a828d95b5da7f8f6966f075b68a744719c432abc8af21806327001e234e17a26bff2a0c2d994c6cb2cb47264cac53ba61b27e36dba68ec39cd5f53db7d";
const BODY: &[u8] = br#"{"keyCode":79,"modifier":0,"platform":2,"pressed":true,"type":3}"#;

fn fixture() -> Vec<u8> {
    (0..FIXTURE.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&FIXTURE[i..i + 2], 16).unwrap())
        .collect()
}

fn metadata() -> Metadata {
    Metadata {
        event_type: 3,
        sequence: 0x010203040506,
        timestamp_ms: 1793625634558,
    }
}

#[test]
fn decrypts_independent_fixture_with_full_sequence_and_timestamp() {
    let codec = Codec::new("kms-test-password", 4096);
    let mut bytes = fixture();
    let frame = codec.decode(&mut bytes).unwrap();
    assert_eq!(frame.header.metadata, metadata());
    assert!(frame.header.encrypted);
    assert_eq!(
        frame.header.body_len,
        (BODY.len() + ENCRYPTION_OVERHEAD) as u32
    );
    assert_eq!(frame.body, BODY);
}

#[test]
fn ciphertext_nonce_and_tag_tampering_are_rejected() {
    let codec = Codec::new("kms-test-password", 4096);
    for index in [HEADER_LEN, HEADER_LEN + NONCE_LEN, fixture().len() - 1] {
        let mut bytes = fixture();
        bytes[index] ^= 1;
        assert_eq!(
            codec.decode(&mut bytes).unwrap_err().kind(),
            ErrorKind::InvalidData
        );
        let end = bytes.len() - TAG_LEN;
        assert!(bytes[HEADER_LEN + NONCE_LEN..end].iter().all(|b| *b == 0));
    }
    assert!(
        Codec::new("wrong-password", 4096)
            .decode(&mut fixture())
            .is_err()
    );
}

#[test]
fn rejects_invalid_header_lengths_and_sequence_overflow() {
    let codec = Codec::new("kms-test-password", 4096);
    for (index, value) in [(0, 0), (2, 2), (5, 2)] {
        let mut bytes = fixture();
        bytes[index] = value;
        assert!(codec.decode(&mut bytes).is_err());
    }
    for length in [0u32, 27, 4097, u32::MAX] {
        let mut bytes = fixture();
        bytes[6..10].copy_from_slice(&length.to_be_bytes());
        assert!(codec.decode(&mut bytes).is_err());
    }
    let mut extra = fixture();
    extra.push(0);
    assert!(codec.decode(&mut extra).is_err());
    let mut output = Vec::new();
    let mut meta = metadata();
    meta.sequence = MAX_SEQUENCE + 1;
    assert_eq!(
        codec
            .encode(meta, false, BODY, &mut output)
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidInput
    );
    meta.sequence = MAX_SEQUENCE;
    codec.encode(meta, false, b"", &mut output).unwrap();
    assert_eq!(
        codec.decode(&mut output).unwrap().header.metadata.sequence,
        MAX_SEQUENCE
    );
    assert!(
        Codec::new("test", 27)
            .encode(meta, true, b"", &mut output)
            .is_err()
    );
}

#[tokio::test]
async fn reads_fragmented_and_coalesced_frames_then_clean_eof() {
    let codec = Codec::new("kms-test-password", 4096);
    let mut wire = fixture();
    let mut heartbeat = Vec::new();
    codec
        .encode(
            Metadata {
                event_type: 100,
                ..metadata()
            },
            false,
            b"alive",
            &mut heartbeat,
        )
        .unwrap();
    wire.extend_from_slice(&heartbeat);
    // A one-byte pipe forces header and body fragmentation, regardless of writes.
    let (mut writer, mut reader) = tokio::io::duplex(1);
    let send = async move {
        writer.write_all(&wire).await.unwrap();
    };
    let receive = async {
        let mut buffer = Vec::new();
        let first = codec.read(&mut reader, &mut buffer).await.unwrap().unwrap();
        assert_eq!(first.body, BODY);
        assert_eq!(first.header.metadata, metadata());
        let second = codec.read(&mut reader, &mut buffer).await.unwrap().unwrap();
        assert_eq!(second.body, b"alive");
        assert_eq!(second.header.metadata.event_type, 100);
        assert!(!second.header.encrypted);
        assert!(
            codec
                .read(&mut reader, &mut buffer)
                .await
                .unwrap()
                .is_none()
        );
    };
    tokio::join!(send, receive);

    let mut combined = fixture();
    combined.extend_from_slice(&heartbeat);
    let mut reader = combined.as_slice();
    let mut buffer = Vec::new();
    assert_eq!(
        codec
            .read(&mut reader, &mut buffer)
            .await
            .unwrap()
            .unwrap()
            .body,
        BODY
    );
    assert_eq!(
        codec
            .read(&mut reader, &mut buffer)
            .await
            .unwrap()
            .unwrap()
            .body,
        b"alive"
    );
}

#[tokio::test]
async fn reports_truncated_frames_and_rejects_oversize_before_body_read() {
    let codec = Codec::new("kms-test-password", 4096);
    let wire = fixture();
    for length in [1, HEADER_LEN - 1, HEADER_LEN, wire.len() - 1] {
        let mut reader = &wire[..length];
        let mut buffer = Vec::new();
        assert_eq!(
            codec
                .read(&mut reader, &mut buffer)
                .await
                .unwrap_err()
                .kind(),
            ErrorKind::UnexpectedEof
        );
    }
    let mut header = wire[..HEADER_LEN].to_vec();
    header[6..10].copy_from_slice(&u32::MAX.to_be_bytes());
    let mut reader = header.as_slice();
    let mut buffer = Vec::new();
    assert_eq!(
        codec
            .read(&mut reader, &mut buffer)
            .await
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidData
    );
    assert!(buffer.is_empty());
}

#[test]
fn encoder_interoperates_with_independent_decryption_and_preserves_unknown_events() {
    use aes_gcm::aead::Aead;
    let codec = Codec::new("kms-test-password", 4096);
    let meta = Metadata {
        event_type: 254,
        ..metadata()
    };
    let mut wire = Vec::new();
    codec.encode(meta, true, BODY, &mut wire).unwrap();
    assert_eq!(&wire[..4], &[0xab, 0x12, 1, 254]);
    assert_eq!(&wire[10..16], &meta.sequence.to_be_bytes()[2..]);
    let nonce: [u8; 12] = wire[24..36].try_into().unwrap();
    let digest = Sha256::digest(b"kms-test-password");
    let cipher = Aes128Gcm::new_from_slice(&digest[..16]).unwrap();
    assert_eq!(
        cipher.decrypt(&Nonce::from(nonce), &wire[36..]).unwrap(),
        BODY
    );
    assert_eq!(
        codec.decode(&mut wire).unwrap().header.metadata.event_type,
        254
    );
}
