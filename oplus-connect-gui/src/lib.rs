#![allow(dead_code)]

pub mod assets;
pub mod cast_input;
pub mod cast_media;
pub mod cast_session;
pub mod cast_settings;
pub mod client;
pub mod expand_cast_media;
pub mod expand_cast_session;
pub mod icons;
pub mod kms_capture;
pub mod kms_input;
pub mod kms_session;
pub mod kms_settings;
pub mod models;
mod output_geometry;
mod persisted;
pub mod state;
pub mod theme;
pub mod tray;
pub mod views;

/// Version shown in the title bar and settings.
pub const APP_VERSION: &str = "v1.0.0";

use client::DaemonClient;
use gpui_kit::component::WindowExt;
use gpui_kit::*;
use models::*;
use state::{AppState, CastEvent, CastPhase, CastViewState};
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::LazyLock;
use std::time::{Duration, Instant};
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

static IN_PROCESS_DAEMON: std::sync::LazyLock<
    tokio::sync::Mutex<Option<oplus_connect_daemon::DaemonHandle>>,
> = std::sync::LazyLock::new(|| tokio::sync::Mutex::new(None));

pub fn shutdown_in_process_daemon() {
    TOKIO_RT.block_on(async {
        tokio::join!(
            cast_session::shutdown(),
            kms_session::shutdown(),
            expand_cast_session::shutdown()
        );
        let mut handle = IN_PROCESS_DAEMON.lock().await;
        if let Some(h) = handle.take() {
            info!("Dropping in-process daemon handle and waiting for server cleanup...");
            drop(h);
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    });
}

/// Opens or reattaches the main window without dropping active sharing sessions.
pub fn open_main_window(
    cx: &mut App,
    main_window_id: Rc<Cell<Option<WindowId>>>,
    root_holder: Rc<RefCell<Option<Entity<RootView>>>>,
) -> anyhow::Result<AnyWindowHandle> {
    let bounds = Bounds::centered(None, size(px(1020.0), px(680.0)), cx);
    let window_options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        window_decorations: Some(WindowDecorations::Client),
        app_id: Some("oplus-connect".into()),
        titlebar: Some(TitlebarOptions {
            title: Some("OPLUS 互联".into()),
            ..gpui_kit::component::TitleBar::title_bar_options()
        }),
        focus: true,
        show: true,
        ..gpui_kit::component::TitleBar::window_options()
    };
    let root_for_window = root_holder.clone();
    let (handle, _) = gpui_kit::open_window(window_options, cx, move |_, cx| {
        if let Some(root) = root_for_window.borrow().clone() {
            return root;
        }
        let root = cx.new(RootView::new);
        *root_for_window.borrow_mut() = Some(root.clone());
        root
    })?;
    main_window_id.set(Some(handle.window_id()));
    Ok(handle)
}

pub struct RootView {
    pub state: AppState,
    pub kms: kms_session::KmsViewState,
    kms_session: Option<kms_session::KmsSessionHandle>,
    pub expand_cast: expand_cast_session::ExpandCastViewState,
    expand_cast_session: Option<expand_cast_session::ExpandCastSessionHandle>,
    share_started_at: Option<Instant>,
    poll_guard: PollGuard,
    cast_session: Option<cast_session::CastSessionHandle>,
    cast_window: Option<WindowHandle<views::cast_window::CastWindow>>,
    cast_frame_size: (u32, u32),
    cast_input: Option<cast_session::InputSender>,
    // Dropping the subscription would stop observing closes.
    _cast_window_closed: Option<Subscription>,
    codec_select: Option<
        Entity<
            gpui_kit::component::select::SelectState<
                gpui_kit::component::searchable_list::SearchableVec<&'static str>,
            >,
        >,
    >,
    _codec_events: Option<Subscription>,
    kms_direction_select: Option<
        Entity<
            gpui_kit::component::select::SelectState<
                gpui_kit::component::searchable_list::SearchableVec<&'static str>,
            >,
        >,
    >,
    _kms_direction_events: Option<Subscription>,
    expand_cast_fps_input: Option<Entity<gpui_kit::component::input::InputState>>,
    expand_cast_bitrate_input: Option<Entity<gpui_kit::component::input::InputState>>,
    _expand_cast_input_events: Vec<Subscription>,
}

// Poll faster until /status proves the daemon is serving.
const STARTUP_POLL_INTERVAL: Duration = Duration::from_millis(400);
const STEADY_POLL_INTERVAL: Duration = Duration::from_secs(2);
// 25 × 400 ms ≈ 10 s without a single successful /status reply.
const STARTUP_MAX_ATTEMPTS: u8 = 25;
// A transient IPC failure is not evidence that an established daemon stopped.
const DAEMON_MAX_CONSECUTIVE_FAILURES: u8 = 3;

#[derive(Default)]
struct PollGuard {
    consecutive_status_failures: u8,
}

impl PollGuard {
    // None means that this observation is inconclusive.
    fn observe_status(&mut self, running: Option<bool>) -> Option<bool> {
        match running {
            Some(running) => {
                self.consecutive_status_failures = 0;
                Some(running)
            }
            None => {
                self.consecutive_status_failures =
                    self.consecutive_status_failures.saturating_add(1);
                (self.consecutive_status_failures >= DAEMON_MAX_CONSECUTIVE_FAILURES)
                    .then_some(false)
            }
        }
    }
}

// A request begun before a share started can contain an older device list, even
// if its response arrives after a device-initiated share request was handled.
fn current_device_snapshot(
    devices: Result<Vec<DeviceInfo>, String>,
    requested_at: Instant,
    share_started_at: Option<Instant>,
) -> Option<Vec<DeviceInfo>> {
    devices
        .ok()
        .filter(|_| share_started_at.is_none_or(|started_at| requested_at > started_at))
}

fn device_missing(devices: &[DeviceInfo], device_id: &str) -> bool {
    !devices
        .iter()
        .any(|device| device.device_id == device_id && device.is_connected)
}

impl RootView {
    fn save_expand_cast_settings(
        &mut self,
        settings: persisted::ExpandCastSettings,
        cx: &mut Context<Self>,
    ) {
        if settings == self.state.expand_cast_settings {
            self.state.expand_cast_settings_feedback = None;
        } else {
            match persisted::save_expand_cast_settings(settings) {
                Ok(()) => {
                    self.state.expand_cast_settings = settings;
                    self.state.expand_cast_settings_feedback = None;
                }
                Err(error) => {
                    self.state.expand_cast_settings_feedback =
                        Some(format!("保存失败，设置未应用：{error}"));
                }
            }
        }
        cx.notify();
    }

    fn update_expand_cast_number(&mut self, value: &str, fps: bool, cx: &mut Context<Self>) {
        let (min, max, label) = if fps {
            (persisted::FPS_MIN, persisted::FPS_MAX, "帧率")
        } else {
            (persisted::BITRATE_MIN, persisted::BITRATE_MAX, "码率")
        };
        let Some(value) = value
            .parse::<u32>()
            .ok()
            .filter(|value| (min..=max).contains(value))
        else {
            self.state.expand_cast_settings_feedback =
                Some(format!("{label}必须为 {min}–{max} 的整数，未保存"));
            cx.notify();
            return;
        };
        let mut settings = self.state.expand_cast_settings;
        if fps {
            settings.fps = value;
        } else {
            settings.bitrate_kbps = value;
        }
        self.save_expand_cast_settings(settings, cx);
    }

    fn update_expand_cast_adaptive(&mut self, enabled: bool, cx: &mut Context<Self>) {
        let mut settings = self.state.expand_cast_settings;
        settings.adaptive_bitrate = enabled;
        self.save_expand_cast_settings(settings, cx);
    }

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
        state.expand_cast_settings = persisted::load_expand_cast_settings();
        cast_settings::set_audio(cast_audio);
        cast_settings::set_input(cast_input);
        cast_settings::set_h265(cast_h265);
        kms_settings::set_direction(persisted::load_kms_direction());
        state.daemon_starting = true;
        let mut view = Self {
            state,
            kms: kms_session::KmsViewState::default(),
            kms_session: None,
            expand_cast: expand_cast_session::ExpandCastViewState::default(),
            expand_cast_session: None,
            share_started_at: None,
            poll_guard: PollGuard::default(),
            cast_session: None,
            cast_window: None,
            cast_frame_size: (0, 0),
            cast_input: None,
            _cast_window_closed: None,
            codec_select: None,
            _codec_events: None,
            kms_direction_select: None,
            _kms_direction_events: None,
            expand_cast_fps_input: None,
            expand_cast_bitrate_input: None,
            _expand_cast_input_events: Vec::new(),
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
                        if !was_pending && let Some(request) = view.state.connection_request.clone()
                        {
                            views::confirm_modal::ConfirmModal::open(request, window, cx);
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

    pub fn start_kms(&mut self, device_id: String, cx: &mut Context<Self>) {
        if self.kms.active()
            || self.expand_cast.active()
            || self.state.cast.active()
            || !self.state.known_devices.iter().any(|device| {
                device.device_id == device_id && device.is_connected && device.kms_capable()
            })
        {
            return;
        }
        self.share_started_at = Some(Instant::now());
        let generation = self.kms.generation.wrapping_add(1);
        self.kms = kms_session::KmsViewState {
            device_id: device_id.clone(),
            generation,
            phase: kms_session::KmsPhase::Preparing,
            detail: "等待系统授权全局键鼠捕获…".into(),
        };
        let (tx, mut rx) = mpsc::channel(16);
        self.kms_session = Some(kms_session::start(device_id, tx));
        cx.spawn(async move |this, app| {
            while let Some(event) = rx.recv().await {
                if this
                    .update(app, |view, cx| {
                        use kms_session::{KmsEvent, KmsPhase};
                        if view.kms.generation != generation || !view.kms.active() {
                            return;
                        }
                        if view.kms.phase == KmsPhase::Stopping
                            && !matches!(event, KmsEvent::Closed | KmsEvent::Error(_))
                        {
                            return;
                        }
                        match event {
                            KmsEvent::Phase(phase) => {
                                view.kms.phase = phase;
                                view.kms.detail = match phase {
                                    KmsPhase::Waiting => "等待键鼠通道连接…",
                                    KmsPhase::Armed => {
                                        "已就绪：从设置的电脑边缘移入设备；从设备相对边缘移回"
                                    }
                                    KmsPhase::Sharing => {
                                        "正在控制设备 · 键鼠均作用于设备，从相对边缘移回电脑"
                                    }
                                    _ => "正在准备键鼠共享…",
                                }
                                .into();
                            }
                            KmsEvent::Closed => {
                                view.kms_session = None;
                                view.kms.phase = KmsPhase::Idle;
                                view.kms.detail = "键鼠共享已停止".into();
                            }
                            KmsEvent::Error(error) => {
                                view.kms_session = None;
                                view.kms.phase = KmsPhase::Failed;
                                view.kms.detail = error.clone();
                                view.state.set_toast(error);
                            }
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }

    pub fn stop_kms(&mut self, cx: &mut Context<Self>) {
        if let Some(session) = &self.kms_session {
            session.stop();
            self.kms.phase = kms_session::KmsPhase::Stopping;
            self.kms.detail = "正在释放按键并关闭键鼠共享…".into();
            cx.notify();
        }
    }
    pub fn start_expand_cast(&mut self, device_id: String, mode: u8, cx: &mut Context<Self>) {
        self.start_expand_cast_with_mode(device_id, Some(mode), cx);
    }

    fn start_expand_cast_with_mode(
        &mut self,
        device_id: String,
        mode: Option<u8>,
        cx: &mut Context<Self>,
    ) {
        if self.expand_cast.active()
            || self.kms.active()
            || self.state.cast.active()
            || !self.state.known_devices.iter().any(|device| {
                device.device_id == device_id
                    && device.is_connected
                    && device.device_type == "tablet"
            })
        {
            return;
        }
        self.share_started_at = Some(Instant::now());
        let generation = self.expand_cast.generation.wrapping_add(1);
        self.expand_cast = expand_cast_session::ExpandCastViewState {
            device_id: device_id.clone(),
            mode,
            generation,
            phase: expand_cast_session::ExpandCastPhase::Preparing,
            detail: match mode {
                Some(1) => "正在准备屏幕扩展，请在系统弹窗中授权…",
                Some(2) => "正在准备屏幕镜像，请在系统弹窗中授权…",
                _ => "正在准备屏幕共享…",
            }
            .into(),
            width: 2016,
            height: 1344,
        };
        let (tx, mut rx) = mpsc::channel(16);
        self.expand_cast_session = Some(expand_cast_session::start(
            device_id,
            tx,
            self.state.expand_cast_settings,
            mode,
        ));
        cx.spawn(async move |this, app| {
            while let Some(event) = rx.recv().await {
                if this
                    .update(app, |view, cx| {
                        use expand_cast_session::{ExpandCastEvent, ExpandCastPhase};
                        if view.expand_cast.generation != generation || !view.expand_cast.active() {
                            return;
                        }
                        if view.expand_cast.phase == ExpandCastPhase::Stopping
                            && !matches!(event, ExpandCastEvent::Closed | ExpandCastEvent::Error(_))
                        {
                            return;
                        }
                        match event {
                            ExpandCastEvent::Phase(phase) => {
                                view.expand_cast.phase = phase;
                                view.expand_cast.detail = match (phase, view.expand_cast.mode) {
                                    (ExpandCastPhase::WaitingForTablet, Some(1)) => {
                                        "屏幕扩展：等待平板连接，并在系统弹窗中授权…"
                                    }
                                    (ExpandCastPhase::WaitingForTablet, Some(2)) => {
                                        "屏幕镜像：等待平板连接，并在系统弹窗中授权…"
                                    }
                                    (ExpandCastPhase::WaitingForTablet, _) => {
                                        "等待平板连接，请在系统弹窗选择要共享的显示器…"
                                    }
                                    (ExpandCastPhase::Streaming, Some(1)) => {
                                        "扩展屏源已授权，正在启动推流（实际显示效果取决于系统）"
                                    }
                                    (ExpandCastPhase::Streaming, Some(2)) => {
                                        "显示器源已授权，正在启动镜像推流"
                                    }
                                    (ExpandCastPhase::Streaming, _) => "屏幕共享推流已启动",
                                    (ExpandCastPhase::Stopping, _) => "正在停止共享屏幕…",
                                    _ => "正在准备屏幕共享…",
                                }
                                .into();
                            }
                            ExpandCastEvent::Resolution(w, h) => {
                                view.expand_cast.width = w;
                                view.expand_cast.height = h;
                            }
                            ExpandCastEvent::Closed => {
                                view.expand_cast_session = None;
                                view.expand_cast.phase = ExpandCastPhase::Idle;
                                view.expand_cast.detail = "屏幕共享已停止".into();
                            }
                            ExpandCastEvent::Error(error) => {
                                view.expand_cast_session = None;
                                view.expand_cast.phase = ExpandCastPhase::Failed;
                                view.expand_cast.detail = error.clone();
                                view.state.set_toast(error);
                            }
                        }
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }

    pub fn stop_expand_cast(&mut self, cx: &mut Context<Self>) {
        if let Some(session) = &self.expand_cast_session {
            session.stop();
            self.expand_cast.phase = expand_cast_session::ExpandCastPhase::Stopping;
            self.expand_cast.detail = "正在停止共享屏幕…".into();
            cx.notify();
        }
    }

    pub fn start_cast(&mut self, device_id: String, cx: &mut Context<Self>) {
        if self.state.cast.active()
            || self.kms.active()
            || self.expand_cast.active()
            || !self.state.known_devices.iter().any(|device| {
                device.device_id == device_id
                    && device.is_connected
                    && device.device_type == "phone"
            })
        {
            return;
        }
        let generation = self.state.cast.generation.wrapping_add(1);
        self.state.cast = CastViewState {
            device_id: device_id.clone(),
            generation,
            phase: CastPhase::Probing,
            detail: "检查视频解码与显示环境…".into(),
        };
        let (tx, mut rx) = mpsc::channel(16);
        let (frame_tx, mut frame_rx) = mpsc::channel(1);
        let (session, input) = cast_session::start(device_id, tx, frame_tx);
        self.cast_input = Some(input);
        self.cast_session = Some(session);
        cx.spawn(async move |this, app| {
            while let Some(event) = rx.recv().await {
                if this
                    .update(app, |view, cx| {
                        if view.handle_app_event(AppEvent::CastChanged { generation, event }, cx) {
                            cx.notify();
                        }
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        cx.spawn(async move |this, app| {
            while let Some(frame) = frame_rx.recv().await {
                if this
                    .update(app, |view, cx| {
                        view.present_cast_frame(generation, frame, cx)
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }

    /// Paint the newest decoded frame in the cast window, opening it on the
    /// first frame so the window appears only when there is something to show.
    fn present_cast_frame(
        &mut self,
        generation: u64,
        frame: crate::cast_media::VideoFrame,
        cx: &mut Context<Self>,
    ) {
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

    /// Outer window size for a phone-sized picture, including the titlebar and
    /// the navigation bar.
    fn cast_window_size(&self, width: u32, height: u32) -> Size<Pixels> {
        let max_height = 900.0_f32;
        let scale = (max_height / height.max(1) as f32).min(1.0);
        size(
            px(width as f32 * scale),
            px(height as f32 * scale)
                + gpui_kit::component::TITLE_BAR_HEIGHT
                + px(crate::cast_input::NAV_BAR_HEIGHT),
        )
    }

    fn cast_window_options(
        &self,
        width: u32,
        height: u32,
        cx: &mut Context<Self>,
    ) -> WindowOptions {
        // The titlebar and navigation bar are drawn inside the window, so the
        // window is that much taller than the picture; otherwise `Contain`
        // letterboxes it.
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
                let devices_requested_at = Instant::now();
                let status = client.get_status().await.map_err(|e| e.to_string());
                let reachable = status.is_ok();
                let devices = client.get_devices().await.map_err(|e| e.to_string());
                let remote = client.get_remote_device().await.map_err(|e| e.to_string());

                if tx
                    .send(AppEvent::DaemonPolled {
                        status,
                        devices,
                        devices_requested_at,
                        remote,
                    })
                    .await
                    .is_err()
                {
                    break;
                }

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
            let mut warned = false;
            loop {
                if tx.is_closed() {
                    return;
                }
                let Some(socket_path) = DaemonClient::event_socket_path() else {
                    if !warned {
                        warned = true;
                        warn!("No private daemon IPC directory; retrying in 3s...");
                    }
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    continue;
                };
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
                                let event = if val.get("event").and_then(|event| event.as_str())
                                    == Some("tablet_share_request")
                                {
                                    let Some(id) = val
                                        .get("device_id")
                                        .and_then(|id| id.as_str())
                                        .filter(|id| !id.is_empty())
                                    else {
                                        continue;
                                    };
                                    let Some(mode @ 0..=2) =
                                        val.get("mode").and_then(|mode| mode.as_u64())
                                    else {
                                        continue;
                                    };
                                    let Ok(devices) = DaemonClient::new().get_devices().await
                                    else {
                                        continue;
                                    };
                                    let Some(device) = devices.into_iter().find(|device| {
                                        device.device_id.eq_ignore_ascii_case(id)
                                            && device.is_connected
                                            && device.device_type == "tablet"
                                    }) else {
                                        continue;
                                    };
                                    AppEvent::TabletShareRequest {
                                        device,
                                        mode: mode as u8,
                                    }
                                } else if val.get("event").and_then(|event| event.as_str())
                                    == Some("phone_cast_request")
                                {
                                    let Some(id) = val
                                        .get("device_id")
                                        .and_then(|id| id.as_str())
                                        .filter(|id| !id.is_empty())
                                    else {
                                        continue;
                                    };
                                    let Ok(devices) = DaemonClient::new().get_devices().await
                                    else {
                                        continue;
                                    };
                                    let Some(device) = devices.into_iter().find(|device| {
                                        device.device_id.eq_ignore_ascii_case(id)
                                            && device.is_connected
                                            && device.device_type == "phone"
                                    }) else {
                                        continue;
                                    };
                                    AppEvent::PhoneCastRequest { device }
                                } else {
                                    AppEvent::IpcMessage(val)
                                };
                                if tx.send(event).await.is_err() {
                                    return;
                                }
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

    pub fn forget_device(&mut self, device_id: String, cx: &mut Context<Self>) {
        let before = self.state.known_devices.len();
        self.state
            .known_devices
            .retain(|device| !device.device_id.eq_ignore_ascii_case(&device_id));
        if self.state.known_devices.len() == before {
            return;
        }
        if self
            .state
            .connected_device
            .as_ref()
            .is_some_and(|device| device.device_id.eq_ignore_ascii_case(&device_id))
        {
            self.state.connected_device = None;
            self.state.remote_details = None;
        }
        persisted::save_known_devices(&self.state.known_devices);
        self.state.set_toast("已删除该设备");
        cx.notify();
    }

    pub fn handle_app_event(&mut self, event: AppEvent, cx: &mut Context<Self>) -> bool {
        match event {
            AppEvent::DaemonPolled {
                status,
                devices,
                devices_requested_at,
                remote,
            } => {
                let mut changed = false;
                let share_started_at = if self.kms.active() || self.expand_cast.active() {
                    self.share_started_at
                } else {
                    None
                };
                let current_poll =
                    share_started_at.is_none_or(|started_at| devices_requested_at > started_at);
                let running = if current_poll {
                    let observation =
                        status.as_ref().ok().map(|st| st.running).or_else(|| {
                            (self.state.daemon_running && devices.is_ok()).then_some(true)
                        });
                    self.poll_guard.observe_status(observation)
                } else {
                    None
                };
                if status.is_ok() {
                    // /status is the only proof that startup finished.
                    self.state.daemon_startup_failures = 0;
                    if self.state.daemon_starting {
                        self.state.daemon_starting = false;
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
                }
                if let Some(running) = running
                    && self.state.daemon_running != running
                {
                    self.state.daemon_running = running;
                    changed = true;
                }
                if running == Some(false) && self.kms.active() {
                    self.stop_kms(cx);
                    changed = true;
                }
                if running == Some(false) && self.expand_cast.active() {
                    self.stop_expand_cast(cx);
                    changed = true;
                }

                if let Some(dev_list) =
                    current_device_snapshot(devices, devices_requested_at, share_started_at)
                {
                    if self.expand_cast.active()
                        && device_missing(&dev_list, &self.expand_cast.device_id)
                    {
                        self.stop_expand_cast(cx);
                        changed = true;
                    }
                    if self.kms.active() && device_missing(&dev_list, &self.kms.device_id) {
                        self.stop_kms(cx);
                        changed = true;
                    }
                    if self.state.cast.active()
                        && !dev_list
                            .iter()
                            .any(|device| device.device_id == self.state.cast.device_id)
                    {
                        self.stop_cast(cx);
                        changed = true;
                    }
                    self.state.sync_known_devices(&dev_list);
                    let first = dev_list.into_iter().next();
                    if self.state.connected_device.as_ref().map(|d| &d.device_id)
                        != first.as_ref().map(|d| &d.device_id)
                    {
                        self.state.connected_device = first;
                        changed = true;
                    }
                }

                match remote {
                    Ok(Some(rm)) => {
                        let is_new = self.state.remote_details.as_ref().map(|r| &r.device_id)
                            != Some(&rm.device_id);
                        let dev = DeviceInfo {
                            device_id: rm.device_id.clone(),
                            device_name: rm.device_name.clone(),
                            device_model: rm.device_model.clone(),
                            device_type: rm.device_type.clone(),
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
            AppEvent::PhoneCastRequest { device } => {
                let device_id = device.device_id.clone();
                self.state.remember_connected_device(&device);
                if self.state.cast.active() && self.state.cast.device_id == device_id {
                    return false;
                }
                if self.state.cast.active() || self.kms.active() || self.expand_cast.active() {
                    self.state
                        .set_toast("手机请求投屏，但已有会话正在进行或停止中，请先结束该会话");
                    return true;
                }
                self.state.set_toast("手机请求投屏，正在启动…");
                self.start_cast(device_id, cx);
                true
            }
            AppEvent::TabletShareRequest { device, mode } => {
                let device_id = device.device_id.clone();
                self.state.remember_connected_device(&device);
                let duplicate = if mode == 0 {
                    self.kms.active() && self.kms.device_id == device_id
                } else {
                    self.expand_cast.active()
                        && self.expand_cast.device_id == device_id
                        && self.expand_cast.mode == Some(mode)
                };
                if duplicate {
                    return false;
                }
                if self.kms.active() || self.expand_cast.active() || self.state.cast.active() {
                    self.state
                        .set_toast("平板请求共享，但已有会话正在进行或停止中，请先结束该会话");
                    return true;
                }
                if mode == 0 {
                    self.state
                        .set_toast("平板请求键鼠共享，请在系统弹窗中授权捕获键鼠");
                    self.start_kms(device_id, cx);
                } else {
                    self.state.set_toast(if mode == 1 {
                        "平板请求屏幕扩展，请在系统弹窗中授权"
                    } else {
                        "平板请求屏幕镜像，请在系统弹窗中授权"
                    });
                    self.start_expand_cast_with_mode(device_id, Some(mode), cx);
                }
                true
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
                        device_address: data["device_address"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                        device_lan_address: data["device_lan_address"]
                            .as_str()
                            .map(str::to_string)
                            .filter(|address| !address.is_empty()),
                        connection_id: data["connection_id"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string(),
                    };
                    info!("Incoming connection approval request: {:?}", req);
                    self.state.connection_request = Some(req);
                    return true;
                }

                let event_type = val
                    .get("event")
                    .and_then(|e| e.as_str())
                    .unwrap_or_default();
                match event_type {
                    "clipboard_callback" => {
                        if let Some(content) = val.get("content").and_then(|c| c.as_str()) {
                            let source_type = val
                                .get("source")
                                .and_then(|s| s.as_str())
                                .unwrap_or("remote");
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
                                icon_base64: data["icon"]
                                    .as_str()
                                    .filter(|encoded| {
                                        encoded.len() <= crate::icons::MAX_ENCODED_ICON_LEN
                                    })
                                    .map(|s| s.to_string()),
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
                        let reason = val["data"]["reason"].as_str().unwrap_or("连接失败");
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
                        if disconnected_id
                            .as_deref()
                            .is_none_or(|id| id == self.state.cast.device_id)
                        {
                            self.stop_cast(cx);
                        }
                        if disconnected_id
                            .as_deref()
                            .is_none_or(|id| id == self.kms.device_id)
                        {
                            self.stop_kms(cx);
                        }
                        if disconnected_id
                            .as_deref()
                            .is_none_or(|id| id == self.expand_cast.device_id)
                        {
                            self.stop_expand_cast(cx);
                        }
                        self.state
                            .mark_device_disconnected(disconnected_id.as_deref());
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
                if generation != self.state.cast.generation || !self.state.cast.active() {
                    return false;
                }
                if self.state.cast.phase == CastPhase::Stopping
                    && !matches!(event, CastEvent::Closed | CastEvent::Error(_))
                {
                    return false;
                }
                match event {
                    CastEvent::Phase(phase) => {
                        self.state.cast.phase = phase;
                        self.state.cast.detail = match phase {
                            CastPhase::Probing => "检查视频解码与显示环境…",
                            CastPhase::Starting => "正在启动投屏信令服务…",
                            CastPhase::Waiting => "等待手机连接并认证；尚无画面",
                            CastPhase::Negotiating => "正在协商 WebRTC；等待实际视频帧",
                            _ => "投屏状态变化",
                        }
                        .into();
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

impl Drop for RootView {
    fn drop(&mut self) {
        if let Some(session) = &self.kms_session {
            session.stop();
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
                gpui_kit::component::select::SelectState::new(items, Some(selected), window, cx)
            });
            self._codec_events = Some(cx.subscribe(
                &state,
                |view,
                 _state,
                 event: &gpui_kit::component::select::SelectEvent<
                    gpui_kit::component::searchable_list::SearchableVec<&'static str>,
                >,
                 cx| {
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
        if self.kms_direction_select.is_none() {
            let labels = vec!["向左", "向右"];
            let index = labels
                .iter()
                .position(|label| *label == kms_settings::direction().label())
                .unwrap_or(0);
            let state = cx.new(|cx| {
                gpui_kit::component::select::SelectState::new(
                    gpui_kit::component::searchable_list::SearchableVec::new(labels),
                    Some(gpui_kit::component::IndexPath::new(index)),
                    window,
                    cx,
                )
            });
            self._kms_direction_events = Some(cx.subscribe(
                &state,
                |view,
                 _,
                 event: &gpui_kit::component::select::SelectEvent<
                    gpui_kit::component::searchable_list::SearchableVec<&'static str>,
                >,
                 cx| {
                    let gpui_kit::component::select::SelectEvent::Confirm(value) = event;
                    let Some(direction) = value
                        .as_deref()
                        .and_then(kms_settings::ExitDirection::from_label)
                    else {
                        return;
                    };
                    match persisted::save_kms_direction(direction) {
                        Ok(()) => {
                            kms_settings::set_direction(direction);
                            view.state.set_toast("共享方向已保存，下次启动键鼠共享生效");
                        }
                        Err(error) => view.state.set_toast(format!("保存共享方向失败：{error}")),
                    }
                    cx.notify();
                },
            ));
            self.kms_direction_select = Some(state);
        }
        if self.expand_cast_fps_input.is_none() {
            let value = self.state.expand_cast_settings.fps.to_string();
            let input = cx.new(|cx| {
                gpui_kit::component::input::InputState::new(window, cx).default_value(value)
            });
            self._expand_cast_input_events
                .push(cx.subscribe(&input, |view, state, event, cx| {
                    if matches!(event, gpui_kit::component::input::InputEvent::Change) {
                        let value = state.read(cx).value().to_string();
                        view.update_expand_cast_number(&value, true, cx);
                    }
                }));
            self.expand_cast_fps_input = Some(input);
        }
        if self.expand_cast_bitrate_input.is_none() {
            let value = self.state.expand_cast_settings.bitrate_kbps.to_string();
            let input = cx.new(|cx| {
                gpui_kit::component::input::InputState::new(window, cx).default_value(value)
            });
            self._expand_cast_input_events
                .push(cx.subscribe(&input, |view, state, event, cx| {
                    if matches!(event, gpui_kit::component::input::InputEvent::Change) {
                        let value = state.read(cx).value().to_string();
                        view.update_expand_cast_number(&value, false, cx);
                    }
                }));
            self.expand_cast_bitrate_input = Some(input);
        }
        let controls = views::SettingsControls {
            codec: self.codec_select.as_ref(),
            kms_direction: self.kms_direction_select.as_ref(),
            expand_cast_fps: self.expand_cast_fps_input.as_ref(),
            expand_cast_bitrate: self.expand_cast_bitrate_input.as_ref(),
        };
        views::MainContainer::render(
            &self.state,
            &self.kms,
            &self.expand_cast,
            controls,
            window,
            cx,
        )
    }
}

#[cfg(test)]
mod polling_tests {
    use super::{
        DAEMON_MAX_CONSECUTIVE_FAILURES, PollGuard, current_device_snapshot, device_missing,
    };
    use crate::models::DeviceInfo;
    use crate::state::AppState;
    use std::time::{Duration, Instant};

    fn device(id: &str, device_type: &str, connected: bool) -> DeviceInfo {
        DeviceInfo {
            device_id: id.into(),
            device_type: device_type.into(),
            is_connected: connected,
            ..Default::default()
        }
    }

    fn tablet(id: &str, connected: bool) -> DeviceInfo {
        device(id, "tablet", connected)
    }

    #[test]
    fn keyboard_mouse_sharing_accepts_phones_and_tablets_only() {
        assert!(device("phone-1", "phone", true).kms_capable());
        assert!(tablet("tablet-1", true).kms_capable());
        assert!(!device("unknown-1", "unknown", true).kms_capable());
        assert!(!device("empty-1", "", true).kms_capable());
    }

    #[test]
    fn status_failure_is_transient_but_repeated_failure_stops_sharing() {
        let mut guard = PollGuard::default();
        assert_eq!(guard.observe_status(Some(true)), Some(true));
        assert_eq!(guard.observe_status(None), None);
        assert_eq!(guard.observe_status(Some(true)), Some(true));
        for _ in 1..DAEMON_MAX_CONSECUTIVE_FAILURES {
            assert_eq!(guard.observe_status(None), None);
        }
        assert_eq!(guard.observe_status(None), Some(false));
        assert_eq!(guard.observe_status(Some(true)), Some(true));
        assert_eq!(guard.observe_status(None), None);
        assert_eq!(guard.observe_status(Some(false)), Some(false));
        assert_eq!(guard.observe_status(None), None);
    }

    #[test]
    fn stale_snapshot_cannot_stop_new_share_or_mark_device_offline() {
        let start = Instant::now();
        let before = start - Duration::from_millis(1);
        let after = start + Duration::from_millis(1);
        let mut state = AppState::new();
        state.remember_connected_device(&tablet("tablet-1", true));

        // The request began before the share but its empty response arrived later.
        let stale = current_device_snapshot(Ok(vec![]), before, Some(start));
        assert!(stale.is_none());
        assert!(state.known_devices[0].is_connected);
        assert!(current_device_snapshot(Ok(vec![]), start, Some(start)).is_none());
        assert!(current_device_snapshot(Err("unavailable".into()), after, Some(start)).is_none());

        let connected =
            current_device_snapshot(Ok(vec![tablet("tablet-1", true)]), after, Some(start))
                .unwrap();
        assert!(!device_missing(&connected, "tablet-1"));
        state.sync_known_devices(&connected);
        assert!(state.known_devices[0].is_connected);

        // A fresh snapshot is authoritative, including an explicitly offline
        // entry or an empty list; genuine disconnects must stop the share.
        let offline =
            current_device_snapshot(Ok(vec![tablet("tablet-1", false)]), after, Some(start))
                .unwrap();
        assert!(device_missing(&offline, "tablet-1"));
        let missing = current_device_snapshot(Ok(vec![]), after, Some(start)).unwrap();
        assert!(device_missing(&missing, "tablet-1"));
        state.sync_known_devices(&missing);
        assert!(!state.known_devices[0].is_connected);

        // A later share must not accept a snapshot from the preceding one.
        let restarted = after + Duration::from_millis(1);
        assert!(current_device_snapshot(Ok(missing), after, Some(restarted)).is_none());
        assert!(current_device_snapshot(Ok(vec![]), before, None).is_some());
    }
}
