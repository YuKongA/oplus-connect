//! KMS control ownership shared by authenticated Panta sessions and IPC.
use crate::kms::input::InputEvent;
use crate::kms::server::{Server, ServerConfig, SessionStatus};
use crate::panta;
use serde::Serialize;
use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::{Arc, Mutex},
};
use tokio::sync::{mpsc, oneshot};

const QUEUE_CAPACITY: usize = 16;

pub enum BusCommand {
    Start {
        config: ServerConfig,
        reply: oneshot::Sender<Result<KmsStartInfo, String>>,
    },
    Stop {
        reply: oneshot::Sender<Result<(), String>>,
    },
    Status {
        reply: oneshot::Sender<KmsStatusInfo>,
    },
    Input {
        generation: u64,
        event: InputEvent,
        reply: oneshot::Sender<Result<(), String>>,
    },
    Release {
        generation: u64,
        reply: oneshot::Sender<Result<(), String>>,
    },
}

struct Entry {
    generation: u64,
    sender: mpsc::Sender<BusCommand>,
    peer_ip: IpAddr,
    local_ip: IpAddr,
}

#[derive(Clone)]
pub struct KmsRegistry {
    inner: Arc<Mutex<HashMap<String, Entry>>>,
    generations: Arc<Mutex<HashMap<String, u64>>>,
}

pub struct BusRegistration {
    pub generation: u64,
    pub receiver: mpsc::Receiver<BusCommand>,
    lease: BusLease,
    pub peer_ip: IpAddr,
    pub local_ip: IpAddr,
}

struct BusLease {
    registry: KmsRegistry,
    device_id: String,
    generation: u64,
}

impl Drop for BusLease {
    fn drop(&mut self) {
        let mut entries = self.registry.inner.lock().expect("KMS registry lock");
        if entries
            .get(&self.device_id)
            .is_some_and(|e| e.generation == self.generation)
        {
            entries.remove(&self.device_id);
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct KmsStartInfo {
    pub device_id: String,
    pub local_addr: SocketAddr,
    pub status: SessionStatus,
}

#[derive(Clone, Debug, Serialize)]
pub struct KmsStatusInfo {
    pub device_id: String,
    pub connection_id: Option<u64>,
    pub status: Option<SessionStatus>,
    pub local_addr: Option<SocketAddr>,
}

pub struct Controller {
    receiver: mpsc::Receiver<BusCommand>,
    pending: Option<BusCommand>,
    _lease: BusLease,
    server: Option<Server>,
    device_id: String,
    peer_ip: IpAddr,
    local_ip: IpAddr,
    peer_name: String,
    peer_type: u8,
    local_name: String,
    local_device_id: String,
}

pub struct StartPublication {
    pub info: KmsStartInfo,
    pub device_info: String,
}

impl KmsRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            generations: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn register_bus(
        &self,
        device_id: &str,
        peer_ip: IpAddr,
        local_ip: IpAddr,
    ) -> Option<BusRegistration> {
        let id = normalize(device_id);
        let (sender, receiver) = mpsc::channel(QUEUE_CAPACITY);
        let mut entries = self.inner.lock().expect("KMS registry lock");
        if entries.contains_key(&id) {
            return None;
        }
        let generation = {
            let mut generations = self.generations.lock().expect("KMS generation lock");
            let generation = generations.entry(id.clone()).or_insert(0);
            *generation = generation.wrapping_add(1);
            *generation
        };
        entries.insert(
            id.clone(),
            Entry {
                generation,
                sender,
                peer_ip,
                local_ip,
            },
        );
        let lease = BusLease {
            registry: self.clone(),
            device_id: id,
            generation,
        };
        Some(BusRegistration {
            generation,
            receiver,
            lease,
            peer_ip,
            local_ip,
        })
    }

    pub fn command_sender(&self, device_id: &str) -> Option<mpsc::Sender<BusCommand>> {
        self.inner
            .lock()
            .ok()?
            .get(&normalize(device_id))
            .map(|e| e.sender.clone())
    }

    fn sender_for_device(&self, device_id: &str) -> Result<mpsc::Sender<BusCommand>, String> {
        let entries = self
            .inner
            .lock()
            .map_err(|_| "KMS registry unavailable".to_string())?;
        let entry = entries
            .get(&normalize(device_id))
            .ok_or_else(|| "device is not connected".to_string())?;
        Ok(entry.sender.clone())
    }

    pub async fn send_input(
        &self,
        device_id: &str,
        connection_id: u64,
        event: InputEvent,
    ) -> Result<(), String> {
        let sender = self.sender_for_device(device_id)?;
        let (reply, result) = oneshot::channel();
        sender
            .try_send(BusCommand::Input {
                generation: connection_id,
                event,
                reply,
            })
            .map_err(|_| "authenticated Panta bus control queue is closed or full".to_string())?;
        tokio::time::timeout(crate::kms::server::WRITE_TIMEOUT, result)
            .await
            .map_err(|_| "timed out waiting for authenticated Panta bus".to_string())?
            .map_err(|_| "authenticated Panta bus closed".to_string())?
    }

    pub async fn release_input(&self, device_id: &str, connection_id: u64) -> Result<(), String> {
        let sender = self.sender_for_device(device_id)?;
        let (reply, result) = oneshot::channel();
        sender
            .try_send(BusCommand::Release {
                generation: connection_id,
                reply,
            })
            .map_err(|_| "authenticated Panta bus control queue is closed or full".to_string())?;
        tokio::time::timeout(crate::kms::server::WRITE_TIMEOUT, result)
            .await
            .map_err(|_| "timed out waiting for authenticated Panta bus".to_string())?
            .map_err(|_| "authenticated Panta bus closed".to_string())?
    }

    pub async fn start_for_device(
        &self,
        device_id: &str,
        width: u32,
        height: u32,
        direction: u8,
    ) -> Result<KmsStartInfo, String> {
        let id = normalize(device_id);
        let (sender, peer_ip, local_ip) = {
            let entries = self.inner.lock().map_err(|_| "KMS registry unavailable")?;
            let entry = entries
                .get(&id)
                .ok_or_else(|| "device is not connected".to_string())?;
            (entry.sender.clone(), entry.peer_ip, entry.local_ip)
        };
        let (reply, result) = oneshot::channel();
        sender
            .try_send(BusCommand::Start {
                config: ServerConfig {
                    device_id: id,
                    peer_ip,
                    bind_ip: local_ip,
                    width,
                    height,
                    direction,
                },
                reply,
            })
            .map_err(|_| "authenticated Panta bus control queue is closed or full".to_string())?;
        tokio::time::timeout(crate::kms::server::WRITE_TIMEOUT, result)
            .await
            .map_err(|_| "timed out waiting for authenticated Panta bus".to_string())?
            .map_err(|_| "authenticated Panta bus closed".to_string())?
    }

    pub async fn stop(&self, device_id: &str) -> Result<(), String> {
        let sender = self
            .command_sender(device_id)
            .ok_or_else(|| "device is not connected".to_string())?;
        let (reply, result) = oneshot::channel();
        sender
            .try_send(BusCommand::Stop { reply })
            .map_err(|_| "authenticated Panta bus control queue is closed or full".to_string())?;
        tokio::time::timeout(crate::kms::server::WRITE_TIMEOUT, result)
            .await
            .map_err(|_| "timed out waiting for authenticated Panta bus".to_string())?
            .map_err(|_| "authenticated Panta bus closed".to_string())?
    }

    pub async fn status(&self, device_id: &str) -> Result<KmsStatusInfo, String> {
        let id = normalize(device_id);
        let sender = self
            .command_sender(&id)
            .ok_or_else(|| "device is not connected".to_string())?;
        let (reply, result) = oneshot::channel();
        sender
            .try_send(BusCommand::Status { reply })
            .map_err(|_| "authenticated Panta bus control queue is closed or full".to_string())?;
        tokio::time::timeout(crate::kms::server::WRITE_TIMEOUT, result)
            .await
            .map_err(|_| "timed out waiting for authenticated Panta bus".to_string())?
            .map_err(|_| "authenticated Panta bus closed".to_string())
    }
}

impl BusRegistration {
    pub fn into_controller(
        self,
        device_id: String,
        peer_name: String,
        peer_type: u8,
        local_name: String,
        local_device_id: String,
    ) -> Controller {
        Controller {
            receiver: self.receiver,
            pending: None,
            _lease: self.lease,
            server: None,
            device_id: normalize(&device_id),
            peer_ip: self.peer_ip,
            local_ip: self.local_ip,
            peer_name,
            peer_type,
            local_name,
            local_device_id,
        }
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
    pub async fn start(&mut self, config: ServerConfig) -> Result<StartPublication, String> {
        if self.server.is_some() {
            return Err("KMS session already active".to_string());
        }
        if normalize(&config.device_id) != self.device_id
            || config.peer_ip != self.peer_ip
            || config.bind_ip != self.local_ip
        {
            return Err("KMS device owner mismatch".to_string());
        }
        let server = Server::bind(config).await.map_err(|e| e.to_string())?;
        let info = KmsStartInfo {
            device_id: self.device_id.clone(),
            local_addr: server.local_addr(),
            status: server.status(),
        };
        let device_info = device_info_json(
            &self.device_id,
            self.peer_ip,
            self.local_ip,
            &self.peer_name,
            self.peer_type,
            &self.local_name,
            &self.local_device_id,
            &server,
        );
        self.server = Some(server);
        Ok(StartPublication {
            info,
            device_info,
        })
    }
    pub async fn send_input(
        &mut self,
        connection_id: u64,
        event: InputEvent,
    ) -> Result<(), String> {
        let server = self
            .server
            .as_ref()
            .ok_or_else(|| "KMS session is not active".to_string())?;
        if server.connection_id() != Some(connection_id) {
            return Err("KMS connection ID is stale".to_string());
        }
        server
            .send_input_for_generation(connection_id, event)
            .await
            .map_err(|error| format!("KMS input write failed: {error}"))
    }

    pub async fn release_input(&mut self, connection_id: u64) -> Result<(), String> {
        let server = self
            .server
            .as_ref()
            .ok_or_else(|| "KMS session is not active".to_string())?;
        if server.connection_id() != Some(connection_id) {
            return Err("KMS connection ID is stale".to_string());
        }
        server
            .release_input_for_generation(connection_id)
            .await
            .map_err(|error| format!("KMS input release failed: {error}"))
    }

    pub async fn stop(&mut self) -> Result<(), String> {
        let Some(mut server) = self.server.take() else {
            return Err("KMS session is not active".to_string());
        };
        server.shutdown().await;
        Ok(())
    }
    pub fn status(&self) -> KmsStatusInfo {
        KmsStatusInfo {
            device_id: self.device_id.clone(),
            connection_id: self.server.as_ref().and_then(Server::connection_id),
            status: self.server.as_ref().map(Server::status),
            local_addr: self.server.as_ref().map(Server::local_addr),
        }
    }
    pub async fn abort(&mut self) {
        if let Some(mut server) = self.server.take() {
            server.shutdown().await;
        }
    }
}

fn device_info_json(
    device_id: &str,
    peer_ip: IpAddr,
    local_ip: IpAddr,
    peer_name: &str,
    peer_type: u8,
    local_name: &str,
    local_device_id: &str,
    server: &Server,
) -> String {
    let body = serde_json::json!({"connection_type":16,"local_device_id":local_device_id,"local_device_name":local_name,"local_device_type":1,"local_ip":local_ip.to_string(),"mode":0,"randomPwd":server.password(),"remote_device_id":device_id,"remote_device_ip":peer_ip.to_string(),"remote_device_name":peer_name,"remote_device_type":peer_type,"socket_port":server.local_addr().port(),"testData":server.test_data(),"version":"17.20.3"});
    let msg = serde_json::json!({"client_id":-1,"msg_body":body.to_string(),"msg_cmd":"pc_device_info","msg_type":0,"msg_value":0});
    panta::topic_data_json(
        "Oplus_KeyMouseLink",
        &serde_json::json!({"key":"kms_ability_sync","value":msg.to_string()}).to_string(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr};

    #[test]
    fn duplicate_owner_rejected_and_drop_releases_generation() {
        let registry = KmsRegistry::new();
        let first = registry
            .register_bus(
                "aabbccddeeff",
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                IpAddr::V4(Ipv4Addr::LOCALHOST),
            )
            .unwrap();
        assert!(
            registry
                .register_bus(
                    "AABBCCDDEEFF",
                    IpAddr::V4(Ipv4Addr::LOCALHOST),
                    IpAddr::V4(Ipv4Addr::LOCALHOST)
                )
                .is_none()
        );
        drop(first);
        assert!(
            registry
                .register_bus(
                    "AABBCCDDEEFF",
                    IpAddr::V4(Ipv4Addr::LOCALHOST),
                    IpAddr::V4(Ipv4Addr::LOCALHOST)
                )
                .is_some()
        );
    }

    #[tokio::test]
    async fn canceled_start_is_observable_before_server_creation() {
        let registry = KmsRegistry::new();
        let registration = registry
            .register_bus(
                "aabbccddeeff",
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                IpAddr::V4(Ipv4Addr::LOCALHOST),
            )
            .unwrap();
        let sender = registry.command_sender("AABBCCDDEEFF").unwrap();
        let (reply, receiver) = oneshot::channel();
        drop(receiver);
        sender
            .try_send(BusCommand::Start {
                config: ServerConfig {
                    device_id: "AABBCCDDEEFF".into(),
                    peer_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
                    bind_ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
                    width: 100,
                    height: 100,
                    direction: 1,
                },
                reply,
            })
            .unwrap();
        let mut controller = registration.into_controller(
            "AABBCCDDEEFF".into(),
            String::new(),
            0,
            String::new(),
            String::new(),
        );
        let Some(BusCommand::Start { reply, .. }) = controller.try_next() else {
            panic!("missing queued start")
        };
        assert!(reply.is_closed());
    }
    #[tokio::test]
    async fn input_rejects_unknown_authenticated_device() {
        let registry = KmsRegistry::new();
        let error = registry
            .send_input(
                "AABBCCDDEEFF",
                1,
                InputEvent::Move {
                    delta_x: 1,
                    delta_y: 0,
                },
            )
            .await
            .unwrap_err();
        assert!(error.contains("not connected"));
    }

    #[tokio::test]
    async fn canceled_input_command_is_skipped_by_controller() {
        let registry = KmsRegistry::new();
        let registration = registry
            .register_bus(
                "aabbccddeeff",
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                IpAddr::V4(Ipv4Addr::LOCALHOST),
            )
            .unwrap();
        let sender = registry.command_sender("aabbccddeeff").unwrap();
        let (reply, receiver) = oneshot::channel();
        drop(receiver);
        sender
            .try_send(BusCommand::Input {
                generation: registration.generation,
                event: InputEvent::Move {
                    delta_x: 1,
                    delta_y: 0,
                },
                reply,
            })
            .unwrap();
        let mut controller = registration.into_controller(
            "AABBCCDDEEFF".into(),
            String::new(),
            0,
            String::new(),
            String::new(),
        );
        let Some(BusCommand::Input { reply, .. }) = controller.try_next() else {
            panic!("missing queued input")
        };
        assert!(reply.is_closed());
    }
}
fn normalize(id: &str) -> String {
    id.trim().to_ascii_uppercase()
}
