#![allow(dead_code)]

pub mod assets;
pub mod client;
pub mod icons;
pub mod models;
mod persisted;
pub mod state;
pub mod theme;
pub mod views;

/// Version shown in the title bar and settings.
pub const APP_VERSION: &str = "v1.0.0";

use client::DaemonClient;
use gpui_kit::*;
use gpui_kit::component::WindowExt;
use models::*;
use state::AppState;
use std::sync::LazyLock;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::net::UnixStream;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;
use tracing::{info, warn};

pub static TOKIO_RT: LazyLock<Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Failed to initialize Tokio runtime")
});

static IN_PROCESS_DAEMON: std::sync::LazyLock<tokio::sync::Mutex<Option<oplus_connect_daemon::DaemonHandle>>> =
    std::sync::LazyLock::new(|| tokio::sync::Mutex::new(None));

pub fn shutdown_in_process_daemon() {
    TOKIO_RT.block_on(async {
        let mut handle = IN_PROCESS_DAEMON.lock().await;
        if let Some(h) = handle.take() {
            info!("Dropping in-process daemon handle and waiting for server cleanup...");
            drop(h);
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    });
}

pub struct RootView {
    pub state: AppState,
}

// Poll faster until /status proves the daemon is serving.
const STARTUP_POLL_INTERVAL: Duration = Duration::from_millis(400);
const STEADY_POLL_INTERVAL: Duration = Duration::from_secs(2);
// 25 × 400 ms ≈ 10 s without a single successful /status reply.
const STARTUP_MAX_ATTEMPTS: u8 = 25;

impl RootView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<AppEvent>();

        // Start the embedded daemon; only /status proves it is ready.
        {
            let tx = tx.clone();
            TOKIO_RT.spawn(async move {
                let daemon_state = oplus_connect_daemon::state::AppState::new();
                let result = match oplus_connect_daemon::start_daemon_service(daemon_state).await {
                    Ok(Some(handle)) => {
                        *IN_PROCESS_DAEMON.lock().await = Some(handle);
                        Ok(())
                    }
                    // Another daemon instance already owns the runtime directory.
                    Ok(None) => Ok(()),
                    Err(error) => Err(error.to_string()),
                };
                let _ = tx.send(AppEvent::DaemonStartup(result));
            });
        }

        let mut state = AppState::new();
        state.known_devices = persisted::load_known_devices();
        state.daemon_starting = true;
        let view = Self { state };

                cx.spawn(async move |this, app| {
            while let Some(event) = rx.recv().await {
                let _ = this.update_in(app, |view, window, cx| {
                    let was_pending = view.state.connection_request.is_some();
                    if view.handle_app_event(event) {
                        if was_pending && view.state.connection_request.is_none() {
                            window.close_dialog(cx);
                        }
                        if !was_pending {
                            if let Some(request) = view.state.connection_request.clone() {
                                views::confirm_modal::ConfirmModal::open(request, window, cx);
                            }
                        }
                        if view.state.known_devices_dirty {
                            view.state.known_devices_dirty = false;
                            persisted::save_known_devices(&view.state.known_devices);
                        }
                        cx.notify();
                    }
                });
            }
        })
        .detach();

                Self::start_daemon_poller(tx.clone());
        Self::start_event_stream_listener(tx.clone());

        view
    }

    fn start_daemon_poller(tx: mpsc::UnboundedSender<AppEvent>) {
        let client = DaemonClient::new();

        TOKIO_RT.spawn(async move {
            loop {
                let status = client.get_status().await.map_err(|e| e.to_string());
                let reachable = status.is_ok();
                let devices = client.get_devices().await.map_err(|e| e.to_string());
                let remote = client.get_remote_device().await.map_err(|e| e.to_string());

                let _ = tx.send(AppEvent::DaemonPolled {
                    status,
                    devices,
                    remote,
                });

                let interval = if reachable {
                    STEADY_POLL_INTERVAL
                } else {
                    STARTUP_POLL_INTERVAL
                };
                tokio::time::sleep(interval).await;
            }
        });
    }

    fn start_event_stream_listener(tx: mpsc::UnboundedSender<AppEvent>) {
        TOKIO_RT.spawn(async move {
            let socket_path = DaemonClient::event_socket_path();
            loop {
                match UnixStream::connect(&socket_path).await {
                    Ok(stream) => {
                        info!("Connected to IPC event stream at {:?}", socket_path);
                        let reader = BufReader::new(stream);
                        let mut lines = reader.lines();

                        while let Ok(Some(line)) = lines.next_line().await {
                            let trimmed = line.trim();
                            if trimmed.is_empty() {
                                continue;
                            }

                            if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) {
                                let _ = tx.send(AppEvent::IpcMessage(val));
                            }
                        }
                    }
                    Err(err) => {
                        warn!("Cannot connect to event stream: {}. Retrying in 3s...", err);
                    }
                }
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        });
    }

    pub fn handle_app_event(&mut self, event: AppEvent) -> bool {
        match event {
            AppEvent::DaemonPolled {
                status,
                devices,
                remote,
            } => {
                let mut changed = false;
                if let Ok(st) = status {
                    // /status is the only proof that startup finished.
                    self.state.daemon_startup_failures = 0;
                    if self.state.daemon_starting {
                        self.state.daemon_starting = false;
                        changed = true;
                    }
                    if self.state.daemon_running != st.running {
                        self.state.daemon_running = st.running;
                        changed = true;
                    }
                } else {
                    if self.state.daemon_starting {
                        self.state.daemon_startup_failures =
                            self.state.daemon_startup_failures.saturating_add(1);
                        if self.state.daemon_startup_failures >= STARTUP_MAX_ATTEMPTS {
                            self.state.daemon_starting = false;
                            self.state.set_toast("守护进程启动超时，请检查日志");
                            changed = true;
                        }
                    }
                    if self.state.daemon_running {
                        self.state.daemon_running = false;
                        changed = true;
                    }
                }

                if let Ok(dev_list) = devices {
                    self.state.sync_known_devices(&dev_list);
                    let first = dev_list.into_iter().next();
                    if self.state.connected_device.as_ref().map(|d| &d.device_id) != first.as_ref().map(|d| &d.device_id) {
                        self.state.connected_device = first;
                        changed = true;
                    }
                }

                match remote {
                    Ok(Some(rm)) => {
                        let is_new = self.state.remote_details.as_ref().map(|r| &r.device_id) != Some(&rm.device_id);
                        let dev = DeviceInfo {
                            device_id: rm.device_id.clone(),
                            device_name: rm.device_name.clone(),
                            device_model: rm.device_model.clone(),
                            brand: rm.brand.clone(),
                            os_version: rm.os_version.clone(),
                            battery: rm.battery_level,
                            is_charging: rm.battery_charging,
                            connect_type: rm.connect_type.clone(),
                            ip_address: Some(rm.device_address.clone()),
                            is_connected: true,
                            last_seen: chrono::Local::now().timestamp(),
                        };
                        let known_changed = self.state.remember_connected_device(&dev);
                        self.state.connected_device = Some(dev);
                        self.state.remote_details = Some(rm);
                        changed |= is_new || known_changed;
                    }
                    Ok(None) => {
                        if self.state.remote_details.take().is_some() {
                            changed = true;
                        }
                    }
                    Err(_) => {}
                }

                if self.state.connected_device.is_some() {
                    if self.state.connect_pending {
                        self.state.connect_pending = false;
                        self.state.set_toast("已连接");
                        changed = true;
                    }
                    // A stale approval dialog cannot stay open once a device is
                    // already connected; the daemon rejects its decision.
                    if self.state.connection_request.take().is_some() {
                        changed = true;
                    }
                }
                changed
            }
            AppEvent::IpcMessage(val) => {
                                let req_data = if val
                    .get("payload")
                    .and_then(|p| p.get("event"))
                    .and_then(|e| e.as_str())
                    == Some("onConnectRequested")
                {
                    val.get("payload").and_then(|p| p.get("data"))
                } else if val.get("event").and_then(|e| e.as_str()) == Some("onConnectRequested") {
                    val.get("data")
                } else {
                    None
                };

                if let Some(data) = req_data {
                    let req = ConnectionRequest {
                        device_id: data["device_id"].as_str().unwrap_or_default().to_string(),
                        device_name: data["device_name"]
                            .as_str()
                            .map(str::trim)
                            .filter(|name| !name.is_empty())
                            .unwrap_or("OPLUS 设备")
                            .to_string(),
                        device_type: data["device_type"].as_str().unwrap_or("phone").to_string(),
                        connect_type: data["connect_type"].as_str().unwrap_or("wlan").to_string(),
                        device_address: data["device_address"].as_str().unwrap_or_default().to_string(),
                        connection_id: data["connection_id"].as_str().unwrap_or_default().to_string(),
                    };
                    info!("Incoming connection approval request: {:?}", req);
                    self.state.connection_request = Some(req);
                    return true;
                }

                                let event_type = val.get("event").and_then(|e| e.as_str()).unwrap_or_default();
                match event_type {
                    "clipboard_callback" => {
                        if let Some(content) = val.get("content").and_then(|c| c.as_str()) {
                            let source_type = val.get("source").and_then(|s| s.as_str()).unwrap_or("remote");
                            let dev_name = self
                                .state
                                .connected_device
                                .as_ref()
                                .map(|d| d.device_name.clone())
                                .unwrap_or_else(|| "移动设备".to_string());
                            let source = if source_type == "local" {
                                ClipboardSource::Local
                            } else {
                                ClipboardSource::Remote(dev_name)
                            };
                            self.state.add_clipboard_record(content.to_string(), source);
                            return true;
                        }
                    }
                    "notification_callback" => {
                        if let Some(data) = val.get("data") {
                            let record = NotificationRecord {
                                id: uuid::Uuid::new_v4().to_string(),
                                key: data["key"].as_str().unwrap_or_default().to_string(),
                                label: data["label"].as_str().unwrap_or_default().to_string(),
                                title: data["title"].as_str().unwrap_or_default().to_string(),
                                content: data["content"].as_str().unwrap_or_default().to_string(),
                                icon: match data["icon"].as_str() {
                                    Some(encoded) => {
                                        let icon = crate::icons::decode_notification_icon(encoded);
                                        if icon.is_none() {
                                            warn!(
                                                "Notification icon could not be decoded ({} bytes base64)",
                                                encoded.len()
                                            );
                                        }
                                        icon
                                    }
                                    None => None,
                                },
                                icon_base64: data["icon"].as_str().map(|s| s.to_string()),
                                timestamp: chrono::Local::now(),
                            };
                            self.state.add_notification_record(record);
                            return true;
                        }
                    }
                    "notification_deleted" => {
                        if let Some(key) = val["data"]["key"].as_str() {
                            self.state.delete_notification_by_key(key);
                            return true;
                        }
                    }
                    "connect_failed" => {
                        self.state.connect_pending = false;
                        let reason = val["data"]["reason"]
                            .as_str()
                            .unwrap_or("连接失败");
                        self.state.set_toast(format!("连接失败：{reason}"));
                        return true;
                    }
                    "connect_connected" => {
                        self.state.connection_request = None;
                        if self.state.connect_pending {
                            self.state.connect_pending = false;
                            self.state.set_toast("已连接");
                        }
                        return true;
                    }
                    "connect_request_expired" => {
                        let expired_id = val["data"]["connection_id"].as_str().unwrap_or_default();
                        let matches = self
                            .state
                            .connection_request
                            .as_ref()
                            .is_some_and(|request| request.connection_id == expired_id);
                        if matches {
                            self.state.connection_request = None;
                            self.state.set_toast("请求已失效");
                            return true;
                        }
                        return false;
                    }
                    "disconnect_callback" => {
                        let disconnected_id = val["data"]["device_id"]
                            .as_str()
                            .map(str::to_string)
                            .or_else(|| {
                                self.state
                                    .connected_device
                                    .as_ref()
                                    .map(|device| device.device_id.clone())
                            });
                        self.state.mark_device_disconnected(disconnected_id.as_deref());
                        self.state.connected_device = None;
                        self.state.remote_details = None;
                        self.state.connection_request = None;
                        self.state.connect_pending = false;
                        self.state.set_toast("设备已断开");
                        return true;
                    }
                    _ => {}
                }
                false
            }
            AppEvent::Toast(msg) => {
                self.state.set_toast(msg);
                true
            }
            AppEvent::DaemonStartup(result) => {
                if let Err(error) = result {
                    self.state.daemon_starting = false;
                    self.state.daemon_running = false;
                    self.state.set_toast(format!("守护进程启动失败：{error}"));
                    true
                } else {
                    // Stay in "启动中" until /status answers.
                    false
                }
            }
            AppEvent::ConnectionRequestResolved => {
                self.state.connection_request = None;
                true
            }
        }
    }
}

impl Render for RootView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some(message) = self.state.toast_message.take() {
            window.push_notification(message, cx);
        }
        views::MainContainer::render(&self.state, window, cx)
    }
}
