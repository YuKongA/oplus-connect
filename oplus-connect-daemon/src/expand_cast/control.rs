//! ExpandCast control ownership shared by authenticated Panta sessions and IPC.
use crate::cast::auth::Identity;
use crate::cast::server::{IncomingMessage, Server, ServerConfig, SessionStatus};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};

const QUEUE_CAPACITY: usize = 32;
pub const CONTROL_TIMEOUT: Duration = Duration::from_secs(15);
pub const BUS_WRITE_TIMEOUT: Duration = Duration::from_secs(3);

type Reply<T> = oneshot::Sender<Result<T, String>>;

pub enum BusCommand {
    Start {
        mode: u8,
        reply: Reply<ExpandCastStatusInfo>,
    },
    Stop {
        reply: Reply<()>,
    },
    Status {
        reply: oneshot::Sender<ExpandCastStatusInfo>,
    },
    OnTabletDeviceInfo {
        width: u32,
        height: u32,
        reply: oneshot::Sender<Result<String, String>>, // returns pc_auth_ready json
    },
    SendSignal {
        connection_id: u64,
        payload: serde_json::Value,
        reply: Reply<()>,
    },
    ReceiveSignal {
        connection_id: u64,
        max: usize,
        reply: Reply<Vec<IncomingMessage>>,
    },
}

#[derive(Clone, Default)]
pub struct ExpandCastRegistry {
    entries: Arc<Mutex<HashMap<String, mpsc::Sender<BusCommand>>>>,
}

struct BusLease {
    registry: ExpandCastRegistry,
    device_id: String,
    sender: mpsc::Sender<BusCommand>,
}

impl Drop for BusLease {
    fn drop(&mut self) {
        let mut entries = self
            .registry
            .entries
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if entries
            .get(&self.device_id)
            .is_some_and(|sender| sender.same_channel(&self.sender))
        {
            entries.remove(&self.device_id);
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExpandCastStatusInfo {
    pub device_id: String,
    pub status: String,
    pub tablet_width: Option<u32>,
    pub tablet_height: Option<u32>,
    pub local_addr: Option<SocketAddr>,
    pub connection_id: Option<u64>,
    pub session: Option<SessionStatus>,
}

pub struct StartPublication {
    pub info: ExpandCastStatusInfo,
    pub general_track: String,
    pub pc_device_info: String,
}

pub struct Controller {
    receiver: mpsc::Receiver<BusCommand>,
    pending: Option<BusCommand>,
    _lease: BusLease,
    device_id: String,
    peer_ip: IpAddr,
    local_ip: IpAddr,
    peer_name: String,
    local_name: String,
    local_device_id: String,
    identity: Arc<Identity>,
    odmf_server: Option<Server>,
    tablet_res: Option<(u32, u32)>,
    active: bool,
    mode: u8,
    /// Tests pick a spare port so the fixed one stays free for session tests.
    #[cfg(test)]
    port: Option<u16>,
}

impl ExpandCastRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_bus(
        &self,
        device_id: &str,
        peer_ip: IpAddr,
        local_ip: IpAddr,
        peer_name: &str,
        local_name: &str,
        local_device_id: &str,
    ) -> Option<(Controller, mpsc::Receiver<BusCommand>)> {
        let id = device_id.trim().to_ascii_uppercase();
        if id.is_empty() || peer_ip.is_unspecified() || local_ip.is_unspecified() {
            return None;
        }
        let (sender, receiver) = mpsc::channel(QUEUE_CAPACITY);
        {
            let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
            entries.insert(id.clone(), sender.clone());
        }
        let lease = BusLease {
            registry: self.clone(),
            device_id: id.clone(),
            sender,
        };
        let (_controller_tx, controller_rx) = mpsc::channel(QUEUE_CAPACITY);
        let controller = Controller {
            receiver,
            pending: None,
            _lease: lease,
            device_id: id,
            peer_ip,
            local_ip,
            peer_name: peer_name.to_string(),
            local_name: local_name.to_string(),
            local_device_id: local_device_id.to_string(),
            identity: Arc::new(Identity::new()),
            odmf_server: None,
            tablet_res: None,
            #[cfg(test)]
            port: None,
            active: false,
            mode: 1,
        };
        Some((controller, controller_rx))
    }

    pub async fn start(&self, device_id: &str, mode: u8) -> Result<ExpandCastStatusInfo, String> {
        if !matches!(mode, 1 | 2) {
            return Err("ExpandCast mode must be 1 or 2".to_string());
        }
        let sender = self.sender_for(device_id)?;
        let (reply, result) = oneshot::channel();
        sender
            .send(BusCommand::Start { mode, reply })
            .await
            .map_err(|_| "ExpandCast bus is closed".to_string())?;
        tokio::time::timeout(CONTROL_TIMEOUT, result)
            .await
            .map_err(|_| "ExpandCast start timed out".to_string())?
            .map_err(|_| "ExpandCast start cancelled".to_string())?
    }

    pub async fn stop(&self, device_id: &str) -> Result<(), String> {
        let sender = self.sender_for(device_id)?;
        let (reply, result) = oneshot::channel();
        sender
            .send(BusCommand::Stop { reply })
            .await
            .map_err(|_| "ExpandCast bus is closed".to_string())?;
        tokio::time::timeout(CONTROL_TIMEOUT, result)
            .await
            .map_err(|_| "ExpandCast stop timed out".to_string())?
            .map_err(|_| "ExpandCast stop cancelled".to_string())?
    }

    pub async fn status(&self, device_id: &str) -> Result<ExpandCastStatusInfo, String> {
        let sender = self.sender_for(device_id)?;
        let (reply, result) = oneshot::channel();
        sender
            .send(BusCommand::Status { reply })
            .await
            .map_err(|_| "ExpandCast bus is closed".to_string())?;
        tokio::time::timeout(Duration::from_secs(3), result)
            .await
            .map_err(|_| "ExpandCast status timed out".to_string())?
            .map_err(|_| "ExpandCast status cancelled".to_string())
    }

    pub async fn send_signal(
        &self,
        device_id: &str,
        connection_id: u64,
        payload: serde_json::Value,
    ) -> Result<(), String> {
        let sender = self.sender_for(device_id)?;
        let (reply, result) = oneshot::channel();
        sender
            .send(BusCommand::SendSignal {
                connection_id,
                payload,
                reply,
            })
            .await
            .map_err(|_| "ExpandCast bus is closed".to_string())?;
        tokio::time::timeout(Duration::from_secs(5), result)
            .await
            .map_err(|_| "ExpandCast send signal timed out".to_string())?
            .map_err(|_| "ExpandCast send signal cancelled".to_string())?
    }

    pub async fn receive_signal(
        &self,
        device_id: &str,
        connection_id: u64,
        max: usize,
    ) -> Result<Vec<IncomingMessage>, String> {
        let sender = self.sender_for(device_id)?;
        let (reply, result) = oneshot::channel();
        sender
            .send(BusCommand::ReceiveSignal {
                connection_id,
                max,
                reply,
            })
            .await
            .map_err(|_| "ExpandCast bus is closed".to_string())?;
        tokio::time::timeout(Duration::from_secs(5), result)
            .await
            .map_err(|_| "ExpandCast receive signal timed out".to_string())?
            .map_err(|_| "ExpandCast receive signal cancelled".to_string())?
    }

    fn sender_for(&self, device_id: &str) -> Result<mpsc::Sender<BusCommand>, String> {
        let id = device_id.trim().to_ascii_uppercase();
        self.entries
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
            .cloned()
            .ok_or_else(|| "device is not connected or does not support ExpandCast".to_string())
    }
}

impl Controller {
    pub async fn wait_command(&mut self) {
        if self.pending.is_some() {
            return;
        }
        self.pending = self.receiver.recv().await;
    }

    pub fn try_next(&mut self) -> Option<BusCommand> {
        if let Some(cmd) = self.pending.take() {
            return Some(cmd);
        }
        self.receiver.try_recv().ok()
    }

    pub async fn start(&mut self, mode: u8) -> Result<StartPublication, String> {
        if !matches!(mode, 1 | 2) {
            return Err("ExpandCast mode must be 1 or 2".to_string());
        }
        if self.active {
            return Err("ExpandCast is already active".to_string());
        }
        self.tablet_res = None;

        // Start ODMF TCP signaling server on local_ip:7239
        #[cfg(test)]
        let port = self.port.unwrap_or(super::EXPAND_CAST_PORT);
        #[cfg(not(test))]
        let port = super::EXPAND_CAST_PORT;
        let server_config = ServerConfig {
            bind_ip: self.local_ip,
            peer_ip: self.peer_ip,
            port: Some(port),
        };
        let server = Server::bind_with_identity(server_config, self.identity.clone())
            .await
            .map_err(|e| format!("Failed to bind ExpandCast ODMF server: {e}"))?;
        let local_addr = server.local_addr();
        self.odmf_server = Some(server);
        self.active = true;
        self.mode = mode;

        let general_track = expand_cast_dbs_json(
            "pc_general_track",
            json!({
                "is_arm": 0,
                "gpu_vendor": "AMD",
                "extMode": mode,
                "version": "17.20.3"
            }),
        );

        let pc_device_info = expand_cast_dbs_json(
            "pc_device_info",
            json!({
                "audio_output": 0,
                "connection_type": 16,
                "local_device_id": self.local_device_id,
                "local_device_name": self.local_name,
                "local_device_type": 1,
                "local_ip": self.local_ip.to_string(),
                "mode": 1,
                "remote_device_id": self.device_id,
                "remote_device_ip": self.peer_ip.to_string(),
                "remote_device_name": self.peer_name,
                "remote_device_type": 10,
                "rotation_type": 0,
                "screen_height": 1080,
                "screen_ratio": 22526776,
                "screen_setting": 0,
                "screen_width": 1920,
                "version": "17.20.3"
            }),
        );

        let info = ExpandCastStatusInfo {
            device_id: self.device_id.clone(),
            status: "waiting_for_tablet".to_string(),
            tablet_width: None,
            tablet_height: None,
            local_addr: Some(local_addr),
            connection_id: None,
            session: None,
        };

        Ok(StartPublication {
            info,
            general_track,
            pc_device_info,
        })
    }

    pub fn mode(&self) -> u8 {
        self.mode
    }

    pub async fn stop(&mut self) -> Result<String, String> {
        self.active = false;
        self.tablet_res = None;
        if let Some(mut server) = self.odmf_server.take() {
            server.shutdown().await;
        }
        let close_msg = expand_cast_dbs_json("close_extend_window", json!({}));
        Ok(close_msg)
    }

    pub fn on_tablet_device_info(&mut self, width: u32, height: u32) -> Result<String, String> {
        self.tablet_res = Some((width, height));
        let auth_info = self.identity.advertisement().to_string();
        let pc_auth_ready = expand_cast_dbs_json(
            "pc_auth_ready",
            json!({
                "auth_data": auth_info,
                "auth_ready": true,
            }),
        );
        Ok(pc_auth_ready)
    }

    pub fn status(&self) -> ExpandCastStatusInfo {
        let (conn_id, session) = if let Some(server) = &self.odmf_server {
            (
                server.connection_ids().first().copied(),
                Some(server.status()),
            )
        } else {
            (None, None)
        };
        let status = if !self.active {
            "idle"
        } else if conn_id.is_some() {
            "connected"
        } else if self.tablet_res.is_some() {
            "ready"
        } else {
            "waiting_for_tablet"
        };
        ExpandCastStatusInfo {
            device_id: self.device_id.clone(),
            status: status.to_string(),
            tablet_width: self.tablet_res.map(|r| r.0),
            tablet_height: self.tablet_res.map(|r| r.1),
            local_addr: self.odmf_server.as_ref().map(|s| s.local_addr()),
            connection_id: conn_id,
            session,
        }
    }

    pub async fn send_signal(
        &self,
        connection_id: u64,
        payload: serde_json::Value,
    ) -> Result<(), String> {
        let server = self
            .odmf_server
            .as_ref()
            .ok_or_else(|| "ExpandCast server is not running".to_string())?;
        server
            .send(connection_id, payload)
            .await
            .map_err(|e| e.to_string())
    }

    pub fn receive_signal(
        &self,
        connection_id: u64,
        max: usize,
    ) -> Result<Vec<IncomingMessage>, String> {
        let server = self
            .odmf_server
            .as_ref()
            .ok_or_else(|| "ExpandCast server is not running".to_string())?;
        server
            .receive_batch(connection_id, max)
            .map_err(|e| e.to_string())
    }
}
pub fn expand_cast_dbs_json(msg_cmd: &str, body: serde_json::Value) -> String {
    let msg_body = if let Some(s) = body.as_str() {
        s.to_string()
    } else {
        body.to_string()
    };
    let msg = json!({
        "client_id": -1,
        "msg_cmd": msg_cmd,
        "msg_type": 0,
        "msg_value": 0,
        "msg_body": msg_body,
    });
    let kv = json!({
        "key": "ex_ability_sync",
        "value": msg.to_string(),
    });
    crate::panta::topic_data_json(super::EXPAND_CAST_TOPIC, &kv.to_string())
}
#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn start_publishes_device_info_and_tracks() {
        let registry = ExpandCastRegistry::new();
        let ip = "127.0.0.1".parse().unwrap();
        let (mut controller, _rx) = registry
            .register_bus("TEST_PAD", ip, ip, "OPPO Pad", "Linux-PC", "ABCDEF")
            .unwrap();

        // Hold a spare port to exercise the busy-port rejection.
        let probe = tokio::net::TcpListener::bind((ip, 0)).await.unwrap();
        controller.port = Some(probe.local_addr().unwrap().port());
        assert!(controller.start(1).await.is_err());
        drop(probe);
        controller.port = Some(0);

        for mode in [1, 2] {
            let publication = controller.start(mode).await.unwrap();
            assert_eq!(publication.info.status, "waiting_for_tablet");
            assert_eq!(controller.mode(), mode);
            for (message, field, expected) in [
                (&publication.general_track, "extMode", mode),
                (&publication.pc_device_info, "mode", 1),
            ] {
                let envelope: serde_json::Value = serde_json::from_str(message).unwrap();
                let kv: serde_json::Value =
                    serde_json::from_str(envelope["dbs_msg_payload"].as_str().unwrap()).unwrap();
                let command: serde_json::Value =
                    serde_json::from_str(kv["value"].as_str().unwrap()).unwrap();
                let body: serde_json::Value =
                    serde_json::from_str(command["msg_body"].as_str().unwrap()).unwrap();
                assert_eq!(body[field], expected);
            }
            controller.stop().await.unwrap();
        }
        assert!(controller.start(0).await.is_err());
        assert!(controller.start(3).await.is_err());
    }

    #[test]
    fn tablet_device_info_generates_auth_ready() {
        let registry = ExpandCastRegistry::new();
        let ip = "127.0.0.1".parse().unwrap();
        let (mut controller, _) = registry
            .register_bus("TEST_PAD", ip, ip, "OPPO Pad", "Linux-PC", "ABCDEF")
            .unwrap();

        let reply = controller.on_tablet_device_info(2520, 1680).unwrap();
        assert!(reply.contains("pc_auth_ready"));
        assert!(reply.contains("auth_ready"));
        assert!(reply.contains("auth_data"));

        let status = controller.status();
        assert_eq!(status.tablet_width, Some(2520));
        assert_eq!(status.tablet_height, Some(1680));
    }
}
