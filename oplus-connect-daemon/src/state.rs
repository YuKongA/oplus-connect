use crate::cast::control::CastRegistry;
use crate::kms::control::KmsRegistry;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::{RwLock, broadcast};

static NEXT_LIFECYCLE_TOKEN: AtomicU64 = AtomicU64::new(1);

fn next_lifecycle_token() -> u64 {
    loop {
        let token = NEXT_LIFECYCLE_TOKEN.fetch_add(1, Ordering::Relaxed);
        if token != 0 {
            return token;
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceInfo {
    pub device_id: String,
    pub device_name: String,
    pub device_model: String,
    #[serde(default)]
    pub device_type: String,
    pub brand: String,
    pub os_version: String,
    pub battery: u8,
    pub is_charging: bool,
    pub connect_type: String, // "usb" or "wifi"
    pub ip_address: Option<String>,
    pub is_connected: bool,
    pub last_seen: u64,
}

/// SCP Sync field 4 and EPI tag 10 share the captured phone/tablet categories.
pub fn device_type_name(category: u64) -> &'static str {
    match category {
        8 => "phone",
        10 => "tablet",
        _ => "unknown",
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NotificationItem {
    pub key: String,
    pub label: String,
    pub title: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipboardItem {
    pub r#type: String, // "text"
    pub source: String, // "local" or "remote"
    pub content: String,
    pub timestamp: u64,
}

#[derive(Clone)]
pub struct AppState {
    pub devices: Arc<RwLock<HashMap<String, DeviceInfo>>>,
    pub active_device_id: Arc<RwLock<Option<String>>>,
    pub event_sender: broadcast::Sender<String>,
    pub notifications: Arc<RwLock<Vec<NotificationItem>>>,
    pub clipboard_history: Arc<RwLock<Vec<ClipboardItem>>>,
    pub last_remote_clipboard: Arc<RwLock<Option<String>>>,
    // Authenticated data channels per device id. The SCP handshake socket is
    // closed by the phone right after SyncSecure, so only these channels prove
    // the connection is still alive.
    device_sessions: Arc<RwLock<HashMap<String, HashSet<u64>>>>,
    disconnect_requests: Arc<RwLock<HashSet<u64>>>,
    pending_removals: Arc<RwLock<HashMap<String, u64>>>,
    pub kms: Arc<KmsRegistry>,
    pub cast: Arc<CastRegistry>,
}

impl AppState {
    pub fn new() -> Self {
        let (event_sender, _) = broadcast::channel(128);

        Self {
            devices: Arc::new(RwLock::new(HashMap::new())),
            active_device_id: Arc::new(RwLock::new(None)),
            event_sender,
            notifications: Arc::new(RwLock::new(Vec::new())),
            clipboard_history: Arc::new(RwLock::new(Vec::new())),
            last_remote_clipboard: Arc::new(RwLock::new(None)),
            device_sessions: Arc::new(RwLock::new(HashMap::new())),
            disconnect_requests: Arc::new(RwLock::new(HashSet::new())),
            pending_removals: Arc::new(RwLock::new(HashMap::new())),
            kms: Arc::new(KmsRegistry::new()),
            cast: Arc::new(CastRegistry::new()),
        }
    }

    pub async fn get_active_device(&self) -> Option<DeviceInfo> {
        let active_id = self.active_device_id.read().await;
        if let Some(id) = active_id.as_ref() {
            let devs = self.devices.read().await;
            devs.get(id).cloned()
        } else {
            let devs = self.devices.read().await;
            devs.values().next().cloned()
        }
    }

    pub async fn upsert_device(&self, device: DeviceInfo) {
        let mut devs = self.devices.write().await;
        let id = device.device_id.clone();
        let was_empty = devs.is_empty();
        devs.insert(id.clone(), device);
        drop(devs);

        if was_empty {
            let mut active = self.active_device_id.write().await;
            *active = Some(id);
        }
    }

    pub async fn remove_device(&self, device_id: &str) {
        let mut devs = self.devices.write().await;
        devs.remove(device_id);
        drop(devs);

        let mut active = self.active_device_id.write().await;
        if active.as_deref() == Some(device_id) {
            *active = None;
        }
    }

    /// Registers an authenticated data channel for `device_id`. Cancels any
    /// pending removal scheduled when the SCP handshake socket closed.
    /// Device ids are case-insensitive across SCP (`SyncSecure`) and panta
    /// (`AUTH_REQUEST`), so keys are normalized before every lookup.
    pub async fn attach_data_session(&self, device_id: &str) -> u64 {
        let device_id = device_id.to_ascii_uppercase();
        let token = next_lifecycle_token();
        let mut sessions = self.device_sessions.write().await;
        sessions.entry(device_id.clone()).or_default().insert(token);
        self.pending_removals.write().await.remove(&device_id);
        token
    }

    /// Drops one authenticated data channel for `device_id`. The device is only
    /// considered disconnected when the last data channel closes: the phone
    /// closes the SCP handshake socket right after `SyncSecure`, while clipboard
    /// and notifications keep flowing over the data channel.
    /// Returns true when the device was removed.
    pub async fn detach_data_session(&self, device_id: &str, token: u64) -> bool {
        let device_id = device_id.to_ascii_uppercase();
        let mut sessions = self.device_sessions.write().await;
        let Some(tokens) = sessions.get_mut(&device_id) else {
            return false;
        };
        if !tokens.remove(&token) {
            return false;
        }
        self.disconnect_requests.write().await.remove(&token);
        if !tokens.is_empty() {
            return false;
        }
        sessions.remove(&device_id);
        // Serialize removal with new attachments so an old close cannot remove
        // the replacement session's device.
        self.finish_disconnect(&device_id).await
    }

    /// Closes every data session active at the time of the request, not a later
    /// reconnect. The last detach removes the device and broadcasts its removal.
    pub async fn request_disconnect(&self, device_id: &str) {
        let sessions = self.device_sessions.read().await;
        if let Some(tokens) = sessions.get(&device_id.to_ascii_uppercase()) {
            self.disconnect_requests.write().await.extend(tokens);
        }
    }

    pub async fn take_disconnect_request(&self, token: u64) -> bool {
        self.disconnect_requests.write().await.remove(&token)
    }

    /// Schedules removal after the SCP handshake socket closes. The data channel
    /// may still be connecting, so removal waits briefly; `attach_data_session`
    /// cancels it. A handshake that never opens a data channel is cleaned up.
    pub async fn schedule_device_removal(&self, device_id: &str, delay: std::time::Duration) {
        let device_id = device_id.to_ascii_uppercase();
        let sessions = self.device_sessions.read().await;
        if sessions.contains_key(&device_id) {
            return;
        }
        let generation = next_lifecycle_token();
        self.pending_removals
            .write()
            .await
            .insert(device_id.clone(), generation);
        drop(sessions);
        let state = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(delay).await;
            let sessions = state.device_sessions.read().await;
            if sessions.contains_key(&device_id) {
                return;
            }
            let mut pending = state.pending_removals.write().await;
            if pending.get(&device_id) != Some(&generation) {
                return;
            }
            pending.remove(&device_id);
            drop(pending);
            state.finish_disconnect(&device_id).await;
        });
    }

    async fn finish_disconnect(&self, device_id: &str) -> bool {
        let device_id = device_id.to_ascii_uppercase();
        self.pending_removals.write().await.remove(&device_id);
        let existed = self.devices.read().await.contains_key(&device_id);
        self.remove_device(&device_id).await;
        if !existed {
            return false;
        }
        tracing::info!(
            "device {} disconnected: last data channel closed, removed from /devices",
            device_id
        );
        let event = serde_json::json!({
            "event": "disconnect_callback",
            "data": { "device_id": device_id }
        });
        self.broadcast_event(&event.to_string());
        true
    }

    pub fn broadcast_event(&self, event_json: &str) {
        let _ = self.event_sender.send(event_json.to_string());
        self.handle_internal_event(event_json);
    }

    fn handle_internal_event(&self, event_json: &str) {
        let Ok(val) = serde_json::from_str::<serde_json::Value>(event_json) else {
            return;
        };
        let event_type = val
            .get("event")
            .and_then(|v| v.as_str())
            .unwrap_or_default();
        match event_type {
            "notification_callback" => {
                if let Some(data) = val.get("data") {
                    let key = data
                        .get("key")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let label = data
                        .get("label")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let title = data
                        .get("title")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let content = data
                        .get("content")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let icon = data
                        .get("icon")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    let timestamp = data
                        .get("timestamp")
                        .and_then(|v| v.as_u64())
                        .unwrap_or_else(|| {
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs()
                        });
                    if !key.is_empty() {
                        let item = NotificationItem {
                            key,
                            label,
                            title,
                            content,
                            icon,
                            timestamp,
                        };
                        let notifs_lock = self.notifications.clone();
                        tokio::spawn(async move {
                            let mut notifs = notifs_lock.write().await;
                            if let Some(pos) = notifs.iter().position(|n| n.key == item.key) {
                                notifs[pos] = item;
                            } else {
                                notifs.insert(0, item);
                                if notifs.len() > 100 {
                                    notifs.truncate(100);
                                }
                            }
                        });
                    }
                }
            }
            "notification_deleted" => {
                if let Some(data) = val.get("data") {
                    if let Some(key) = data.get("key").and_then(|v| v.as_str()) {
                        let key = key.to_string();
                        let notifs_lock = self.notifications.clone();
                        tokio::spawn(async move {
                            let mut notifs = notifs_lock.write().await;
                            notifs.retain(|n| n.key != key);
                        });
                    }
                }
            }
            "clipboard_callback" => {
                let content = val
                    .get("content")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let source = val
                    .get("source")
                    .and_then(|v| v.as_str())
                    .unwrap_or("local")
                    .to_string();
                let item_type = val
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("text")
                    .to_string();
                if !content.is_empty() {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    let item = ClipboardItem {
                        r#type: item_type,
                        source: source.clone(),
                        content: content.clone(),
                        timestamp: now,
                    };
                    if source == "remote" {
                        let remote_clip = self.last_remote_clipboard.clone();
                        let c = content.clone();
                        tokio::spawn(async move {
                            *remote_clip.write().await = Some(c);
                        });
                    }
                    let history_lock = self.clipboard_history.clone();
                    tokio::spawn(async move {
                        let mut history = history_lock.write().await;
                        if let Some(first) = history.first() {
                            if first.content == item.content {
                                return;
                            }
                        }
                        history.insert(0, item);
                        if history.len() > 100 {
                            history.truncate(100);
                        }
                    });
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn device_removed_after_last_session_closes() {
        let state = AppState::new();
        let mut events = state.event_sender.subscribe();
        state
            .upsert_device(DeviceInfo {
                device_id: "3B161P013CM00000".into(),
                device_name: "一加 15T".into(),
                device_model: "PLZ110".into(),
                device_type: "phone".into(),
                brand: String::new(),
                os_version: String::new(),
                battery: 90,
                is_charging: false,
                connect_type: "wifi".into(),
                ip_address: Some("192.168.2.181".into()),
                is_connected: true,
                last_seen: 1,
            })
            .await;

        // Two control ports can serve the same device; closing one must keep it.
        let first = state.attach_data_session("3B161P013CM00000").await;
        let second = state.attach_data_session("3B161P013CM00000").await;
        assert!(!state.detach_data_session("3B161P013CM00000", first).await);
        assert!(state.devices.read().await.contains_key("3B161P013CM00000"));

        // The last close is the phone-side disconnect.
        assert!(state.detach_data_session("3B161P013CM00000", second).await);
        assert!(state.devices.read().await.is_empty());
        assert!(state.active_device_id.read().await.is_none());

        let event = tokio::time::timeout(tokio::time::Duration::from_millis(200), events.recv())
            .await
            .expect("disconnect callback broadcast")
            .expect("event channel open");
        let value: serde_json::Value = serde_json::from_str(&event).unwrap();
        assert_eq!(value["event"], "disconnect_callback");
        assert_eq!(value["data"]["device_id"], "3B161P013CM00000");
    }

    #[tokio::test]
    async fn scp_close_cleanup_is_cancelled_by_data_channel() {
        let state = AppState::new();
        state
            .upsert_device(DeviceInfo {
                device_id: "C77C702EC021".into(),
                device_name: "一加 15T".into(),
                device_model: "2B1488".into(),
                device_type: "phone".into(),
                brand: String::new(),
                os_version: String::new(),
                battery: 90,
                is_charging: false,
                connect_type: "wifi".into(),
                ip_address: Some("192.168.2.181".into()),
                is_connected: true,
                last_seen: 1,
            })
            .await;

        // The phone closes the SCP handshake socket right after SyncSecure.
        state
            .schedule_device_removal("C77C702EC021", tokio::time::Duration::from_millis(50))
            .await;
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        // The data channel authenticates and keeps the device connected.
        // panta reports the id in lowercase while SCP uses uppercase.
        let token = state.attach_data_session("c77c702ec021").await;
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        assert!(state.devices.read().await.contains_key("C77C702EC021"));

        // Only the data channel ending disconnects the device.
        assert!(state.detach_data_session("c77c702ec021", token).await);
        assert!(state.devices.read().await.is_empty());
    }

    #[tokio::test]
    async fn handshake_without_data_channel_is_cleaned_up() {
        let state = AppState::new();
        state
            .upsert_device(DeviceInfo {
                device_id: "C77C702EC021".into(),
                device_name: "一加 15T".into(),
                device_model: "2B1488".into(),
                device_type: "phone".into(),
                brand: String::new(),
                os_version: String::new(),
                battery: 90,
                is_charging: false,
                connect_type: "wifi".into(),
                ip_address: Some("192.168.2.181".into()),
                is_connected: true,
                last_seen: 1,
            })
            .await;
        state
            .schedule_device_removal("C77C702EC021", tokio::time::Duration::from_millis(30))
            .await;

        tokio::time::sleep(tokio::time::Duration::from_millis(80)).await;
        assert!(state.devices.read().await.is_empty());
    }

    #[tokio::test]
    async fn notification_events_update_state() {
        let state = AppState::new();
        let notif_evt = serde_json::json!({
            "event": "notification_callback",
            "data": {
                "key": "test_notif_1",
                "label": "TestApp",
                "title": "Hello",
                "content": "World",
                "icon": null,
                "timestamp": 1234567890,
            }
        });
        state.broadcast_event(&notif_evt.to_string());
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let notifs = state.notifications.read().await;
        assert_eq!(notifs.len(), 1);
        assert_eq!(notifs[0].key, "test_notif_1");
        assert_eq!(notifs[0].title, "Hello");
        assert_eq!(notifs[0].content, "World");
        drop(notifs);

        let del_evt = serde_json::json!({
            "event": "notification_deleted",
            "data": {
                "key": "test_notif_1"
            }
        });
        state.broadcast_event(&del_evt.to_string());
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let notifs = state.notifications.read().await;
        assert_eq!(notifs.len(), 0);
    }

    #[tokio::test]
    async fn clipboard_events_update_history_and_remote() {
        let state = AppState::new();
        let clip_evt = serde_json::json!({
            "event": "clipboard_callback",
            "type": "text",
            "source": "remote",
            "content": "Copied from phone",
        });
        state.broadcast_event(&clip_evt.to_string());
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let history = state.clipboard_history.read().await;
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].content, "Copied from phone");
        assert_eq!(history[0].source, "remote");
        drop(history);

        let remote = state.last_remote_clipboard.read().await;
        assert_eq!(remote.as_deref(), Some("Copied from phone"));
    }
}
