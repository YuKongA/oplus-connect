//! Explicit cast startup and private signaling control on an authenticated Panta bus.
use crate::cast::server::{IncomingMessage, Server, ServerConfig, SessionStatus};
use crate::panta;
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::{mpsc, oneshot};

const QUEUE_CAPACITY: usize = 16;
pub(crate) const CONTROL_TIMEOUT: Duration = Duration::from_secs(15);
pub(crate) const BUS_WRITE_TIMEOUT: Duration = Duration::from_secs(3);

type Reply<T> = oneshot::Sender<Result<T, String>>;

pub enum BusCommand {
    Start {
        reply: Reply<CastStatusInfo>,
    },
    Stop {
        reply: Reply<()>,
    },
    Status {
        reply: Reply<CastStatusInfo>,
    },
    Send {
        connection_id: u64,
        payload: Value,
        reply: Reply<()>,
    },
    ConfigureVideo {
        connection_id: u64,
        width: u32,
        fps: u32,
        reply: Reply<()>,
    },
    Receive {
        connection_id: u64,
        max: usize,
        reply: Reply<Vec<IncomingMessage>>,
    },
}

#[derive(Clone, Default)]
pub struct CastRegistry {
    entries: Arc<Mutex<HashMap<String, mpsc::Sender<BusCommand>>>>,
}

struct BusLease {
    registry: CastRegistry,
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

#[derive(Debug, Serialize)]
pub struct CastStatusInfo {
    pub device_id: String,
    pub local_addr: Option<SocketAddr>,
    pub status: Option<SessionStatus>,
}

pub struct StartPublication {
    pub info: CastStatusInfo,
    pub messages: [String; 4],
}

pub struct Controller {
    receiver: mpsc::Receiver<BusCommand>,
    pending: Option<BusCommand>,
    _lease: BusLease,
    device_id: String,
    peer_ip: IpAddr,
    local_ip: IpAddr,
    server: Option<Server>,
}

impl CastRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_bus(
        &self,
        device_id: &str,
        peer_ip: IpAddr,
        local_ip: IpAddr,
    ) -> Option<Controller> {
        let device_id = normalize(device_id);
        let mut entries = self.entries.lock().ok()?;
        if entries
            .get(&device_id)
            .is_some_and(|sender| !sender.is_closed())
        {
            return None;
        }
        let (sender, receiver) = mpsc::channel(QUEUE_CAPACITY);
        entries.insert(device_id.clone(), sender.clone());
        Some(Controller {
            receiver,
            pending: None,
            _lease: BusLease {
                registry: self.clone(),
                device_id: device_id.clone(),
                sender,
            },
            device_id,
            peer_ip,
            local_ip,
            server: None,
        })
    }

    async fn request<T>(
        &self,
        device_id: &str,
        command: impl FnOnce(Reply<T>) -> BusCommand,
    ) -> Result<T, String> {
        let sender = self
            .entries
            .lock()
            .map_err(|_| "cast registry unavailable")?
            .get(&normalize(device_id))
            .cloned()
            .ok_or("device has no authenticated cast bus")?;
        let (reply, result) = oneshot::channel();
        sender
            .try_send(command(reply))
            .map_err(|_| "authenticated cast bus queue is closed or full")?;
        tokio::time::timeout(CONTROL_TIMEOUT, result)
            .await
            .map_err(|_| "timed out waiting for authenticated cast bus")?
            .map_err(|_| "authenticated cast bus closed")?
    }

    pub async fn start(&self, device_id: &str) -> Result<CastStatusInfo, String> {
        self.request(device_id, |reply| BusCommand::Start { reply })
            .await
    }

    pub async fn stop(&self, device_id: &str) -> Result<(), String> {
        self.request(device_id, |reply| BusCommand::Stop { reply })
            .await
    }

    pub async fn status(&self, device_id: &str) -> Result<CastStatusInfo, String> {
        self.request(device_id, |reply| BusCommand::Status { reply })
            .await
    }

    pub async fn send(
        &self,
        device_id: &str,
        connection_id: u64,
        payload: Value,
    ) -> Result<(), String> {
        self.request(device_id, |reply| BusCommand::Send {
            connection_id,
            payload,
            reply,
        })
        .await
    }

    pub async fn configure_video(
        &self,
        device_id: &str,
        connection_id: u64,
        width: u32,
        fps: u32,
    ) -> Result<(), String> {
        self.request(device_id, |reply| BusCommand::ConfigureVideo {
            connection_id,
            width,
            fps,
            reply,
        })
        .await
    }

    pub async fn receive(
        &self,
        device_id: &str,
        connection_id: u64,
        max: usize,
    ) -> Result<Vec<IncomingMessage>, String> {
        self.request(device_id, |reply| BusCommand::Receive {
            connection_id,
            max,
            reply,
        })
        .await
    }
}

impl Controller {
    pub fn try_next(&mut self) -> Option<BusCommand> {
        self.pending.take().or_else(|| self.receiver.try_recv().ok())
    }

    pub async fn wait_command(&mut self) {
        if self.pending.is_none() {
            self.pending = self.receiver.recv().await;
        }
    }

    pub async fn start(&mut self) -> Result<StartPublication, String> {
        if self.server.is_some() {
            return Err("cast session already active".into());
        }
        let server = Server::bind(ServerConfig {
            bind_ip: self.local_ip,
            peer_ip: self.peer_ip,
        })
        .await
        .map_err(|error| error.to_string())?;
        tracing::info!(
            "cast server for {} bound {} (time sync {:?})",
            self.device_id,
            server.local_addr(),
            server.time_sync_addr()
        );
        let messages = startup_messages(server.local_addr(), server.advertisement());
        self.server = Some(server);
        Ok(StartPublication {
            info: self.status(),
            messages,
        })
    }

    pub fn status(&self) -> CastStatusInfo {
        CastStatusInfo {
            device_id: self.device_id.clone(),
            local_addr: self.server.as_ref().map(Server::local_addr),
            status: self.server.as_ref().map(Server::status),
        }
    }

    pub async fn stop(&mut self) -> Result<(), String> {
        let mut server = self.server.take().ok_or("cast session is not active")?;
        server.shutdown().await;
        Ok(())
    }

    pub async fn send(&mut self, connection_id: u64, payload: Value) -> Result<(), String> {
        self.server
            .as_mut()
            .ok_or("cast session is not active")?
            .send(connection_id, payload)
            .await
            .map_err(|error| error.to_string())
    }

    pub async fn configure_video(
        &mut self,
        connection_id: u64,
        width: u32,
        fps: u32,
    ) -> Result<(), String> {
        self.server
            .as_mut()
            .ok_or("cast session is not active")?
            .configure_video(connection_id, width, fps)
            .await
            .map_err(|error| error.to_string())
    }

    pub fn receive(
        &mut self,
        connection_id: u64,
        max: usize,
    ) -> Result<Vec<IncomingMessage>, String> {
        self.server
            .as_mut()
            .ok_or("cast session is not active")?
            .receive_batch(connection_id, max)
            .map_err(|error| error.to_string())
    }
}

fn normalize(device_id: &str) -> String {
    device_id.trim().to_ascii_uppercase()
}

fn cast_message(command: &str, body: String) -> String {
    let message =
        json!({"client_id":-1,"msg_body":body,"msg_cmd":command,"msg_type":0,"msg_value":0});
    panta::topic_data_json(
        "Oplus_Cast",
        &json!({"key":"cast_ability_sync","value":message.to_string()}).to_string(),
    )
}

fn startup_messages(address: SocketAddr, advertisement: Value) -> [String; 4] {
    // The phone starts its mirror scene as soon as it processes the capability
    // message, then dials the announced port. Announce our endpoint first so a
    // fresh identity/port is always known before that start; otherwise the peer
    // reuses the previous session endpoint and its connect fails silently.
    [
        cast_message(
            "rsp_odmf_key",
            json!({
                "client_id":1,"odmf_auth_info":advertisement.to_string(),"port":address.port(),
                "privacyProtect":false,"server_ip":address.ip().to_string()
            })
            .to_string(),
        ),
        cast_message(
            "pc_devicetype_supportodmf",
            json!({
                "audioPath":0,"castComponent":"","castMode":0,"device_type":"Win",
                "mirageFrom":1,"notifyKey":"","pc_support_odmf":true
            })
            .to_string(),
        ),
        cast_message("send_image_quality", "0".into()),
        cast_message("send_is_front_scene", "0".into()),
    ]
}

pub fn disconnect_message() -> String {
    cast_message(
        "screen_cast_disconnect",
        json!({"deviceType":"Win","screenNumber":0}).to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[tokio::test]
    async fn unknown_device_and_inactive_transport_reject_signaling() {
        let registry = CastRegistry::new();
        assert!(registry.receive("missing", 1, 1).await.is_err());
        assert!(registry.send("missing", 1, json!({})).await.is_err());
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let mut controller = registry.register_bus("phone", ip, ip).unwrap();
        assert!(controller.receive(1, 1).is_err());
        assert!(controller.send(1, json!({})).await.is_err());
    }

    #[test]
    fn bus_ownership_is_exclusive_and_drop_unregisters() {
        let registry = CastRegistry::new();
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let first = registry.register_bus(" phone ", ip, ip).unwrap();
        assert!(registry.register_bus("PHONE", ip, ip).is_none());
        drop(first);
        assert!(registry.register_bus("PHONE", ip, ip).is_some());
    }

    #[test]
    fn stale_lease_does_not_remove_replacement() {
        let registry = CastRegistry::new();
        let ip = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let mut first = registry.register_bus("phone", ip, ip).unwrap();
        first.receiver.close();
        let replacement = registry.register_bus("phone", ip, ip).unwrap();
        drop(first);
        assert!(registry.register_bus("phone", ip, ip).is_none());
        drop(replacement);
        assert!(registry.entries.lock().unwrap().is_empty());
    }
}
