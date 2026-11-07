//! Global Wayland capture through the InputCapture v2 portal and its EIS transport.
//!
//! Only the configured left/right outer desktop boundary is armed. Dropping the
//! consumer or overflowing its bounded queue terminates capture; callers release
//! remote held input when this task ends as well as on `Deactivated`.

use std::{collections::HashSet, os::unix::net::UnixStream, time::Duration};

use anyhow::{Context, Result, anyhow, bail, ensure};
use ashpd::desktop::{
    Session,
    input_capture::{
        ActivatedBarrier, Barrier, BarrierID, Capabilities, InputCapture, ReleaseOptions,
        StartOptions, Zones,
    },
};
use futures_util::StreamExt;
use reis::{
    ei::{self, button::ButtonState, keyboard::KeyState},
    event::{Device, DeviceCapability, EiEvent},
};
use tokio::{
    sync::{mpsc, watch},
    time::{Instant, timeout},
};

use crate::kms_settings::ExitDirection;

const CLOSE_TIMEOUT: Duration = Duration::from_secs(3);
const ACTIVATION_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Debug)]
pub enum CaptureEvent {
    Ready {
        width: u32,
        height: u32,
    },
    /// Target Pad entry edge, with Y normalized over the host desktop rectangle.
    Activated {
        activation: u64,
        pct_x: f64,
        pct_y: f64,
    },
    Motion {
        dx: f64,
        dy: f64,
    },
    /// Linux evdev button code, not an Android button mask.
    Button {
        code: u32,
        pressed: bool,
    },
    /// Linux evdev key code. All keys, including Escape, belong to the Pad.
    Key {
        code: u32,
        pressed: bool,
    },
    /// Fractional wheel steps, positive right/down. EIS discrete units / 120;
    /// continuous logical pixels / 15. Discrete axes supersede continuous axes.
    Scroll {
        dx: f64,
        dy: f64,
    },
    Deactivated,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ReturnRequest {
    pub activation: u64,
    pub pct_x: f64,
    pub pct_y: f64,
}

/// Prepare global capture, publish geometry, then wait for the Pad connection gate.
/// Callers cancel and await this task rather than aborting its portal cleanup.
pub async fn run(
    events: mpsc::Sender<CaptureEvent>,
    mut cancel: watch::Receiver<bool>,
    enabled: watch::Receiver<bool>,
    direction: ExitDirection,
    return_rx: watch::Receiver<Option<ReturnRequest>>,
) -> Result<()> {
    let connection = tokio::select! {
        biased;
        _ = cancelled(&mut cancel) => return Ok(()),
        connection = zbus::Connection::session() => connection.context("Connecting to session bus")?,
    };
    // Keep a private owner: disconnect also destroys a session if cancellation
    // interrupts CreateSession2 before its handle has reached us.
    let result = async {
        tokio::select! {
            biased;
            _ = cancelled(&mut cancel) => return Ok(()),
            result = register_host_app(&connection) => result?,
        }
        let portal = tokio::select! {
            biased;
            _ = cancelled(&mut cancel) => return Ok(()),
            portal = InputCapture::with_connection(connection.clone()) => portal?,
        };
        ensure!(portal.version() >= 2, "Global keyboard/mouse sharing requires InputCapture portal version 2");
        let session = tokio::select! {
            biased;
            _ = cancelled(&mut cancel) => return Ok(()),
            session = portal.create_session2(Default::default()) => session.context("Creating input capture session")?,
        };
        let result = tokio::select! {
            biased;
            _ = cancelled(&mut cancel) => Ok(()),
            result = capture(&portal, &session, &events, enabled, direction, return_rx) => result,
        };
        // Force an immediate release of any active capture so KWin/Wayland immediately
        // restores the pointer to the local desktop without waiting on session teardown.
        let _ = timeout(Duration::from_millis(200), portal.release(&session, ReleaseOptions::default())).await;
        let closed = timeout(CLOSE_TIMEOUT, session.close()).await
            .context("Input capture session close timed out")?
            .context("Closing input capture session");
        result.and(closed)
    }.await;
    let disconnected = timeout(CLOSE_TIMEOUT, connection.close())
        .await
        .context("Input capture bus disconnect timed out")?
        .context("Disconnecting input capture bus");
    result.and(disconnected)
}

async fn register_host_app(connection: &zbus::Connection) -> Result<()> {
    // ashpd's AppID requires reverse-DNS syntax and cannot represent our installed
    // host desktop basename. Use the same Registry method with the actual ID;
    // never continue under an inherited editor identity if registration fails.
    let registry = zbus::Proxy::new(
        connection,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.host.portal.Registry",
    )
    .await
    .context("Connecting to host application registry")?;
    let options = std::collections::HashMap::<&str, zbus::Variant<'_>>::new();
    registry
        .call_method("Register", &("oplus-connect", options))
        .await
        .context("Registering installed oplus-connect desktop identity")?;
    Ok(())
}

async fn cancelled(cancel: &mut watch::Receiver<bool>) {
    loop {
        if *cancel.borrow_and_update() {
            return;
        }
        if cancel.changed().await.is_err() {
            return;
        }
    }
}

fn emit(events: &mpsc::Sender<CaptureEvent>, event: CaptureEvent) -> Result<()> {
    events.try_send(event).map_err(|error| match error {
        mpsc::error::TrySendError::Full(_) => {
            anyhow!("Input capture event queue overflowed; stopping to avoid stuck input")
        }
        mpsc::error::TrySendError::Closed(_) => anyhow!("Input capture consumer disconnected"),
    })
}

async fn capture(
    portal: &InputCapture,
    session: &Session<InputCapture>,
    events: &mpsc::Sender<CaptureEvent>,
    enabled: watch::Receiver<bool>,
    direction: ExitDirection,
    return_rx: watch::Receiver<Option<ReturnRequest>>,
) -> Result<()> {
    let session_path = serde_json::to_value(session)?;
    let session_path = session_path
        .as_str()
        .context("Invalid portal session path")?;
    // Subscribe before Start, including while permission and EIS setup are pending.
    let mut closed = session.receive_closed().await?;
    let mut disabled = portal.receive_disabled().await?;
    let mut zones_changed = portal.receive_zones_changed().await?;
    let monitor = async {
        loop {
            tokio::select! {
                _ = closed.next() => bail!("Input capture portal session closed"),
                signal = disabled.next() => {
                    let signal = signal.context("Input capture disabled signal stream ended")?;
                    if signal.session_handle().as_str() == session_path {
                        bail!("Input capture was disabled by the desktop");
                    }
                }
                signal = zones_changed.next() => {
                    let signal = signal.context("Input capture zone signal stream ended")?;
                    if signal.session_handle().as_str() == session_path {
                        bail!("Desktop monitor geometry changed; restart keyboard/mouse sharing");
                    }
                }
            }
        }
    };
    let result = tokio::select! {
        biased;
        result = monitor => result,
        result = capture_events(portal, session, session_path, events, enabled, direction, return_rx) => result,
        _ = events.closed() => bail!("Input capture consumer disconnected"),
    };
    let _ = timeout(
        Duration::from_millis(200),
        portal.release(session, ReleaseOptions::default()),
    )
    .await;
    result
}
#[derive(Clone, Copy, Debug)]
struct ZoneBounds {
    left: f64,
    right: f64,
    top: f64,
    bottom: f64,
}

struct Desktop {
    left: i64,
    top: i64,
    width: u32,
    height: u32,
    direction: ExitDirection,
    barriers: Vec<Barrier>,
    barrier_zones: Vec<ZoneBounds>,
}

impl Desktop {
    fn from_zones(zones: &Zones, direction: ExitDirection) -> Result<Self> {
        Self::from_regions(
            zones.regions().iter().map(|region| {
                (
                    region.width(),
                    region.height(),
                    region.x_offset(),
                    region.y_offset(),
                )
            }),
            direction,
        )
    }

    fn from_regions(
        regions: impl Iterator<Item = (u32, u32, i32, i32)> + Clone,
        direction: ExitDirection,
    ) -> Result<Self> {
        let mut left = i64::MAX;
        let mut top = i64::MAX;
        let mut right = i64::MIN;
        let mut bottom = i64::MIN;
        for (width, height, x, y) in regions.clone() {
            ensure!(
                width > 0 && height > 0,
                "Portal returned an empty desktop zone"
            );
            left = left.min(i64::from(x));
            top = top.min(i64::from(y));
            right = right.max(i64::from(x) + i64::from(width));
            bottom = bottom.max(i64::from(y) + i64::from(height));
        }
        ensure!(left != i64::MAX, "Portal returned no desktop zones");
        let width = u32::try_from(right - left).context("Desktop width exceeds capture range")?;
        let height = u32::try_from(bottom - top).context("Desktop height exceeds capture range")?;
        let edge = match direction {
            ExitDirection::Left => left,
            ExitDirection::Right => right,
        };
        let edge = i32::try_from(edge).context("Desktop edge exceeds portal coordinate range")?;
        let mut barriers = Vec::new();
        let mut barrier_zones = Vec::new();
        for (region_width, region_height, x, y) in regions {
            let region_left = i64::from(x);
            let region_right = region_left + i64::from(region_width);
            let is_edge = match direction {
                ExitDirection::Left => region_left == left,
                ExitDirection::Right => region_right == right,
            };
            // Do not arm internal seams or recessed edges in staggered layouts.
            if !is_edge {
                continue;
            }
            let end_y = i32::try_from(i64::from(y) + i64::from(region_height) - 1)?;
            let position = (edge, y, edge, end_y);
            if barriers.iter().any(|barrier: &Barrier| {
                <(i32, i32, i32, i32)>::from(barrier.position()) == position
            }) {
                continue;
            }
            let id = u32::try_from(barriers.len() + 1)?;
            barriers.push(Barrier::new(
                BarrierID::new(id).context("Invalid barrier id")?,
                position,
            ));
            barrier_zones.push(ZoneBounds {
                left: region_left as f64,
                right: (region_right - 1) as f64,
                top: f64::from(y),
                bottom: f64::from(end_y),
            });
        }
        ensure!(
            !barriers.is_empty(),
            "No configured outer desktop edge is available"
        );
        Ok(Self {
            left,
            top,
            width,
            height,
            direction,
            barriers,
            barrier_zones,
        })
    }

    fn entry_position(&self, cursor: (f64, f64)) -> (f64, f64) {
        let y = ((cursor.1 - self.top as f64) / f64::from(self.height)).clamp(0.0, 1.0);
        let x = match self.direction {
            ExitDirection::Left => 1.0,
            ExitDirection::Right => 0.0,
        };
        (x, y)
    }

    fn return_position(&self, zone: ZoneBounds, pct_y: f64) -> (f64, f64) {
        let y = (self.top as f64 + pct_y.clamp(0.0, 1.0) * f64::from(self.height))
            .clamp(zone.top, zone.bottom);
        let x = match self.direction {
            ExitDirection::Left => (zone.left + 7.0).min(zone.right),
            ExitDirection::Right => (zone.right + 1.0 - 7.0).max(zone.left),
        };
        (x, y)
    }
}

#[derive(Clone, Copy, Debug)]
struct Activation {
    id: u32,
    cursor: (f64, f64),
    zone: ZoneBounds,
}

/// The portal and EIS are independent streams. Neither alone publishes Enter.
/// Keeping retired IDs also handles queued signals delivered after local release.
#[derive(Default)]
struct ActivationOrder {
    active: Option<Activation>,
    pending: Option<Activation>,
    eis_started: Option<u32>,
    deadline: Option<Instant>,
    retired: HashSet<u32>,
}

impl ActivationOrder {
    fn portal_started(&mut self, activation: Activation) -> Result<()> {
        if self.retired.contains(&activation.id) {
            return Ok(());
        }
        ensure!(
            self.active.is_none_or(|active| active.id != activation.id),
            "Portal duplicated an active sequence"
        );
        ensure!(
            self.pending.is_none(),
            "Portal queued multiple capture activations without deactivation"
        );
        self.pending = Some(activation);
        self.deadline
            .get_or_insert_with(|| Instant::now() + ACTIVATION_TIMEOUT);
        Ok(())
    }

    fn eis_started(&mut self, id: u32) -> Result<()> {
        if self.retired.contains(&id) || self.active.is_some_and(|active| active.id == id) {
            return Ok(());
        }
        ensure!(
            self.eis_started.is_none_or(|sequence| sequence == id),
            "EIS started conflicting capture sequences"
        );
        self.eis_started = Some(id);
        self.deadline
            .get_or_insert_with(|| Instant::now() + ACTIVATION_TIMEOUT);
        Ok(())
    }

    fn activate(&mut self) -> Option<Activation> {
        let pending = self.pending?;
        if self.active.is_some() || self.eis_started != Some(pending.id) {
            return None;
        }
        self.pending = None;
        self.eis_started = None;
        self.deadline = None;
        self.active = Some(pending);
        Some(pending)
    }

    fn deactivate(&mut self, id: u32) -> bool {
        self.retired.insert(id);
        let was_active = self.active.is_some_and(|active| active.id == id);
        if was_active {
            self.active = None;
        }
        if self.pending.is_some_and(|pending| pending.id == id) {
            self.pending = None;
        }
        if self.eis_started == Some(id) {
            self.eis_started = None;
        }
        if self.pending.is_none() && self.eis_started.is_none() {
            self.deadline = None;
        }
        was_active
    }

    fn return_activation(&self, request: ReturnRequest) -> Option<Activation> {
        self.active
            .filter(|active| u64::from(active.id) == request.activation)
    }
}

struct InputDevice {
    device: Device,
    sequence: Option<u32>,
    continuous: (f64, f64),
    discrete: (Option<f64>, Option<f64>),
}

impl InputDevice {
    fn reset_scroll(&mut self) {
        self.continuous = (0.0, 0.0);
        self.discrete = (None, None);
    }
}

fn device_mut<'a>(devices: &'a mut [InputDevice], device: &Device) -> Result<&'a mut InputDevice> {
    devices
        .iter_mut()
        .find(|entry| entry.device == *device)
        .context("EIS sent input for an unknown device")
}

fn is_current(devices: &[InputDevice], device: &Device, active: Option<Activation>) -> bool {
    active.is_some_and(|activation| {
        devices
            .iter()
            .any(|entry| entry.device == *device && entry.sequence == Some(activation.id))
    })
}

async fn capture_events(
    portal: &InputCapture,
    session: &Session<InputCapture>,
    session_path: &str,
    events: &mpsc::Sender<CaptureEvent>,
    mut enabled: watch::Receiver<bool>,
    direction: ExitDirection,
    mut return_rx: watch::Receiver<Option<ReturnRequest>>,
) -> Result<()> {
    let mut activated = portal.receive_activated().await?;
    let mut deactivated = portal.receive_deactivated().await?;
    let capabilities = Capabilities::KEYBOARD | Capabilities::POINTER;
    let response = portal
        .start(
            session,
            None,
            StartOptions::default().set_capabilities(capabilities),
        )
        .await
        .context("Requesting global keyboard/mouse capture permission")?
        .response()?;
    ensure!(
        response.capabilities().contains(capabilities),
        "Desktop did not grant both keyboard and pointer capture"
    );
    let zones = portal
        .zones(session, Default::default())
        .await?
        .response()?;
    let desktop = Desktop::from_zones(&zones, direction)?;
    let barriers = portal
        .set_pointer_barriers(
            session,
            &desktop.barriers,
            zones.zone_set(),
            Default::default(),
        )
        .await?
        .response()?;
    ensure!(
        barriers.failed_barriers().is_empty(),
        "Desktop rejected configured outer-edge capture barriers"
    );
    let fd = portal.connect_to_eis(session, Default::default()).await?;
    let socket = UnixStream::from(fd);
    socket.set_nonblocking(true)?;
    let context = ei::Context::new(socket)?;
    let (_connection, mut input) = context
        .handshake_tokio(
            "Oplus Connect keyboard/mouse sharing",
            ei::handshake::ContextType::Receiver,
        )
        .await
        .context("Negotiating EIS input transport")?;
    emit(
        events,
        CaptureEvent::Ready {
            width: desktop.width,
            height: desktop.height,
        },
    )?;

    let mut armed = false;
    let mut order = ActivationOrder::default();
    let mut devices: Vec<InputDevice> = Vec::new();
    let mut ctrl_held = false;
    let mut alt_held = false;
    loop {
        if !armed && *enabled.borrow_and_update() {
            portal.enable(session, Default::default()).await?;
            armed = true;
        }
        if let Some(active) = order.activate() {
            let (pct_x, pct_y) = desktop.entry_position(active.cursor);
            emit(
                events,
                CaptureEvent::Activated {
                    activation: u64::from(active.id),
                    pct_x,
                    pct_y,
                },
            )?;
        }
        let activation_deadline = order
            .deadline
            .unwrap_or_else(|| Instant::now() + ACTIVATION_TIMEOUT);
        tokio::select! {
            biased;
            changed = return_rx.changed() => {
                changed.context("Pad return control disconnected")?;
                let request = *return_rx.borrow_and_update();
                if let Some(request) = request {
                    if let Some(active) = order.return_activation(request) {
                        ensure!(request.pct_x.is_finite() && request.pct_y.is_finite(), "Pad return request contained invalid coordinates");
                        portal.release(session, ReleaseOptions::default()
                            .set_activation_id(active.id)
                            .set_cursor_position(desktop.return_position(active.zone, request.pct_y))).await?;
                        order.deactivate(active.id);
                        ctrl_held = false;
                        alt_held = false;
                        for device in &mut devices { device.reset_scroll(); }
                        // Release keeps the barriers enabled. Do not disable/re-enable:
                        // emit Leave before another matching portal/EIS pair can Enter.
                        emit(events, CaptureEvent::Deactivated)?;
                    }
                }
            }
            signal = deactivated.next() => {
                let signal = signal.context("Input capture deactivation stream ended")?;
                if signal.session_handle().as_str() != session_path { continue; }
                let id = signal.activation_id().context("Portal deactivation omitted its EIS sequence id")?;
                if order.deactivate(id) {
                    ctrl_held = false;
                    alt_held = false;
                    for device in &mut devices { device.reset_scroll(); }
                    emit(events, CaptureEvent::Deactivated)?;
                }
            }
            signal = activated.next() => {
                let signal = signal.context("Input capture activation stream ended")?;
                if signal.session_handle().as_str() != session_path { continue; }
                ensure!(armed, "Desktop activated capture before the Pad was connected");
                let id = signal.activation_id().context("Portal activation omitted its EIS sequence id")?;
                let barrier = match signal.barrier_id() {
                    Some(ActivatedBarrier::Barrier(id)) => id,
                    _ => bail!("Desktop activated capture without the configured outer-edge barrier"),
                };
                let index = desktop.barriers.iter().position(|entry| entry.barrier_id() == barrier)
                    .context("Desktop activated an unknown capture barrier")?;
                let (x, y) = signal.cursor_position().context("Portal activation omitted cursor position")?;
                let cursor = (f64::from(x), f64::from(y));
                ensure!(cursor.0.is_finite() && cursor.1.is_finite(), "Portal supplied invalid cursor coordinates");
                order.portal_started(Activation { id, cursor, zone: desktop.barrier_zones[index] })?;
            }
            changed = enabled.changed() => {
                changed.context("Pad capture enable gate disconnected")?;
                ensure!(!armed || *enabled.borrow_and_update(), "Pad capture enable gate was revoked");
            }
            _ = tokio::time::sleep_until(activation_deadline), if order.deadline.is_some() => {
                bail!("Portal and EIS did not complete the matching capture activation");
            }
            // If EIS arrived first, leave its following key/button edges queued
            // until the portal identifies the activation. Portal-first still reads
            // through old StopEmulating events to the new StartEmulating.
            event = input.next(), if order.eis_started.is_none() => {
                let event = event.context("EIS input transport disconnected")??;
                match event {
                    EiEvent::Disconnected(_) => bail!("EIS server disconnected capture"),
                    EiEvent::SeatAdded(event) => {
                        event.seat.bind_capabilities(DeviceCapability::Pointer | DeviceCapability::Keyboard | DeviceCapability::Button | DeviceCapability::Scroll);
                        context.flush().context("Binding EIS input capabilities")?;
                    }
                    EiEvent::SeatRemoved(_) => bail!("EIS input seat was removed"),
                    EiEvent::DeviceAdded(event) => devices.push(InputDevice {
                        device: event.device, sequence: None,
                        continuous: (0.0, 0.0), discrete: (None, None),
                    }),
                    EiEvent::DeviceRemoved(event) => {
                        ensure!(!is_current(&devices, &event.device, order.active), "Active EIS input device was removed");
                        devices.retain(|entry| entry.device != event.device);
                    }
                    EiEvent::DevicePaused(event) => {
                        ensure!(!is_current(&devices, &event.device, order.active), "Active EIS input device was paused");
                        let device = device_mut(&mut devices, &event.device)?;
                        device.sequence = None;
                        device.reset_scroll();
                    }
                    EiEvent::DeviceStartEmulating(event) => {
                        let device = device_mut(&mut devices, &event.device)?;
                        device.sequence = Some(event.sequence);
                        device.reset_scroll();
                        order.eis_started(event.sequence)?;
                    }
                    EiEvent::DeviceStopEmulating(event) => {
                        let device = device_mut(&mut devices, &event.device)?;
                        device.sequence = None;
                        device.reset_scroll();
                    }
                    EiEvent::PointerMotion(event) if is_current(&devices, &event.device, order.active) => {
                        emit(events, CaptureEvent::Motion { dx: f64::from(event.dx), dy: f64::from(event.dy) })?;
                    }
                    EiEvent::Button(event) if is_current(&devices, &event.device, order.active) => {
                        tracing::debug!(code = event.button, pressed = (event.state == ButtonState::Press), "EIS button captured");
                        emit(events, CaptureEvent::Button { code: event.button, pressed: event.state == ButtonState::Press })?;
                    }
                    EiEvent::KeyboardKey(event) if is_current(&devices, &event.device, order.active) => {
                        match event.key {
                            29 | 97 => ctrl_held = event.state == KeyState::Press,
                            56 | 100 => alt_held = event.state == KeyState::Press,
                            1 if event.state == KeyState::Press && ctrl_held && alt_held => {
                                tracing::warn!("emergency host escape shortcut (Ctrl+Alt+Escape) triggered");
                                if let Some(active) = order.active {
                                    let _ = portal.release(session, ReleaseOptions::default()
                                        .set_activation_id(active.id)
                                        .set_cursor_position(desktop.return_position(active.zone, 0.5))).await;
                                    order.deactivate(active.id);
                                } else {
                                    let _ = portal.release(session, ReleaseOptions::default()).await;
                                }
                                ctrl_held = false;
                                alt_held = false;
                                emit(events, CaptureEvent::Deactivated)?;
                                continue;
                            }
                            _ => {}
                        }
                        emit(events, CaptureEvent::Key { code: event.key, pressed: event.state == KeyState::Press })?;
                    }
                    EiEvent::ScrollDelta(event) if is_current(&devices, &event.device, order.active) => {
                        let device = device_mut(&mut devices, &event.device)?;
                        device.continuous.0 += f64::from(event.dx) / 15.0;
                        device.continuous.1 += f64::from(event.dy) / 15.0;
                    }
                    EiEvent::ScrollDiscrete(event) if is_current(&devices, &event.device, order.active) => {
                        let device = device_mut(&mut devices, &event.device)?;
                        if event.discrete_dx != 0 { device.discrete.0 = Some(f64::from(event.discrete_dx) / 120.0); }
                        if event.discrete_dy != 0 { device.discrete.1 = Some(f64::from(event.discrete_dy) / 120.0); }
                    }
                    EiEvent::Frame(event) if is_current(&devices, &event.device, order.active) => {
                        let device = device_mut(&mut devices, &event.device)?;
                        let dx = device.discrete.0.unwrap_or(device.continuous.0);
                        let dy = device.discrete.1.unwrap_or(device.continuous.1);
                        device.reset_scroll();
                        if dx != 0.0 || dy != 0.0 { emit(events, CaptureEvent::Scroll { dx, dy })?; }
                    }
                    _ => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn activation(id: u32) -> Activation {
        Activation {
            id,
            cursor: (0.0, 540.0),
            zone: ZoneBounds {
                left: 0.0,
                right: 1919.0,
                top: 0.0,
                bottom: 1079.0,
            },
        }
    }

    #[test]
    fn entry_uses_opposite_edge_and_preserves_captured_y() {
        let regions = [(1920, 1080, 0, 0)];
        let left = Desktop::from_regions(regions.into_iter(), ExitDirection::Left).unwrap();
        let right = Desktop::from_regions(regions.into_iter(), ExitDirection::Right).unwrap();
        assert_eq!(left.entry_position((0.0, 540.0)), (1.0, 0.5));
        assert_eq!(right.entry_position((1920.0, 540.0)), (0.0, 0.5));
        assert_eq!(
            left.return_position(left.barrier_zones[0], 0.5),
            (7.0, 540.0)
        );
        assert_eq!(
            right.return_position(right.barrier_zones[0], 0.5),
            (1913.0, 540.0)
        );
    }

    #[test]
    fn only_outer_edges_are_armed_and_return_stays_in_triggering_zone() {
        let regions = [(1920, 1080, -1920, -200), (1280, 720, 0, 0)];
        let left = Desktop::from_regions(regions.into_iter(), ExitDirection::Left).unwrap();
        let right = Desktop::from_regions(regions.into_iter(), ExitDirection::Right).unwrap();
        assert_eq!(left.barriers.len(), 1);
        assert_eq!(right.barriers.len(), 1);
        assert_eq!(
            <(i32, i32, i32, i32)>::from(left.barriers[0].position()),
            (-1920, -200, -1920, 879)
        );
        assert_eq!(
            <(i32, i32, i32, i32)>::from(right.barriers[0].position()),
            (1280, 0, 1280, 719)
        );
        assert_eq!(
            right.return_position(right.barrier_zones[0], 0.0),
            (1273.0, 0.0)
        );
        assert_eq!(
            right.return_position(right.barrier_zones[0], 1.0),
            (1273.0, 719.0)
        );
        assert_eq!(left.entry_position((-1920.0, 340.0)), (1.0, 0.5));
    }

    #[test]
    fn return_inset_is_bounded_by_narrow_zone() {
        let regions = [(3, 2, -3, -2)];
        let left = Desktop::from_regions(regions.into_iter(), ExitDirection::Left).unwrap();
        let right = Desktop::from_regions(regions.into_iter(), ExitDirection::Right).unwrap();
        assert_eq!(
            left.return_position(left.barrier_zones[0], 1.0),
            (-1.0, -1.0)
        );
        assert_eq!(
            right.return_position(right.barrier_zones[0], 0.0),
            (-3.0, -2.0)
        );
    }

    #[test]
    fn activation_requires_matching_portal_and_eis_in_either_order() {
        for eis_first in [false, true] {
            let mut order = ActivationOrder::default();
            if eis_first {
                order.eis_started(10).unwrap();
            } else {
                order.portal_started(activation(10)).unwrap();
            }
            assert!(order.activate().is_none());
            if eis_first {
                order.portal_started(activation(10)).unwrap();
            } else {
                order.eis_started(10).unwrap();
            }
            assert_eq!(order.activate().unwrap().id, 10);
            assert!(order.activate().is_none());
        }
    }

    #[test]
    fn queued_deactivation_precedes_new_activation_and_stale_return_is_ignored() {
        let mut order = ActivationOrder::default();
        order.portal_started(activation(10)).unwrap();
        order.eis_started(10).unwrap();
        order.activate().unwrap();
        order.portal_started(activation(11)).unwrap();
        order.eis_started(11).unwrap();
        assert!(order.activate().is_none());
        assert!(order.deactivate(10));
        assert_eq!(order.activate().unwrap().id, 11);
        assert!(!order.deactivate(10));
        assert_eq!(order.active.unwrap().id, 11);
        let request = ReturnRequest {
            activation: 10,
            pct_x: 1.0,
            pct_y: 0.5,
        };
        assert!(order.return_activation(request).is_none());
        assert!(
            order
                .return_activation(ReturnRequest {
                    activation: 11,
                    ..request
                })
                .is_some()
        );
    }

    #[test]
    fn late_retired_device_and_portal_events_do_not_reactivate() {
        let mut order = ActivationOrder::default();
        order.deactivate(10);
        order.portal_started(activation(10)).unwrap();
        order.eis_started(10).unwrap();
        assert!(order.activate().is_none());
        assert!(order.eis_started.is_none());
        order.eis_started(11).unwrap();
        order.portal_started(activation(11)).unwrap();
        assert_eq!(order.activate().unwrap().id, 11);
    }
}
