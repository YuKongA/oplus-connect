use super::*;
use aes_gcm::{
    Aes256Gcm, Nonce, Tag,
    aead::{AeadInOut, KeyInit},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use p256::{PublicKey, SecretKey, ecdh::diffie_hellman, elliptic_curve::sec1::ToSec1Point};
use tokio::io::AsyncReadExt;

async fn server() -> Server {
    Server::bind(ServerConfig {
        bind_ip: "127.0.0.1".parse().unwrap(),
        peer_ip: "127.0.0.1".parse().unwrap(),
        port: None,
    })
    .await
    .unwrap()
}

async fn value(stream: &mut TcpStream) -> Value {
    let mut buffer = Vec::new();
    assert!(
        timeout(Duration::from_secs(2), read_frame(stream, &mut buffer))
            .await
            .unwrap()
            .unwrap()
    );
    serde_json::from_slice(&buffer).unwrap()
}

async fn closed(stream: &mut TcpStream) {
    let mut byte = [0u8; 1];
    let result = timeout(Duration::from_secs(2), stream.read(&mut byte))
        .await
        .unwrap();
    assert!(
        matches!(result, Ok(0) | Err(_)),
        "expected closed connection"
    );
}

async fn wait_for(server: &Server, predicate: impl Fn(&SessionStatus) -> bool) {
    timeout(Duration::from_secs(2), async {
        loop {
            if predicate(&server.status()) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

fn proof(advertisement: &Value) -> (Value, Aes256Gcm) {
    // A deterministic test-only peer key; the production server remains fresh.
    let secret = SecretKey::from_slice(&[3u8; 32]).unwrap();
    let server_der = STANDARD
        .decode(advertisement["public_key"].as_str().unwrap())
        .unwrap();
    let server_key = PublicKey::from_sec1_bytes(&server_der[26..]).unwrap();
    let shared = diffie_hellman(&secret.to_nonzero_scalar(), server_key.as_affine());
    let cipher = Aes256Gcm::new_from_slice(shared.raw_secret_bytes()).unwrap();
    let mut peer_der = server_der[..26].to_vec();
    peer_der.extend_from_slice(secret.public_key().to_sec1_point(false).as_bytes());
    let timestamp = "1793626185444";
    let nonce = [4u8; 12];
    let mut encrypted = STANDARD
        .decode(advertisement["id"].as_str().unwrap())
        .unwrap();
    let tag = cipher
        .encrypt_inout_detached(
            &Nonce::from(nonce),
            timestamp.as_bytes(),
            encrypted.as_mut_slice().into(),
        )
        .unwrap();
    encrypted.extend_from_slice(&tag);
    (
        json!({"cmd":"OdmfAuthResponse","auth_info":json!({
        "auth_version":1,"public_key":STANDARD.encode(peer_der),
        "timestamp":timestamp,"iv":STANDARD.encode(nonce),"encrypt_cipher":STANDARD.encode(encrypted)
    }).to_string()}),
        cipher,
    )
}

async fn authenticate(server: &Server) -> (TcpStream, Codec, u64) {
    let mut stream = TcpStream::connect(server.local_addr()).await.unwrap();
    let request = value(&mut stream).await;
    assert_eq!(request["cmd"], "OdmfAuthRequest");
    let id = request["msg"].as_u64().unwrap();
    let (proof, cipher) = proof(&server.advertisement());
    write_plain(&mut stream, &proof).await.unwrap();
    let success = value(&mut stream).await;
    assert_eq!(success["cmd"], "AuthSuccess");
    let nonce: [u8; 12] = STANDARD
        .decode(success["iv"].as_str().unwrap())
        .unwrap()
        .try_into()
        .unwrap();
    let mut encrypted = STANDARD
        .decode(success["encrypt_cipher"].as_str().unwrap())
        .unwrap();
    let tag_start = encrypted.len() - 16;
    let tag = Tag::from(<[u8; 16]>::try_from(&encrypted[tag_start..]).unwrap());
    cipher
        .decrypt_inout_detached(
            &Nonce::from(nonce),
            success["timestamp"].as_str().unwrap().as_bytes(),
            encrypted[..tag_start].as_mut().into(),
            &tag,
        )
        .unwrap();
    let key: Value = serde_json::from_slice(&encrypted[..tag_start]).unwrap();
    let key: [u8; 32] = STANDARD
        .decode(key["key"].as_str().unwrap())
        .unwrap()
        .try_into()
        .unwrap();
    (stream, Codec::new(&key), id)
}

async fn encrypted(stream: &mut TcpStream, codec: &Codec, message: &Value) {
    let mut frame = Vec::new();
    codec
        .encode(&serde_json::to_vec(message).unwrap(), &mut frame)
        .unwrap();
    stream.write_all(&frame).await.unwrap();
}

async fn pair(server: &Server) -> (TcpStream, Codec, u64) {
    let (mut stream, codec, id) = authenticate(server).await;
    encrypted(&mut stream, &codec, &json!({"cmd":"PairRequest"})).await;
    let success = value(&mut stream).await;
    assert_eq!(
        success,
        json!({"cmd":"PairSuccess","high_performance":"","platform":"",
        "remote_transportbus_id":id.to_string(),"remote_user_id":"local_client",
        "remote_user_name":"local_client","x264":""})
    );
    // Captured official order: PairSuccess, then encrypted TimeSyncServer, and
    // only then the caller's media signaling.
    let mut frame = Vec::new();
    assert!(read_frame(&mut stream, &mut frame).await.unwrap());
    let time_sync: Value = serde_json::from_slice(codec.decode(&mut frame).unwrap()).unwrap();
    assert_eq!(time_sync["cmd"], "TimeSyncServer");
    let advertised = server.time_sync_addr().expect("time-sync endpoint");
    assert_eq!(
        time_sync["time_sync_server_port"].as_u64().unwrap() as u16,
        advertised.port()
    );
    assert_eq!(
        time_sync["time_sync_server_ip"].as_str().unwrap(),
        advertised.ip().to_string()
    );
    wait_for(server, |status| status.connection_ids.contains(&id)).await;
    (stream, codec, id)
}

#[tokio::test]
async fn authenticates_pairs_and_exchanges_only_encrypted_signaling() {
    let mut server = server().await;
    assert_eq!(
        server.local_addr().ip(),
        "127.0.0.1".parse::<IpAddr>().unwrap()
    );
    assert_ne!(server.local_addr().port(), 0);
    let (mut stream, codec, id) = pair(&server).await;
    write_plain(&mut stream, &json!({"cmd":"ping"}))
        .await
        .unwrap();
    assert_eq!(value(&mut stream).await, json!({"cmd":"pong"}));
    let payload = json!({"type":"candidate","candidate":"candidate supplied by test caller"});
    server.send(id, payload.clone()).await.unwrap();
    let mut frame = Vec::new();
    assert!(read_frame(&mut stream, &mut frame).await.unwrap());
    let envelope: Value = serde_json::from_slice(codec.decode(&mut frame).unwrap()).unwrap();
    assert_eq!(envelope["cmd"], "send");
    assert_eq!(
        serde_json::from_str::<Value>(envelope["msg"].as_str().unwrap()).unwrap(),
        payload
    );
    encrypted(&mut stream, &codec, &envelope).await;
    let received = timeout(Duration::from_secs(2), async {
        loop {
            if let Some(payload) = server.receive(id).unwrap() {
                break payload;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(received, IncomingMessage::Signaling(payload));
    server.shutdown().await;
    closed(&mut stream).await;
}

#[tokio::test]
async fn rejects_invalid_proof_and_does_not_expose_a_ready_token() {
    let mut server = server().await;
    let mut stream = TcpStream::connect(server.local_addr()).await.unwrap();
    let request = value(&mut stream).await;
    let id = request["msg"].as_u64().unwrap();
    assert!(server.status().connection_ids.is_empty());
    assert_eq!(
        server
            .send(id, json!({"type":"offer"}))
            .await
            .unwrap_err()
            .kind(),
        ErrorKind::NotConnected
    );
    let (mut response, _) = proof(&server.advertisement());
    let mut info: Value = serde_json::from_str(response["auth_info"].as_str().unwrap()).unwrap();
    let mut cipher = STANDARD
        .decode(info["encrypt_cipher"].as_str().unwrap())
        .unwrap();
    cipher[0] ^= 1;
    info["encrypt_cipher"] = STANDARD.encode(cipher).into();
    response["auth_info"] = info.to_string().into();
    write_plain(&mut stream, &response).await.unwrap();
    closed(&mut stream).await;
    wait_for(&server, |status| {
        status.state == ServerState::Listening && status.last_error.is_some()
    })
    .await;
    server.shutdown().await;
}

#[tokio::test]
async fn authentication_does_not_allow_plaintext_pairing_or_early_signaling() {
    let mut server = server().await;
    let (mut stream, _, id) = authenticate(&server).await;
    assert_eq!(server.status().state, ServerState::Authenticating);
    assert_eq!(
        server
            .send(id, json!({"type":"offer"}))
            .await
            .unwrap_err()
            .kind(),
        ErrorKind::NotConnected
    );
    write_plain(&mut stream, &json!({"cmd":"PairRequest"}))
        .await
        .unwrap();
    closed(&mut stream).await;
    let (mut next, codec, _) = authenticate(&server).await;
    encrypted(&mut next, &codec, &json!({"cmd":"send","msg":"{}"})).await;
    closed(&mut next).await;
    server.shutdown().await;
}

#[tokio::test]
async fn rejects_plaintext_signaling_after_successful_pairing() {
    let mut server = server().await;
    let (mut stream, _, _) = pair(&server).await;
    write_plain(
        &mut stream,
        &json!({"cmd":"send","msg":"{\"type\":\"offer\"}"}),
    )
    .await
    .unwrap();
    closed(&mut stream).await;
    wait_for(&server, |status| status.connection_ids.is_empty()).await;
    assert!(server.status().last_error.unwrap().contains("plaintext"));
    server.shutdown().await;
}

#[tokio::test]
async fn owner_ip_and_two_connection_limit_are_enforced() {
    let mut foreign = Server::bind(ServerConfig {
        bind_ip: "127.0.0.1".parse().unwrap(),
        peer_ip: "127.0.0.2".parse().unwrap(),
        port: None,
    })
    .await
    .unwrap();
    let mut rejected = TcpStream::connect(foreign.local_addr()).await.unwrap();
    closed(&mut rejected).await;
    assert_eq!(foreign.status().state, ServerState::Listening);
    foreign.shutdown().await;

    let mut server = server().await;
    let (mut first, _, first_id) = pair(&server).await;
    let (mut second, _, second_id) = pair(&server).await;
    assert_ne!(first_id, second_id);
    let mut third = TcpStream::connect(server.local_addr()).await.unwrap();
    closed(&mut third).await;
    assert_eq!(server.status().connection_ids.len(), 2);
    server.shutdown().await;
    closed(&mut first).await;
    closed(&mut second).await;
    assert_eq!(server.status().state, ServerState::Closed);
    assert!(TcpStream::connect(server.local_addr()).await.is_err());
    server.shutdown().await;
}

#[tokio::test]
async fn stale_tokens_and_queued_payloads_do_not_survive_reconnect() {
    let mut server = server().await;
    let (mut first, codec, old_id) = pair(&server).await;
    encrypted(
        &mut first,
        &codec,
        &json!({"cmd":"send","msg":"{\"type\":\"candidate\"}"}),
    )
    .await;
    drop(first);
    wait_for(&server, |status| status.connection_ids.is_empty()).await;
    let (mut second, _, new_id) = pair(&server).await;
    assert_ne!(old_id, new_id);
    assert_eq!(
        server
            .send(old_id, json!({"type":"offer"}))
            .await
            .unwrap_err()
            .kind(),
        ErrorKind::NotConnected
    );
    assert_eq!(
        server.receive(old_id).unwrap_err().kind(),
        ErrorKind::NotConnected
    );
    assert!(server.receive(new_id).unwrap().is_none());
    server.shutdown().await;
    closed(&mut second).await;
}

#[tokio::test]
async fn queue_overflow_closes_only_the_slow_connection_with_an_explicit_error() {
    let mut server = server().await;
    let (mut slow, codec, slow_id) = pair(&server).await;
    let (mut other, _, other_id) = pair(&server).await;
    for _ in 0..=QUEUE_CAPACITY {
        encrypted(
            &mut slow,
            &codec,
            &json!({"cmd":"send","msg":"{\"type\":\"candidate\"}"}),
        )
        .await;
    }
    closed(&mut slow).await;
    wait_for(&server, |status| !status.connection_ids.contains(&slow_id)).await;
    assert_eq!(server.status().connection_ids, vec![other_id]);
    assert!(server.status().last_error.unwrap().contains("queue full"));
    write_plain(&mut other, &json!({"cmd":"ping"}))
        .await
        .unwrap();
    assert_eq!(value(&mut other).await, json!({"cmd":"pong"}));
    server.shutdown().await;
}

#[tokio::test]
async fn shutdown_cancels_partial_handshake_and_both_paired_reads() {
    let mut server = server().await;
    let mut pending = TcpStream::connect(server.local_addr()).await.unwrap();
    value(&mut pending).await;
    pending.write_all(&[0]).await.unwrap();
    let (mut paired, _, _) = pair(&server).await;
    timeout(Duration::from_secs(1), server.shutdown())
        .await
        .unwrap();
    closed(&mut pending).await;
    closed(&mut paired).await;
    assert!(server.status().connection_ids.is_empty());
}

#[tokio::test]
async fn drop_and_bye_close_connections() {
    let server = server().await;
    let (mut stream, codec, _) = pair(&server).await;
    encrypted(
        &mut stream,
        &codec,
        &json!({"cmd":"send","msg":"{\"type\":\"bye\"}"}),
    )
    .await;
    closed(&mut stream).await;
    wait_for(&server, |status| status.connection_ids.is_empty()).await;
    let (mut next, _, _) = pair(&server).await;
    drop(server);
    closed(&mut next).await;
}

#[tokio::test]
async fn outbound_writes_preserve_a_partially_received_frame() {
    let mut server = server().await;
    let (mut stream, codec, id) = pair(&server).await;
    let mut frame = Vec::new();
    codec
        .encode(
            br#"{"cmd":"send","msg":"{\"type\":\"candidate\"}"}"#,
            &mut frame,
        )
        .unwrap();
    stream.write_all(&frame[..3]).await.unwrap();
    server.send(id, json!({"type":"candidate"})).await.unwrap();
    stream.write_all(&frame[3..]).await.unwrap();
    timeout(Duration::from_secs(2), async {
        loop {
            if let Some(payload) = server.receive(id).unwrap() {
                assert_eq!(
                    payload,
                    IncomingMessage::Signaling(json!({"type":"candidate"}))
                );
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    server.shutdown().await;
}

#[tokio::test]
async fn outgoing_queue_saturation_cancels_the_exact_tcp_token() {
    let mut server = server().await;
    let id = next_connection_id();
    let (outgoing, _outgoing_rx) = mpsc::channel(1);
    let (_incoming, incoming_rx) = mpsc::channel(1);
    let (cancel, cancelled) = watch::channel(false);
    let (reply, _result) = oneshot::channel();
    assert!(
        outgoing
            .try_send(Command {
                frame: Vec::new(),
                close: false,
                reply
            })
            .is_ok()
    );
    server.shared.lock().unwrap().peers.insert(
        id,
        Peer {
            ready: true,
            outgoing,
            incoming: incoming_rx,
            cancel,
        },
    );
    let error = server
        .send(id, json!({"type":"candidate"}))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("queue full"));
    assert!(*cancelled.borrow());
    assert!(server.status().connection_ids.is_empty());
    assert_eq!(
        server.receive(id).unwrap_err().kind(),
        ErrorKind::NotConnected
    );
    server.shutdown().await;
}

#[tokio::test]
async fn captured_control_messages_are_bounded_typed_observations() {
    let mut server = server().await;
    let (mut stream, codec, id) = pair(&server).await;
    let broadcast = json!({"cmd":"customBroadcast","data":{"COMMAND":"SUB_MODE","subMode":1}});
    let timing = json!({"cmd":"TimeSyncResult","TimeSyncOffset":-34,"TimeSyncRtt":7});
    for message in [&broadcast, &timing] {
        encrypted(&mut stream, &codec, message).await;
    }
    encrypted(
        &mut stream,
        &codec,
        &json!({"cmd":"send","msg":"{\"type\":\"candidate\"}"}),
    )
    .await;
    let expected = [
        IncomingMessage::Control(broadcast),
        IncomingMessage::Control(timing.clone()),
        IncomingMessage::Signaling(json!({"type":"candidate"})),
    ];
    for expected in expected {
        let received = timeout(Duration::from_secs(2), async {
            loop {
                if let Some(message) = server.receive(id).unwrap() {
                    break message;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(received, expected);
    }
    assert_eq!(
        serde_json::to_value(IncomingMessage::Control(timing.clone())).unwrap(),
        json!({"kind":"control","payload":timing})
    );
    assert_eq!(
        serde_json::to_value(IncomingMessage::Signaling(json!({"type":"candidate"}))).unwrap(),
        json!({"kind":"signaling","payload":{"type":"candidate"}})
    );
    write_plain(&mut stream, &timing).await.unwrap();
    closed(&mut stream).await;
    wait_for(&server, |status| status.connection_ids.is_empty()).await;
    assert!(server.status().last_error.unwrap().contains("plaintext"));
    server.shutdown().await;
}

#[tokio::test]
async fn cancelled_queued_commands_are_not_written_or_applied() {
    let mut server = server().await;
    let (mut stream, codec, id) = pair(&server).await;
    let mut frame = Vec::new();
    codec
        .encode(br#"{"cmd":"send","msg":"{\"type\":\"bye\"}"}"#, &mut frame)
        .unwrap();
    let outgoing = server
        .shared
        .lock()
        .unwrap()
        .peers
        .get(&id)
        .unwrap()
        .outgoing
        .clone();
    let (reply, result) = oneshot::channel();
    drop(result);
    assert!(
        outgoing
            .try_send(Command {
                frame,
                close: true,
                reply
            })
            .is_ok()
    );
    server.send(id, json!({"type":"candidate"})).await.unwrap();
    let mut received = Vec::new();
    assert!(
        timeout(
            Duration::from_secs(2),
            read_frame(&mut stream, &mut received)
        )
        .await
        .unwrap()
        .unwrap()
    );
    let envelope: Value = serde_json::from_slice(codec.decode(&mut received).unwrap()).unwrap();
    let payload: Value = serde_json::from_str(envelope["msg"].as_str().unwrap()).unwrap();
    assert_eq!(payload, json!({"type":"candidate"}));
    assert_eq!(server.status().connection_ids, vec![id]);
    server.shutdown().await;
}

#[tokio::test]
async fn time_sync_replies_with_captured_server_response_time_schema() {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let address = socket.local_addr().unwrap();
    let (shutdown, receiver) = watch::channel(false);
    let task = tokio::spawn(time_sync_loop(
        socket,
        "127.0.0.1".parse().unwrap(),
        receiver,
    ));
    let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    client
        .send_to(br#"{"client_request_time":1793624310515}"#, address)
        .await
        .unwrap();
    let mut buffer = [0u8; 512];
    let (len, _) = timeout(Duration::from_secs(2), client.recv_from(&mut buffer))
        .await
        .unwrap()
        .unwrap();
    let reply: Value = serde_json::from_slice(&buffer[..len]).unwrap();
    assert_eq!(reply["client_request_time"], 1793624310515u64);
    // The peer derives its offset from this field; any other name yields an
    // epoch-sized offset instead of a small clock delta.
    assert!(reply["server_response_time"].as_u64().unwrap() > 1_600_000_000_000);
    assert!(reply.get("server_receive_time").is_none());
    shutdown.send_replace(true);
    task.await.unwrap();
}
