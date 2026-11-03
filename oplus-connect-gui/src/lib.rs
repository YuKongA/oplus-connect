#![allow(dead_code)]

pub mod assets;
pub mod cast_input;
pub mod cast_settings;
pub mod cast_media;
pub mod cast_session;
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
use state::{AppState, CastEvent, CastPhase, CastViewState};
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
        cast_session::shutdown().await;
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
    cast_session: Option<cast_session::CastSessionHandle>,
    cast_window: Option<WindowHandle<views::cast_window::CastWindow>>,
    cast_frame_size: (u32, u32),
    cast_input: Option<cast_session::InputSender>,
    // Dropping the subscription would stop observing closes.
    _cast_window_closed: Option<Subscription>,
    codec_select: Option<Entity<gpui_kit::component::select::SelectState<
        gpui_kit::component::searchable_list::SearchableVec<&'static str>,
    >>>,
    _codec_events: Option<Subscription>,
}

// Poll faster until /status proves the daemon is serving.
const STARTUP_POLL_INTERVAL: Duration = Duration::from_millis(400);
const STEADY_POLL_INTERVAL: Duration = Duration::from_secs(2);
// 25 × 400 ms ≈ 10 s without a single successful /status reply.
const STARTUP_MAX_ATTEMPTS: u8 = 25;

impl RootView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let (tx, mut rx) = mpsc::channel::<AppEvent>(64);

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
                let _ = tx.send(AppEvent::DaemonStartup(result)).await;
            });
        }

        let mut state = AppState::new();
        state.known_devices = persisted::load_known_devices();
        let (cast_audio, cast_input, cast_h265) = persisted::load_cast_settings();
        state.cast_audio = cast_audio;
        state.cast_input = cast_input;
        state.cast_h265 = cast_h265;
        cast_settings::set_audio(cast_audio);
        cast_settings::set_input(cast_input);
        cast_settings::set_h265(cast_h265);
        state.daemon_starting = true;
        let mut view = Self {
            state,
            cast_session: None,
            cast_window: None,
            cast_frame_size: (0, 0),
            cast_input: None,
            _cast_window_closed: None,
            codec_select: None,
            _codec_events: None,
        };
        // The cast window carries the app's own close button, so closing it
        // must end the cast instead of leaving a session with no picture.
        let weak = cx.entity().downgrade();
        view._cast_window_closed = Some(cx.on_window_closed(move |app, window_id| {
            // Closing the cast window from inside a RootView update re-enters
            // the same entity borrow, so react on the next effect instead.
            let weak = weak.clone();
            app.defer(move |app| {
                let _ = weak.update(app, |view, cx| {
                    if view.cast_window.map(|handle| handle.window_id()) == Some(window_id) {
                        view.cast_window = None;
                        view.stop_cast_session();
                        cx.notify();
                    }
                });
            });
        }));

                cx.spawn(async move |this, app| {
            while let Some(event) = rx.recv().await {
                let _ = this.update_in(app, |view, window, cx| {
                    let was_pending = view.state.connection_request.is_some();
                    if view.handle_app_event(event, cx) {
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

    pub fn start_cast(&mut self, device_id: String, cx: &mut Context<Self>) {
        if self.state.cast.active() || !self.state.known_devices.iter().any(|device| device.device_id == device_id && device.is_connected) {
            return;
        }
        let generation = self.state.cast.generation.wrapping_add(1);
        self.state.cast = CastViewState { device_id: device_id.clone(), generation, phase: CastPhase::Probing, detail: "检查视频解码与显示环境…".into() };
        let (tx, mut rx) = mpsc::channel(16);
        let (frame_tx, mut frame_rx) = mpsc::channel(1);
        let (session, input) = cast_session::start(device_id, tx, frame_tx);
        self.cast_input = Some(input);
        self.cast_session = Some(session);
        cx.spawn(async move |this, app| {
            while let Some(event) = rx.recv().await {
                if this.update(app, |view, cx| {
                    if view.handle_app_event(AppEvent::CastChanged { generation, event }, cx) { cx.notify(); }
                }).is_err() { break; }
            }
        }).detach();
        cx.spawn(async move |this, app| {
            while let Some(frame) = frame_rx.recv().await {
                if this.update(app, |view, cx| view.present_cast_frame(generation, frame, cx)).is_err() { break; }
            }
        }).detach();
        cx.notify();
    }

    /// Paint the newest decoded frame in the cast window, opening it on the
    /// first frame so the window appears only when there is something to show.
    fn present_cast_frame(&mut self, generation: u64, frame: crate::cast_media::VideoFrame, cx: &mut Context<Self>) {
        if !self.state.cast.accepts_frames(generation) || self.cast_session.is_none() {
            return;
        }
        let (width, height) = (frame.width, frame.height);
        // The frame moves into the view; keep it here until a window accepts it.
        let mut pending = Some(frame);
        let resized = self.cast_frame_size != (width, height);
        if let Some(handle) = self.cast_window {
            let updated = handle.update(cx, |view, window, cx| {
                if resized {
                    window.resize(self.cast_window_size(width, height));
                }
                if let Some(frame) = pending.take() {
                    view.set_frame(frame, cx);
                }
            });
            if updated.is_ok() {
                self.cast_window = Some(handle);
                self.cast_frame_size = (width, height);
                return;
            }
            // A user-closed window must not be recreated before its deferred
            // close notification arrives.
            self.cast_window = None;
            self.stop_cast_session();
            return;
        }
        let Some(frame) = pending.take() else { return };
        let options = self.cast_window_options(width, height, cx);
        let input = self.cast_input.clone();
        let cast = match cx.open_window(options, |_window, cx| {
            cx.new(|cx| views::cast_window::CastWindow::new(input.clone(), cx))
        }) {
            Ok(handle) => handle,
            Err(error) => {
                tracing::warn!("cannot open cast window: {error:#}");
                return;
            }
        };
        if cast
            .update(cx, |view, _window, cx| view.set_frame(frame, cx))
            .is_err()
        {
            return;
        }
        self.cast_window = Some(cast);
        self.cast_frame_size = (width, height);
    }

    /// Outer window size for a phone-sized video area.
    fn cast_window_size(&self, width: u32, height: u32) -> Size<Pixels> {
        let max_height = 900.0_f32;
        let scale = (max_height / height.max(1) as f32).min(1.0);
        size(
            px(width as f32 * scale),
            px(height as f32 * scale) + gpui_kit::component::TITLE_BAR_HEIGHT,
        )
    }

    fn cast_window_options(
        &self,
        width: u32,
        height: u32,
        cx: &mut Context<Self>,
    ) -> WindowOptions {
        // The titlebar is drawn inside the window, so the window must be that
        // much taller than the video area; otherwise `Contain` letterboxes it.
        let bounds = Bounds::centered(None, self.cast_window_size(width, height), cx);
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_decorations: Some(WindowDecorations::Client),
            app_id: Some("oplus-connect-cast".into()),
            titlebar: Some(TitlebarOptions {
                title: Some("OPLUS 互联 · 投屏".into()),
                ..gpui_kit::component::TitleBar::title_bar_options()
            }),
            focus: true,
            show: true,
            ..gpui_kit::component::TitleBar::window_options()
        }
    }

    fn close_cast_window(&mut self, cx: &mut Context<Self>) {
        if let Some(handle) = self.cast_window.take() {
            let _ = handle.update(cx, |_, window, _| window.remove_window());
        }
    }

    /// Persist and apply a cast switch from the settings page.
    pub fn set_cast_audio(&mut self, enabled: bool) {
        self.state.cast_audio = enabled;
        cast_settings::set_audio(enabled);
        persisted::save_cast_settings(enabled, self.state.cast_input, self.state.cast_h265);
    }

    pub fn set_cast_input(&mut self, enabled: bool, cx: &mut Context<Self>) {
        if let Some(handle) = self.cast_window {
            let _ = handle.update(cx, |view, _, _| view.set_input_enabled(enabled));
        }
        self.state.cast_input = enabled;
        cast_settings::set_input(enabled);
        persisted::save_cast_settings(self.state.cast_audio, enabled, self.state.cast_h265);
    }

    pub fn set_cast_h265(&mut self, enabled: bool) {
        self.state.cast_h265 = enabled;
        cast_settings::set_h265(enabled);
        persisted::save_cast_settings(self.state.cast_audio, self.state.cast_input, enabled);
    }

    pub fn stop_cast(&mut self, cx: &mut Context<Self>) {
        self.stop_cast_session();
        self.close_cast_window(cx);
    }

    fn stop_cast_session(&mut self) {
        self.cast_input = None;
        if let Some(session) = &self.cast_session {
            session.stop();
            self.state.cast.phase = CastPhase::Stopping;
            self.state.cast.detail = "正在关闭投屏连接…".into();
        }
    }

    fn start_daemon_poller(tx: mpsc::Sender<AppEvent>) {
        let client = DaemonClient::new();

        TOKIO_RT.spawn(async move {
            loop {
                let status = client.get_status().await.map_err(|e| e.to_string());
                let reachable = status.is_ok();
                let devices = client.get_devices().await.map_err(|e| e.to_string());
                let remote = client.get_remote_device().await.map_err(|e| e.to_string());

                if tx.send(AppEvent::DaemonPolled {
                    status,
                    devices,
                    remote,
                }).await.is_err() { break; }

                let interval = if reachable {
                    STEADY_POLL_INTERVAL
                } else {
                    STARTUP_POLL_INTERVAL
                };
                tokio::time::sleep(interval).await;
            }
        });
    }

    fn start_event_stream_listener(tx: mpsc::Sender<AppEvent>) {
        TOKIO_RT.spawn(async move {
            let socket_path = DaemonClient::event_socket_path();
            loop {
                if tx.is_closed() { return; }
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
                                if tx.send(AppEvent::IpcMessage(val)).await.is_err() { return; }
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

    pub fn handle_app_event(&mut self, event: AppEvent, cx: &mut Context<Self>) -> bool {
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
                    if self.state.cast.active() && !dev_list.iter().any(|device| device.device_id == self.state.cast.device_id) {
                        self.stop_cast(cx);
                        changed = true;
                    }
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
                        if disconnected_id.as_deref().map_or(true, |id| id == self.state.cast.device_id) { self.stop_cast(cx); }
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
            AppEvent::CastChanged { generation, event } => {
                if generation != self.state.cast.generation || !self.state.cast.active() { return false; }
                if self.state.cast.phase == CastPhase::Stopping && !matches!(event, CastEvent::Closed | CastEvent::Error(_)) { return false; }
                match event {
                    CastEvent::Phase(phase) => {
                        self.state.cast.phase = phase;
                        self.state.cast.detail = match phase {
                            CastPhase::Probing => "检查视频解码与显示环境…",
                            CastPhase::Starting => "正在启动投屏信令服务…",
                            CastPhase::Waiting => "等待手机连接并认证；尚无画面",
                            CastPhase::Negotiating => "正在协商 WebRTC；等待实际视频帧",
                            _ => "投屏状态变化",
                        }.into();
                    }
                    CastEvent::Displaying { width, height } => {
                        self.state.cast.phase = CastPhase::Displaying;
                        self.state.cast.detail = format!("正在显示 · {width} × {height}");
                    }
                    CastEvent::Error(error) => {
                        self.cast_session = None;
                        self.cast_input = None;
                        self.close_cast_window(cx);
                        self.state.cast.phase = CastPhase::Failed;
                        self.state.cast.detail = error;
                    }
                    CastEvent::Closed => {
                        self.cast_session = None;
                        self.cast_input = None;
                        self.close_cast_window(cx);
                        self.state.cast.phase = CastPhase::Idle;
                        self.state.cast.detail = "投屏已停止".into();
                    }
                }
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
        if self.codec_select.is_none() {
            let items =
                gpui_kit::component::searchable_list::SearchableVec::new(vec!["H.264", "H.265"]);
            let selected = gpui_kit::component::IndexPath::new(usize::from(self.state.cast_h265));
            let state = cx.new(|cx| {
                gpui_kit::component::select::SelectState::new(
                    items,
                    Some(selected),
                    window,
                    cx,
                )
            });
            self._codec_events = Some(cx.subscribe(
                &state,
                |view, _state, event: &gpui_kit::component::select::SelectEvent<
                    gpui_kit::component::searchable_list::SearchableVec<&'static str>,
                >, cx| {
                    let gpui_kit::component::select::SelectEvent::Confirm(value) = event;
                    match value.as_deref() {
                        Some("H.265") => view.set_cast_h265(true),
                        Some("H.264") => view.set_cast_h265(false),
                        _ => return,
                    }
                    cx.notify();
                },
            ));
            self.codec_select = Some(state);
        }
        views::MainContainer::render(&self.state, self.codec_select.as_ref(), window, cx)
    }
}
