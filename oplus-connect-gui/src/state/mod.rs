use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavTab {
    Clipboard,
    Notifications,
    DeviceDetail,
    Settings,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardFilter {
    All,
    Favorites,
    Local,
    Remote,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationFilter {
    All,
    Apps,
    System,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipboardSource {
    Local,
    Remote(String),
}

impl ClipboardSource {
    pub fn label(&self) -> String {
        match self {
            ClipboardSource::Local => "本机电脑".to_string(),
            ClipboardSource::Remote(dev) => dev.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipboardRecord {
    pub id: String,
    pub content: String,
    pub source: ClipboardSource,
    pub timestamp: DateTime<Local>,
    pub is_favorite: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationRecord {
    pub id: String,
    pub key: String,
    pub label: String,
    pub title: String,
    pub content: String,
    #[serde(skip)]
    pub icon: Option<std::sync::Arc<gpui_kit::Image>>,
    pub icon_base64: Option<String>,
    pub timestamp: DateTime<Local>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConnectionRequest {
    pub device_id: String,
    pub device_name: String,
    pub device_type: String,
    pub connect_type: String,
    pub device_address: String,
    pub connection_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct DeviceInfo {
    pub device_id: String,
    pub device_name: String,
    pub device_model: String,
    pub brand: String,
    pub os_version: String,
    pub battery: u8,
    pub is_charging: bool,
    pub connect_type: String,
    pub ip_address: Option<String>,
    pub is_connected: bool,
    pub last_seen: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonStatus {
    pub active_devices: usize,
    pub daemon_version: String,
    pub running: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteDevicePayload {
    pub device_id: String,
    pub device_name: String,
    pub device_model: String,
    pub brand: String,
    pub device_type: String,
    pub os_version: String,
    pub battery_level: u8,
    pub battery_charging: bool,
    pub connect_type: String,
    pub lan_ip: String,
    pub local_ip: String,
    pub device_address: String,
    pub support_link_change: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CastStatus {
    pub device_id: String,
    pub local_addr: Option<std::net::SocketAddr>,
    pub status: Option<CastSessionStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CastSessionStatus {
    pub state: String,
    #[serde(default)]
    pub connection_ids: Vec<u64>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum CastIncoming {
    Signaling(serde_json::Value),
    Control(serde_json::Value),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastPhase {
    Idle,
    Probing,
    Starting,
    Waiting,
    Negotiating,
    Displaying,
    Stopping,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CastViewState {
    pub device_id: String,
    pub generation: u64,
    pub phase: CastPhase,
    pub detail: String,
}

impl Default for CastViewState {
    fn default() -> Self {
        Self { device_id: String::new(), generation: 0, phase: CastPhase::Idle, detail: "尚未投屏".into() }
    }
}

impl CastViewState {
    pub fn active(&self) -> bool {
        !matches!(self.phase, CastPhase::Idle | CastPhase::Failed)
    }

    pub fn accepts_frames(&self, generation: u64) -> bool {
        self.generation == generation && self.active() && self.phase != CastPhase::Stopping
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CastEvent {
    Phase(CastPhase),
    Displaying { width: u32, height: u32 },
    Error(String),
    Closed,
}

#[derive(Debug)]
pub enum AppEvent {
    DaemonPolled {
        status: Result<DaemonStatus, String>,
        devices: Result<Vec<DeviceInfo>, String>,
        remote: Result<Option<RemoteDevicePayload>, String>,
    },
    IpcMessage(serde_json::Value),
    Toast(String),
    DaemonStartup(Result<(), String>),
    ConnectionRequestResolved,
    CastChanged { generation: u64, event: CastEvent },
}

pub struct AppState {
    pub connected_device: Option<DeviceInfo>,
    pub cast: CastViewState,
    // Devices seen so far, connected first; persisted beside ksc.bin.
    pub known_devices: Vec<DeviceInfo>,
    pub known_devices_dirty: bool,
    pub clipboard_history: Vec<ClipboardRecord>,
    pub notification_history: Vec<NotificationRecord>,
    pub connection_request: Option<ConnectionRequest>,
    pub active_tab: NavTab,
    // Consumed once by RootView for the Kit notification stack.
    pub toast_message: Option<String>,

        pub daemon_running: bool,
    // Set until /status answers or startup fails/times out.
    pub daemon_starting: bool,
    // True while a PC-initiated LAN connection waits for the phone/user.
    pub connect_pending: bool,
    // Bounds startup when a daemon exists but never answers IPC.
    pub daemon_startup_failures: u8,
    pub remote_details: Option<RemoteDevicePayload>,
    pub search_query: String,
    pub clipboard_filter: ClipboardFilter,
    pub last_copied_id: Option<String>,
    /// Mirrors the settings page; the cast session reads these.
    pub cast_audio: bool,
    pub cast_input: bool,
    pub cast_h265: bool,
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

impl AppState {
    pub fn new() -> Self {
        Self {
            connected_device: None,
            cast: CastViewState::default(),
            known_devices: Vec::new(),
            known_devices_dirty: false,
            clipboard_history: Vec::new(),
            notification_history: Vec::new(),
            connection_request: None,
            active_tab: NavTab::DeviceDetail,
            toast_message: None,
            daemon_running: false,
            daemon_starting: false,
            connect_pending: false,
            daemon_startup_failures: 0,
            remote_details: None,
            search_query: String::new(),
            clipboard_filter: ClipboardFilter::All,
            last_copied_id: None,
            cast_audio: true,
            cast_input: true,
            cast_h265: true,
        }
    }

    pub fn set_toast(&mut self, message: impl Into<String>) {
        self.toast_message = Some(message.into());
    }

    /// Marks absent devices offline and upserts live ones. Only identity,
    /// address or connection changes dirty the cache, never battery polls.
    pub fn sync_known_devices(&mut self, devices: &[DeviceInfo]) -> bool {
        let mut changed = false;

        for known in &mut self.known_devices {
            if known.is_connected
                && !devices
                    .iter()
                    .any(|device| device.device_id == known.device_id)
            {
                known.is_connected = false;
                known.battery = 0;
                known.is_charging = false;
                self.known_devices_dirty = true;
                changed = true;
            }
        }

        for device in devices {
            changed |= self.remember_connected_device(device);
        }
        self.sort_known_devices();
        changed
    }

    /// Upserts one connected device; returns whether visible fields changed.
    pub fn remember_connected_device(&mut self, device: &DeviceInfo) -> bool {
        if device.device_id.is_empty() && device.device_name.is_empty() {
            return false;
        }

        let mut incoming = device.clone();
        incoming.is_connected = true;
        incoming.last_seen = Local::now().timestamp();

        let Some(index) = self
            .known_devices
            .iter()
            .position(|known| known.device_id == incoming.device_id)
        else {
            self.known_devices.push(incoming);
            self.known_devices_dirty = true;
            self.sort_known_devices();
            return true;
        };

        let known = &mut self.known_devices[index];
        let persist = known.device_name != incoming.device_name
            || known.device_model != incoming.device_model
            || known.brand != incoming.brand
            || known.os_version != incoming.os_version
            || known.connect_type != incoming.connect_type
            || known.ip_address != incoming.ip_address
            || !known.is_connected;
        let changed = persist
            || known.battery != incoming.battery
            || known.is_charging != incoming.is_charging;

        *known = incoming;
        if persist {
            self.known_devices_dirty = true;
        }
        changed
    }

    /// Marks one device offline, or all when no id is given.
    pub fn mark_device_disconnected(&mut self, device_id: Option<&str>) -> bool {
        let mut changed = false;
        for known in &mut self.known_devices {
            if !known.is_connected {
                continue;
            }
            if device_id.is_some_and(|id| known.device_id != id) {
                continue;
            }
            known.is_connected = false;
            known.battery = 0;
            known.is_charging = false;
            changed = true;
        }
        if changed {
            self.known_devices_dirty = true;
        }
        changed
    }

    fn sort_known_devices(&mut self) {
        self.known_devices.sort_by(|left, right| {
            right
                .is_connected
                .cmp(&left.is_connected)
                .then_with(|| right.last_seen.cmp(&left.last_seen))
                .then_with(|| left.device_id.cmp(&right.device_id))
        });
    }

    pub fn add_clipboard_record(&mut self, content: String, source: ClipboardSource) {
        let trimmed = content.trim();
        if trimmed.is_empty() {
            return;
        }

        if let Some(first) = self.clipboard_history.first() {
            if first.content == content {
                return;
            }
        }

        let record = ClipboardRecord {
            id: Uuid::new_v4().to_string(),
            content,
            source,
            timestamp: Local::now(),
            is_favorite: false,
        };

        self.clipboard_history.insert(0, record);
        if self.clipboard_history.len() > 200 {
            self.clipboard_history.truncate(200);
        }
    }

    pub fn toggle_clipboard_favorite(&mut self, id: &str) {
        if let Some(item) = self.clipboard_history.iter_mut().find(|r| r.id == id) {
            item.is_favorite = !item.is_favorite;
        }
    }

    pub fn delete_clipboard_record(&mut self, id: &str) {
        self.clipboard_history.retain(|r| r.id != id);
    }

    pub fn clear_clipboard_records(&mut self) {
        self.clipboard_history.retain(|r| r.is_favorite);
        self.set_toast("已清空，收藏保留");
    }

    pub fn add_notification_record(&mut self, mut record: NotificationRecord) {
        if let Some(pos) = self.notification_history.iter().position(|r| r.key == record.key) {
            let previous = self.notification_history.remove(pos);
            // A same-key update often carries only text. Keep the app icon the
            // phone sent with the original Add instead of dropping it.
            if record.icon.is_none() {
                record.icon = previous.icon;
            }
            if record.icon_base64.is_none() {
                record.icon_base64 = previous.icon_base64;
            }
        }
        self.notification_history.insert(0, record);
        if self.notification_history.len() > 200 {
            self.notification_history.truncate(200);
        }
    }

    pub fn delete_notification_record(&mut self, id: &str) {
        self.notification_history.retain(|r| r.id != id);
    }

    pub fn delete_notification_by_key(&mut self, key: &str) {
        self.notification_history.retain(|r| r.key != key);
    }

    pub fn clear_notifications(&mut self) {
        self.notification_history.clear();
        self.set_toast("已清空");
    }

    pub fn filtered_clipboard_records(&self) -> Vec<&ClipboardRecord> {
        let query = self.search_query.trim().to_lowercase();
        self.clipboard_history
            .iter()
            .filter(|r| {
                let match_filter = match self.clipboard_filter {
                    ClipboardFilter::All => true,
                    ClipboardFilter::Favorites => r.is_favorite,
                    ClipboardFilter::Local => matches!(r.source, ClipboardSource::Local),
                    ClipboardFilter::Remote => matches!(r.source, ClipboardSource::Remote(_)),
                };
                if !match_filter {
                    return false;
                }
                if query.is_empty() {
                    true
                } else {
                    r.content.to_lowercase().contains(&query)
                }
            })
            .collect()
    }

    pub fn filtered_notification_records(&self) -> Vec<&NotificationRecord> {
        let query = self.search_query.trim().to_lowercase();
        self.notification_history
            .iter()
            .filter(|r| {
                if query.is_empty() {
                    true
                } else {
                    r.title.to_lowercase().contains(&query)
                        || r.content.to_lowercase().contains(&query)
                        || r.label.to_lowercase().contains(&query)
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod cast_tests {
    use super::{CastPhase, CastViewState};

    #[test]
    fn stopped_and_old_generations_cannot_reopen_the_cast_window() {
        let mut state = CastViewState { generation: 7, phase: CastPhase::Negotiating, ..Default::default() };
        assert!(state.accepts_frames(7));
        assert!(!state.accepts_frames(6));
        for phase in [CastPhase::Stopping, CastPhase::Idle, CastPhase::Failed] {
            state.phase = phase;
            assert!(!state.accepts_frames(7));
        }
    }
}

#[cfg(test)]
mod notification_icon_tests {
    use super::*;
    use std::sync::Arc;

    fn record(key: &str, icon: Option<Arc<gpui_kit::Image>>) -> NotificationRecord {
        let icon_base64 = icon.as_ref().map(|_| "cG5n".to_string());
        NotificationRecord {
            id: key.to_string(),
            key: key.to_string(),
            label: "抖音".into(),
            title: "标题".into(),
            content: "内容".into(),
            icon,
            icon_base64,
            timestamp: Local::now(),
        }
    }

    #[test]
    fn same_key_text_update_keeps_the_app_icon() {
        let mut state = AppState::new();
        let icon = Arc::new(gpui_kit::Image::from_bytes(
            gpui_kit::ImageFormat::Png,
            b"\x89PNG\r\n\x1a\nicon".to_vec(),
        ));
        state.add_notification_record(record("same", Some(icon.clone())));
        state.add_notification_record(record("same", None));

        assert_eq!(state.notification_history.len(), 1);
        assert!(state.notification_history[0].icon.is_some());
        assert_eq!(state.notification_history[0].icon_base64.as_deref(), Some("cG5n"));
    }

    #[test]
    fn new_key_without_icon_has_no_inherited_icon() {
        let mut state = AppState::new();
        state.add_notification_record(record("first", None));
        assert!(state.notification_history[0].icon.is_none());
    }
}
