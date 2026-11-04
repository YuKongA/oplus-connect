use super::*;
use aes_gcm::{
    Aes128Gcm, Nonce,
    aead::{Aead, KeyInit},
};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

fn config() -> ServerConfig {
    ServerConfig {
        device_id: "20108139A722".into(),
        peer_ip: "127.0.0.1".parse().unwrap(),
        bind_ip: "127.0.0.1".parse().unwrap(),
        width: 3840,
        height: 2160,
        direction: 1,
    }
}

fn timing() -> Timing {
    Timing {
        handshake: Duration::from_secs(1),
        idle: Duration::from_secs(2),
        heartbeat: Duration::from_millis(50),
        write: Duration::from_secs(1),
    }
}

async fn wait_status(server: &Server, expected: SessionStatus) {
    timeout(Duration::from_secs(2), async {
        loop {
            if server.status() == expected {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

fn encode(codec: &Codec, event_type: u8, body: &[u8]) -> Vec<u8> {
    let mut output = Vec::new();
    codec
        .encode(
            Metadata {
                event_type,
                sequence: 0,
                timestamp_ms: 0,
            },
            false,
            body,
            &mut output,
        )
        .unwrap();
    output
}

async fn hello(stream: &mut TcpStream, codec: &Codec, id: &str, support: u8) {
    let identity = serde_json::to_vec(&json!({"deviceid": id, "supportEncrypt": support})).unwrap();
    stream
        .write_all(&encode(codec, 6, &identity))
        .await
        .unwrap();
}

async fn resolution(stream: &mut TcpStream, codec: &Codec) {
    stream
        .write_all(&encode(
            codec,
            1,
            br#"{"sync_t":"resolution","width":2520,"height":1680}"#,
        ))
        .await
        .unwrap();
}

async fn next(stream: &mut TcpStream, codec: &Codec) -> (super::super::Header, Vec<u8>) {
    let mut buffer = Vec::new();
    let frame = timeout(Duration::from_secs(2), codec.read(stream, &mut buffer))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    (frame.header, frame.body.to_vec())
}

async fn connected(server: &Server) -> (TcpStream, Codec) {
    let codec = Codec::new(server.password(), MAX_BODY_LEN);
    let mut stream = TcpStream::connect(server.local_addr()).await.unwrap();
    hello(&mut stream, &codec, "20108139a722", 1).await;
    resolution(&mut stream, &codec).await;
    for sequence in 1..=2 {
        let (header, _) = next(&mut stream, &codec).await;
        assert!(header.encrypted);
        assert_eq!(header.metadata.event_type, 1);
        assert_eq!(header.metadata.sequence, sequence);
    }
    wait_status(server, SessionStatus::Connected).await;
    (stream, codec)
}

async fn closed_without_data(stream: &mut TcpStream) {
    let mut byte = [0];
    let result = timeout(Duration::from_secs(2), stream.read(&mut byte))
        .await
        .unwrap();
    match result {
        Ok(0) => {}
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted
            ) => {}
        other => panic!("expected closed stream without payload, got {other:?}"),
    }
}

#[tokio::test]
async fn fresh_secrets_and_test_data_interoperate_with_independent_aes() {
    let mut first = Server::bind(config()).await.unwrap();
    let mut second = Server::bind(config()).await.unwrap();
    assert_eq!(first.password().len(), 16);
    assert!(first.password().is_ascii());
    assert_ne!(first.password(), second.password());
    assert_ne!(first.test_data(), second.test_data());
    let wire = STANDARD.decode(first.test_data()).unwrap();
    assert_eq!(wire.len(), 12 + 8 + 16);
    let digest = Sha256::digest(first.password().as_bytes());
    let cipher = Aes128Gcm::new_from_slice(&digest[..16]).unwrap();
    let nonce: [u8; 12] = wire[..12].try_into().unwrap();
    assert_eq!(
        cipher.decrypt(&Nonce::from(nonce), &wire[12..]).unwrap(),
        b"1234abcd"
    );
    assert_eq!(
        serde_json::to_string(&first.status()).unwrap(),
        "\"listening\""
    );
    first.shutdown().await;
    second.shutdown().await;
    assert_eq!(first.status(), SessionStatus::Closed);
}

#[tokio::test]
async fn handshake_sends_only_encrypted_captured_arrangement_and_resolution() {
    let mut server = Server::bind(config()).await.unwrap();
    let codec = Codec::new(server.password(), MAX_BODY_LEN);
    let mut stream = TcpStream::connect(server.local_addr()).await.unwrap();
    hello(&mut stream, &codec, "20108139a722", 1).await;
    wait_status(&server, SessionStatus::Handshaking).await;
    // No outgoing payload until the peer completes both handshake frames.
    let mut byte = [0];
    assert!(
        timeout(Duration::from_millis(20), stream.read(&mut byte))
            .await
            .is_err()
    );
    resolution(&mut stream, &codec).await;
    for (sequence, expected) in [
        (1u64, json!({"count":1,"direction":1,"sync_t":"arrange"})),
        (2, json!({"height":2160,"width":3840,"sync_t":"resolution"})),
    ] {
        // Read raw wire and decrypt independently from the server Codec.
        let mut header = [0u8; HEADER_LEN];
        timeout(Duration::from_secs(2), stream.read_exact(&mut header))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(&header[..6], &[0xab, 0x12, 1, 1, 0, 1]);
        assert_eq!(&header[10..16], &sequence.to_be_bytes()[2..]);
        let length = u32::from_be_bytes(header[6..10].try_into().unwrap()) as usize;
        let mut body = vec![0u8; length];
        stream.read_exact(&mut body).await.unwrap();
        let digest = Sha256::digest(server.password().as_bytes());
        let cipher = Aes128Gcm::new_from_slice(&digest[..16]).unwrap();
        let nonce: [u8; 12] = body[..12].try_into().unwrap();
        let plain = cipher.decrypt(&Nonce::from(nonce), &body[12..]).unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&plain).unwrap(),
            expected
        );
    }
    wait_status(&server, SessionStatus::Connected).await;
    server.shutdown().await;
    closed_without_data(&mut stream).await;
    server.shutdown().await;
}

#[tokio::test]
async fn heartbeat_echo_and_requests_use_plaintext_sequence_zero() {
    let mut server = Server::bind_with_timing(config(), timing()).await.unwrap();
    let (mut stream, codec) = connected(&server).await;
    stream
        .write_all(&encode(&codec, 4, br#"{"heartbeat_id":739}"#))
        .await
        .unwrap();
    loop {
        let (header, body) = next(&mut stream, &codec).await;
        assert!(!header.encrypted);
        assert_eq!(header.metadata.sequence, 0);
        if header.metadata.event_type == 5 {
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
                json!({"heartbeat_id":739})
            );
            break;
        }
        assert_eq!(header.metadata.event_type, 4);
    }
    let (header, body) = next(&mut stream, &codec).await;
    assert_eq!(header.metadata.event_type, 4);
    assert!(!header.encrypted);
    assert_eq!(header.metadata.sequence, 0);
    stream.write_all(&encode(&codec, 5, &body)).await.unwrap();
    stream
        .write_all(&encode(&codec, 100, b"alive"))
        .await
        .unwrap();
    assert_eq!(server.status(), SessionStatus::Connected);
    server.shutdown().await;
}

#[tokio::test]
async fn fragmented_header_and_body_survive_multiple_heartbeat_ticks() {
    let mut server = Server::bind_with_timing(config(), timing()).await.unwrap();
    let (mut stream, codec) = connected(&server).await;
    let wire = encode(&codec, 4, br#"{"heartbeat_id":918273}"#);
    stream.write_all(&wire[..7]).await.unwrap();
    let (header, _) = next(&mut stream, &codec).await;
    assert_eq!(header.metadata.event_type, 4);
    stream.write_all(&wire[7..HEADER_LEN + 5]).await.unwrap();
    let (header, _) = next(&mut stream, &codec).await;
    assert_eq!(header.metadata.event_type, 4);
    stream.write_all(&wire[HEADER_LEN + 5..]).await.unwrap();
    loop {
        let (header, body) = next(&mut stream, &codec).await;
        if header.metadata.event_type == 5 {
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
                json!({"heartbeat_id":918273})
            );
            break;
        }
        assert_eq!(header.metadata.event_type, 4);
    }
    assert_eq!(server.status(), SessionStatus::Connected);
    server.shutdown().await;
}

#[tokio::test]
async fn wrong_identity_and_downgrade_do_not_send_or_kill_listener() {
    let mut server = Server::bind(config()).await.unwrap();
    let codec = Codec::new(server.password(), MAX_BODY_LEN);
    for (id, support) in [("wrong-id", 1), ("20108139A722", 0), ("20108139A722", 2)] {
        let mut stream = TcpStream::connect(server.local_addr()).await.unwrap();
        hello(&mut stream, &codec, id, support).await;
        closed_without_data(&mut stream).await;
        wait_status(&server, SessionStatus::Listening).await;
    }
    let (stream, _) = connected(&server).await;
    drop(stream);
    wait_status(&server, SessionStatus::Listening).await;
    server.shutdown().await;
}

#[tokio::test]
async fn wrong_source_ip_is_rejected_before_any_handshake_output() {
    let mut cfg = config();
    cfg.peer_ip = "127.0.0.2".parse().unwrap();
    let mut server = Server::bind(cfg).await.unwrap();
    let mut stream = TcpStream::connect(server.local_addr()).await.unwrap();
    closed_without_data(&mut stream).await;
    assert_eq!(server.status(), SessionStatus::Listening);
    server.shutdown().await;
}

#[tokio::test]
async fn handshake_deadline_drops_partial_frames_and_listener_can_reconnect() {
    let mut times = timing();
    times.handshake = Duration::from_millis(60);
    let mut server = Server::bind_with_timing(config(), times).await.unwrap();
    let mut stream = TcpStream::connect(server.local_addr()).await.unwrap();
    stream.write_all(&[0xab, 0x12, 1]).await.unwrap();
    wait_status(&server, SessionStatus::Handshaking).await;
    closed_without_data(&mut stream).await;
    wait_status(&server, SessionStatus::Listening).await;
    let (stream, _) = connected(&server).await;
    drop(stream);
    wait_status(&server, SessionStatus::Listening).await;
    server.shutdown().await;
}

#[tokio::test]
async fn idle_deadline_ignores_own_ticks_and_partial_peer_frames() {
    let mut times = timing();
    times.idle = Duration::from_millis(180);
    let mut server = Server::bind_with_timing(config(), times).await.unwrap();
    let (mut stream, codec) = connected(&server).await;
    let partial = encode(&codec, 100, b"alive");
    stream.write_all(&partial[..HEADER_LEN + 1]).await.unwrap();
    wait_status(&server, SessionStatus::Listening).await;
    // Drain requests queued before the idle close; own writes do not extend it.
    let mut sink = Vec::new();
    timeout(Duration::from_secs(2), stream.read_to_end(&mut sink))
        .await
        .unwrap()
        .unwrap();
    assert!(!sink.is_empty());
    let (stream, _) = connected(&server).await;
    drop(stream);
    server.shutdown().await;
}

#[tokio::test]
async fn only_one_peer_and_shutdown_or_drop_close_streams_and_listener() {
    let mut server = Server::bind(config()).await.unwrap();
    let addr = server.local_addr();
    let (mut stream, _) = connected(&server).await;
    let mut extra = TcpStream::connect(addr).await.unwrap();
    closed_without_data(&mut extra).await;
    assert_eq!(server.status(), SessionStatus::Connected);
    drop(stream);
    wait_status(&server, SessionStatus::Listening).await;
    (stream, _) = connected(&server).await;
    server.shutdown().await;
    closed_without_data(&mut stream).await;
    assert!(TcpStream::connect(addr).await.is_err());

    let server = Server::bind(config()).await.unwrap();
    let addr = server.local_addr();
    let (mut stream, _) = connected(&server).await;
    drop(server);
    closed_without_data(&mut stream).await;
    assert!(TcpStream::connect(addr).await.is_err());
}

#[tokio::test]
async fn dropping_during_partial_handshake_closes_owned_read() {
    let server = Server::bind(config()).await.unwrap();
    let addr = server.local_addr();
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(&[0xab]).await.unwrap();
    wait_status(&server, SessionStatus::Handshaking).await;
    drop(server);
    closed_without_data(&mut stream).await;
    assert!(TcpStream::connect(addr).await.is_err());
}

#[tokio::test]
async fn rejects_unspecified_bind_and_unsupported_direction_or_dimensions() {
    for cfg in [
        ServerConfig {
            bind_ip: "0.0.0.0".parse().unwrap(),
            ..config()
        },
        ServerConfig {
            width: 0,
            ..config()
        },
        ServerConfig {
            height: 16385,
            ..config()
        },
        ServerConfig {
            direction: 5,
            ..config()
        },
    ] {
        assert!(
            matches!(Server::bind(cfg).await, Err(error) if error.kind() == ErrorKind::InvalidInput)
        );
    }
    assert_eq!(
        canonical_ip("::ffff:127.0.0.1".parse().unwrap()),
        config().peer_ip
    );
}

// Captured peer-to-PC type-2 body, 2026-11-02 21:21:39.545.
const PEER_POINTER: &[u8] = br#"{"type":1,"down_t":0,"event_t":279847801,"act":7,"act_b":0,"bt_s":0,"src":8194,"cls":0,"flg":0,"left_key":0,"pt_c":1,"pts":[{"pt_id":0,"tool_t":3,"x":2519.0,"y":379.5918}]}"#;

async fn wait_pointer(server: &Server, event_time: u64) -> Arc<PeerPointerEvent> {
    timeout(Duration::from_secs(2), async {
        loop {
            if let Some(event) = server.latest_peer_pointer() {
                if event.event_t == event_time {
                    return event;
                }
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn cross_back_is_normalized_consumed_and_scoped_to_connection() {
    let mut server = Server::bind(config()).await.unwrap();
    let (mut stream, codec) = connected(&server).await;
    let generation = server.connection_id().unwrap();
    stream
        .write_all(&encode(&codec, 2, PEER_POINTER))
        .await
        .unwrap();
    wait_pointer(&server, 279847801).await;
    assert!(
        server
            .receive_cross_back_for_generation(generation + 1)
            .await
            .is_err()
    );
    let position = server
        .receive_cross_back_for_generation(generation)
        .await
        .unwrap()
        .unwrap();
    assert!((position.pct_x - 2519.0 / 2520.0).abs() < 1e-9);
    assert!((position.pct_y - 379.5918 / 1680.0).abs() < 1e-9);
    assert_eq!(
        server
            .receive_cross_back_for_generation(generation)
            .await
            .unwrap(),
        None
    );

    let mut peer: serde_json::Value = serde_json::from_slice(PEER_POINTER).unwrap();
    peer["event_t"] = json!(279847802u64);
    stream
        .write_all(&encode(&codec, 2, &serde_json::to_vec(&peer).unwrap()))
        .await
        .unwrap();
    wait_pointer(&server, 279847802).await;
    server
        .send_input_for_generation(
            generation,
            InputEvent::Enter {
                pct_x: 1.0,
                pct_y: 0.5,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        server
            .receive_cross_back_for_generation(generation)
            .await
            .unwrap(),
        None
    );
    peer["event_t"] = json!(279847803u64);
    peer["pts"][0]["y"] = json!(840.0);
    stream
        .write_all(&encode(&codec, 2, &serde_json::to_vec(&peer).unwrap()))
        .await
        .unwrap();
    wait_pointer(&server, 279847803).await;
    assert_eq!(
        server
            .receive_cross_back_for_generation(generation)
            .await
            .unwrap()
            .unwrap()
            .pct_y,
        0.5
    );

    drop(stream);
    wait_status(&server, SessionStatus::Listening).await;
    let (_stream, _codec) = connected(&server).await;
    let next_generation = server.connection_id().unwrap();
    assert_ne!(next_generation, generation);
    assert!(
        server
            .receive_cross_back_for_generation(generation)
            .await
            .is_err()
    );
    assert_eq!(
        server
            .receive_cross_back_for_generation(next_generation)
            .await
            .unwrap(),
        None
    );
    server.shutdown().await;
}

#[tokio::test]
async fn captured_pointer_hover_and_drag_keep_session_alive_and_replace_snapshot() {
    let mut server = Server::bind(config()).await.unwrap();
    let (mut stream, codec) = connected(&server).await;
    assert!(server.latest_peer_pointer().is_none());
    stream
        .write_all(&encode(&codec, 2, PEER_POINTER))
        .await
        .unwrap();
    let hover = wait_pointer(&server, 279847801).await;
    assert_eq!(hover.kind, 1);
    assert_eq!(hover.act, 7);
    assert_eq!(hover.pt_c, 1);
    assert_eq!(hover.pts[0].x, 2519.0);
    assert_eq!(hover.pts[0].y, 379.5918);
    assert_eq!(server.status(), SessionStatus::Connected);

    // Captured drag crossing: 2026-11-02 21:23:50.361 (act=2, button down).
    let drag = br#"{"type":1,"down_t":279977872,"event_t":279978619,"act":2,"act_b":0,"bt_s":1,"src":8194,"cls":0,"flg":0,"left_key":1,"pt_c":1,"pts":[{"pt_id":0,"tool_t":3,"x":2518.872,"y":1149.1904}]}"#;
    stream.write_all(&encode(&codec, 2, drag)).await.unwrap();
    let drag = wait_pointer(&server, 279978619).await;
    assert_eq!(drag.act, 2);
    assert_eq!(drag.left_key, 1);
    assert_eq!(drag.bt_s, 1);
    assert_eq!(drag.pts[0].x, 2518.872);
    // An existing snapshot stays immutable; the server only retains the latest.
    assert_eq!(hover.event_t, 279847801);
    assert!(!Arc::ptr_eq(&hover, &drag));
    stream
        .write_all(&encode(&codec, 4, br#"{"heartbeat_id":123}"#))
        .await
        .unwrap();
    let (header, body) = next(&mut stream, &codec).await;
    assert_eq!(header.metadata.event_type, 5);
    assert_eq!(body, br#"{"heartbeat_id":123}"#);
    assert_eq!(server.status(), SessionStatus::Connected);

    drop(stream);
    wait_status(&server, SessionStatus::Listening).await;
    assert!(server.latest_peer_pointer().is_none());
    let (mut stream, codec) = connected(&server).await;
    assert!(server.latest_peer_pointer().is_none());
    stream
        .write_all(&encode(&codec, 2, PEER_POINTER))
        .await
        .unwrap();
    wait_pointer(&server, 279847801).await;
    server.shutdown().await;
    assert!(server.latest_peer_pointer().is_none());
    closed_without_data(&mut stream).await;
}

#[tokio::test]
async fn malformed_pointer_events_close_only_the_peer_and_clear_snapshot() {
    let mut server = Server::bind(config()).await.unwrap();
    let source: serde_json::Value = serde_json::from_slice(PEER_POINTER).unwrap();
    let mut wrong_count = source.clone();
    wrong_count["pt_c"] = json!(2);
    let mut missing_field = source.clone();
    missing_field.as_object_mut().unwrap().remove("act");
    let mut wrong_coordinate = source.clone();
    wrong_coordinate["pts"][0]["x"] = json!("not-a-coordinate");
    let mut empty_points = source.clone();
    empty_points["pts"] = json!([]);
    empty_points["pt_c"] = json!(0);
    let mut oversized = source.clone();
    oversized["pts"] = serde_json::Value::Array(vec![source["pts"][0].clone(); 33]);
    oversized["pt_c"] = json!(33);
    let mut wrong_kind = source;
    wrong_kind["type"] = json!(6);
    for malformed in [
        wrong_count,
        missing_field,
        wrong_coordinate,
        empty_points,
        oversized,
        wrong_kind,
    ] {
        let (mut stream, codec) = connected(&server).await;
        stream
            .write_all(&encode(&codec, 2, PEER_POINTER))
            .await
            .unwrap();
        wait_pointer(&server, 279847801).await;
        let body = serde_json::to_vec(&malformed).unwrap();
        stream.write_all(&encode(&codec, 2, &body)).await.unwrap();
        closed_without_data(&mut stream).await;
        wait_status(&server, SessionStatus::Listening).await;
        assert!(server.latest_peer_pointer().is_none());
    }
    server.shutdown().await;
}

#[tokio::test]
async fn requested_input_is_encrypted_and_repeated_controls_release() {
    let mut server = Server::bind(config()).await.unwrap();
    let (mut stream, codec) = connected(&server).await;
    let key = InputEvent::Key {
        key_code: 79,
        modifier: 0,
        pressed: true,
    };
    server.send_input(key.clone()).await.unwrap();
    server.send_input(key).await.unwrap();
    for _ in 0..2 {
        let (header, body) = next(&mut stream, &codec).await;
        assert_eq!(header.metadata.event_type, 3);
        assert!(header.encrypted);
        assert_eq!(
            body,
            br#"{"keyCode":79,"modifier":0,"platform":2,"pressed":true,"type":3}"#
        );
    }
    server.release_input().await.unwrap();
    let (header, body) = next(&mut stream, &codec).await;
    assert_eq!(header.metadata.event_type, 3);
    assert_eq!(
        body,
        br#"{"keyCode":79,"modifier":0,"platform":2,"pressed":false,"type":3}"#
    );
    server.shutdown().await;
}

#[tokio::test]
async fn shutdown_releases_pressed_button_before_closing_peer() {
    let mut server = Server::bind(config()).await.unwrap();
    let (mut stream, codec) = connected(&server).await;
    server
        .send_input(InputEvent::Button {
            button_type: 1,
            pressed: true,
        })
        .await
        .unwrap();
    let (header, _) = next(&mut stream, &codec).await;
    assert_eq!(header.metadata.event_type, 3);
    server.shutdown().await;
    let (header, body) = next(&mut stream, &codec).await;
    assert_eq!(header.metadata.event_type, 3);
    assert_eq!(
        body,
        br#"{"buttonType":1,"clickCount":0,"platform":2,"pressed":false,"type":1}"#
    );
    closed_without_data(&mut stream).await;
}

#[tokio::test]
async fn command_does_not_cancel_fragmented_peer_read() {
    let mut server = Server::bind_with_timing(config(), timing()).await.unwrap();
    let (mut stream, codec) = connected(&server).await;
    let wire = encode(&codec, 4, br#"{"heartbeat_id":918273}"#);
    stream.write_all(&wire[..7]).await.unwrap();
    let (saw_move, saw_echo) = {
        let input = server.send_input(InputEvent::Move {
            delta_x: 2,
            delta_y: -3,
        });
        tokio::pin!(input);
        stream.write_all(&wire[7..]).await.unwrap();
        input.await.unwrap();
        let mut saw_move = false;
        let mut saw_echo = false;
        for _ in 0..8 {
            let (header, body) = next(&mut stream, &codec).await;
            if header.metadata.event_type == 3 {
                saw_move = true;
                assert_eq!(body, br#"{"deltaX":2,"deltaY":-3,"platform":2,"type":0}"#);
            } else if header.metadata.event_type == 5 {
                saw_echo = true;
                assert_eq!(body, br#"{"heartbeat_id":918273}"#);
            }
            if saw_move && saw_echo {
                break;
            }
        }
        (saw_move, saw_echo)
    };
    assert!(saw_move && saw_echo);
    server.shutdown().await;
}
