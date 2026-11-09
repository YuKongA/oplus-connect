//! Independent keyboard/mouse sharing lifecycle; no video session is involved.
use crate::{
    client::{CastRequestRejected, DaemonClient},
    kms_capture::{self, CaptureEvent},
    kms_input::InputMapper,
};
use anyhow::{Context, Result, anyhow};
use std::{
    future::Future,
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, watch};

// Outlast the client's 17-second and daemon's 15-second start deadlines so
// cleanup cannot race an otherwise still-running daemon start request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
type ShutdownSignal = (watch::Sender<bool>, watch::Receiver<bool>);
static ACTIVE_SESSION: std::sync::Mutex<Option<ShutdownSignal>> = std::sync::Mutex::new(None);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum KmsPhase {
    #[default]
    Idle,
    Preparing,
    Waiting,
    Armed,
    Sharing,
    Stopping,
    Failed,
}

#[derive(Default)]
pub struct KmsViewState {
    pub device_id: String,
    pub generation: u64,
    pub phase: KmsPhase,
    pub detail: String,
}

impl KmsViewState {
    pub fn active(&self) -> bool {
        !matches!(self.phase, KmsPhase::Idle | KmsPhase::Failed)
    }
}

pub enum KmsEvent {
    Phase(KmsPhase),
    Closed,
    Error(String),
}

pub struct KmsSessionHandle {
    cancel: watch::Sender<bool>,
    done: watch::Receiver<bool>,
}

impl KmsSessionHandle {
    pub fn stop(&self) {
        let _ = self.cancel.send(true);
    }

    pub fn completion(&self) -> watch::Receiver<bool> {
        self.done.clone()
    }
}

impl Drop for KmsSessionHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

async fn wait_done(mut done: watch::Receiver<bool>) {
    while !*done.borrow() {
        if done.changed().await.is_err() {
            break;
        }
    }
}

pub async fn shutdown() {
    let active = ACTIVE_SESSION
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take();
    if let Some((cancel, done)) = active {
        let _ = cancel.send(true);
        wait_done(done).await;
    }
}

pub fn start(device_id: String, events: mpsc::Sender<KmsEvent>) -> KmsSessionHandle {
    let (cancel, cancel_rx) = watch::channel(false);
    let (done_tx, done) = watch::channel(false);
    let previous = ACTIVE_SESSION
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .replace((cancel.clone(), done.clone()));
    let worker_cancel = cancel.clone();
    crate::TOKIO_RT.spawn(async move {
        // Serialize ownership even if a root view is replaced while its old
        // portal and daemon session are still closing.
        if let Some((cancel, done)) = previous {
            let _ = cancel.send(true);
            wait_done(done).await;
        }
        let result = if *cancel_rx.borrow() {
            Ok(())
        } else {
            run_session(&device_id, &events, worker_cancel, cancel_rx).await
        };
        let event = match result {
            Ok(()) => KmsEvent::Closed,
            Err(error) => KmsEvent::Error(format!("键鼠共享已停止：{error:#}")),
        };
        let _ = done_tx.send(true);
        let _ = events.send(event).await;
    });
    KmsSessionHandle { cancel, done }
}

async fn cancelled(cancel: &mut watch::Receiver<bool>) {
    while !*cancel.borrow() {
        if cancel.changed().await.is_err() {
            break;
        }
    }
}

// Only the daemon's exact rejection for a generation-bound operation means
// the peer disconnected. Context and unrelated errors must remain visible.
fn connection_result<T>(result: Result<T>) -> Result<Option<T>> {
    match result {
        Ok(value) => Ok(Some(value)),
        Err(error)
            if error
                .downcast_ref::<CastRequestRejected>()
                .is_some_and(|rejection| rejection.0 == "KMS connection ID is stale") =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}
async fn request<T>(future: impl Future<Output = Result<T>>) -> Result<T> {
    tokio::time::timeout(REQUEST_TIMEOUT, future)
        .await
        .context("守护进程请求超时")?
}

async fn run_session(
    device_id: &str,
    events: &mpsc::Sender<KmsEvent>,
    cancel_tx: watch::Sender<bool>,
    mut cancel: watch::Receiver<bool>,
) -> Result<()> {
    let direction = crate::kms_settings::direction();
    let (capture_tx, mut capture_rx) = mpsc::channel(1024);
    let (enable, enable_rx) = watch::channel(false);
    let (return_tx, return_rx) = watch::channel(None);
    let capture_cancel = cancel.clone();
    // The EIS stream is !Send. Construct and drive it entirely on one blocking
    // worker; cancellation remains cooperative and cleanup always joins it.
    let capture = tokio::task::spawn_blocking(move || {
        crate::TOKIO_RT.block_on(kms_capture::run(
            capture_tx,
            capture_cancel,
            enable_rx,
            direction,
            return_rx,
        ))
    });
    let client = DaemonClient::new();
    let mut started = false;
    let mut connection_id = None;
    async fn fast_input(
        client: &DaemonClient,
        device_id: &str,
        connection_id: u64,
        event: &oplus_connect_daemon::kms::input::InputEvent,
    ) -> Result<()> {
        tokio::time::timeout(
            Duration::from_millis(1200),
            client.kms_input(device_id, connection_id, event),
        )
        .await
        .context("发送设备输入请求超时")?
        .context("发送设备输入失败")
    }

    async fn flush_motion(
        client: &DaemonClient,
        device_id: &str,
        connection_id: u64,
        dx: &mut i32,
        dy: &mut i32,
    ) -> Result<()> {
        if *dx != 0 || *dy != 0 {
            let delta_x = *dx;
            let delta_y = *dy;
            *dx = 0;
            *dy = 0;
            fast_input(
                client,
                device_id,
                connection_id,
                &oplus_connect_daemon::kms::input::InputEvent::Move { delta_x, delta_y },
            )
            .await?;
        }
        Ok(())
    }

    let result = async {
        let ready = tokio::select! {
            biased;
            _ = cancelled(&mut cancel) => return Ok(()),
            event = capture_rx.recv() => event.context("系统输入捕获未能就绪")?,
        };
        let CaptureEvent::Ready { width, height } = ready else {
            return Err(anyhow!("系统输入捕获在就绪前发送了输入"));
        };
        if *cancel.borrow() { return Ok(()); }
        if capture.is_finished() { return Err(anyhow!("系统输入捕获已关闭")); }
        tracing::debug!(device_id, phase = ?KmsPhase::Waiting, "kms phase");
        let _ = events.try_send(KmsEvent::Phase(KmsPhase::Waiting));
        started = true;
        // A start request can already have reached the daemon. Finish it before
        // issuing stop, including when cancellation occurs during the request.
        request(client.kms_start(device_id, width, height, direction.wire_value())).await.context("启动键鼠共享失败")?;
        let connected = tokio::time::timeout(CONNECT_TIMEOUT, async {
            let mut poll = tokio::time::interval(Duration::from_millis(250));
            poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                if *cancel.borrow() { return Ok(None); }
                if capture.is_finished() { return Err(anyhow!("系统输入捕获已关闭")); }
                tokio::select! {
                    biased;
                    _ = cancelled(&mut cancel) => return Ok(None),
                    event = capture_rx.recv() => match event {
                        // Never replay a stale activation or held input after
                        // connection. The disabled portal must not capture yet.
                        Some(CaptureEvent::Deactivated) => {},
                        Some(_) => return Err(anyhow!("设备就绪前系统意外捕获了输入")),
                        None => return Err(anyhow!("系统输入捕获已关闭")),
                    },
                    _ = poll.tick() => {
                        let status = request(client.kms_status(device_id)).await?;
                        if status.status.as_deref() == Some("connected") {
                            if let Some(id) = status.connection_id { return Ok(Some(id)); }
                        }
                    }
                }
            }
        }).await.context("等待键鼠连接超时，请在设备确认连接")??;
        let Some(id) = connected else { return Ok(()); };
        connection_id = Some(id);
        if *cancel.borrow() { return Ok(()); }
        // Status requests may have been in flight when an event arrived.
        // Discard only deactivation; never arm with queued pre-connection input.
        while let Ok(event) = capture_rx.try_recv() {
            if !matches!(event, CaptureEvent::Deactivated) {
                return Err(anyhow!("平板就绪前系统意外捕获了输入"));
            }
        }
        let mut mapper = InputMapper::default();
        enable.send(true).context("系统输入捕获已退出")?;
        tracing::debug!(device_id, phase = ?KmsPhase::Armed, "kms phase");
        let _ = events.try_send(KmsEvent::Phase(KmsPhase::Armed));
        let mut sharing = false;
        let mut activation = None;
        let mut return_sent = None;
        let mut pending_dx: i32 = 0;
        let mut pending_dy: i32 = 0;
        let mut status_poll = tokio::time::interval(Duration::from_millis(500));
        status_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut status_failures = 0u32;
        let mut status_ticks = 0u64;
        // Host capture liveness: a silent compositor and an idle user look the
        // same to this loop, so record when the last capture event arrived and
        // report it periodically instead of guessing.
        let mut last_capture_at = Instant::now();
        let mut return_poll = tokio::time::interval(Duration::from_millis(60));
        return_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut motion_flush = tokio::time::interval(Duration::from_millis(8));
        motion_flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            if *cancel.borrow() { return Ok(()); }
            if capture.is_finished() { return Err(anyhow!("系统输入捕获已关闭")); }
            tokio::select! {
                _ = cancelled(&mut cancel) => return Ok(()),
                // A return is a control-plane edge: service it ahead of the
                // potentially busy motion queue, then drop input until the
                // matching deactivation arrives.
                _ = return_poll.tick(), if sharing && activation.is_some() && return_sent.is_none() => {
                    let receive_res = tokio::time::timeout(Duration::from_millis(400), client.kms_receive(device_id, id)).await;
                    let Some(received) = connection_result(receive_res.unwrap_or(Ok(None)))? else { return Ok(()); };
                    let Some((pct_x, pct_y)) = received else { continue; };
                    let Some(current) = activation else { continue; };
                    if !sharing || activation != Some(current) || return_sent.is_some() { continue; }
                    tracing::debug!(device_id, activation = current, pct_x, pct_y, "kms cross-back return");
                    pending_dx = 0;
                    pending_dy = 0;
                    return_tx.send(Some(kms_capture::ReturnRequest { activation: current, pct_x, pct_y }))
                        .context("系统输入返回通道已退出")?;
                    return_sent = Some(current);
                },
                _ = motion_flush.tick(), if sharing && (pending_dx != 0 || pending_dy != 0) => {
                    if connection_result(flush_motion(&client, device_id, id, &mut pending_dx, &mut pending_dy).await)?.is_none() { return Ok(()); }
                },
                _ = status_poll.tick() => {
                    status_ticks = status_ticks.saturating_add(1);
                    if sharing && status_ticks % 10 == 0 {
                        tracing::debug!(
                            device_id,
                            idle_ms = last_capture_at.elapsed().as_millis() as u64,
                            return_sent = return_sent.is_some(),
                            "kms sharing heartbeat"
                        );
                    }
                    let status_res = tokio::time::timeout(Duration::from_millis(600), client.kms_status(device_id)).await;
                    match status_res {
                        Ok(Ok(status)) => {
                            status_failures = 0;
                            if status.connection_id != Some(id) || status.status.as_deref() != Some("connected") {
                                return Ok(());
                            }
                        }
                        _ => {
                            status_failures += 1;
                            if status_failures == 1 || status_failures % 10 == 0 {
                                tracing::warn!(
                                    device_id,
                                    status_failures,
                                    "KMS daemon status poll failed while sharing"
                                );
                            }
                        }
                    }
                },
                event = capture_rx.recv() => {
                    let event = event.context("系统输入捕获已关闭")?;
                    last_capture_at = Instant::now();
                    match &event {
                        CaptureEvent::Ready { .. } => return Err(anyhow!("显示器布局已改变，请重新启动共享")),
                        CaptureEvent::Activated { activation: current, .. } => {
                            if sharing { return Err(anyhow!("系统输入捕获重复激活")); }
                            mapper.reset();
                            // Clear any return request from the retired activation
                            // before allowing input for this one.
                            return_tx.send(None).context("系统输入返回通道已退出")?;
                            activation = Some(*current);
                            return_sent = None;
                            pending_dx = 0;
                            pending_dy = 0;
                            sharing = true;
                            tracing::debug!(device_id, activation = *current, "kms enter activation");
                            tracing::debug!(device_id, phase = ?KmsPhase::Sharing, "kms phase");
                            let _ = events.try_send(KmsEvent::Phase(KmsPhase::Sharing));
                        }
                        CaptureEvent::Deactivated => {
                            let retired = activation.take();
                            return_sent = None;
                            pending_dx = 0;
                            pending_dy = 0;
                            return_tx.send(None).context("系统输入返回通道已退出")?;
                            tracing::debug!(device_id, activation = ?retired, "kms activation returned");
                            if connection_result(request(client.kms_release(device_id, id)).await.context("释放按键失败"))?.is_none() {
                                return Ok(());
                            }
                            mapper.reset();
                            sharing = false;
                            tracing::debug!(device_id, phase = ?KmsPhase::Armed, "kms phase");
                            let _ = events.try_send(KmsEvent::Phase(KmsPhase::Armed));
                            continue;
                        }
                        // Once a return is requested, no queued input from that
                        // activation may be sent while the portal is leaving.
                        _ if return_sent.is_some() => continue,
                        _ if !sharing => continue,
                        _ => {}
                    }
                    for input in mapper.translate(&event) {
                        if *cancel.borrow() { return Ok(()); }
                        match input {
                            oplus_connect_daemon::kms::input::InputEvent::Move { delta_x, delta_y } => {
                                pending_dx = pending_dx.saturating_add(delta_x);
                                pending_dy = pending_dy.saturating_add(delta_y);
                            }
                            discrete => {
                                if connection_result(flush_motion(&client, device_id, id, &mut pending_dx, &mut pending_dy).await)?.is_none() { return Ok(()); }
                                tracing::debug!(?discrete, "sending discrete event to peer");
                                if connection_result(fast_input(&client, device_id, id, &discrete).await)?.is_none() { return Ok(()); }
                            }
                        }
                    }
                }
            }
        }
    }.await;

    // Always restore the host pointer and close the portal, even if starting
    // the daemon failed. Closing the receiver also unblocks any pending send.
    let _ = return_tx.send(None);
    let _ = enable.send(false);
    let _ = cancel_tx.send(true);
    drop(capture_rx);
    let cleanup = async {
        let release = if let Some(id) = connection_id {
            match tokio::time::timeout(
                Duration::from_millis(1500),
                client.kms_release(device_id, id),
            )
            .await
            {
                Ok(result) => connection_result(result.context("释放按键失败")).map(|_| ()),
                Err(_) => Err(anyhow!("释放按键超时")),
            }
        } else {
            Ok(())
        };
        let stop = if started {
            tokio::time::timeout(Duration::from_millis(1500), client.kms_stop(device_id))
                .await
                .map_err(|_| anyhow!("停止键鼠共享超时"))
                .and_then(|r| r.context("停止键鼠共享失败"))
        } else {
            Ok(())
        };
        combine(release, stop)
    };
    let (portal, cleanup) = tokio::join!(capture, cleanup);
    let portal = portal
        .context("系统输入捕获任务异常退出")
        .and_then(|result| result);
    combine(combine(result, portal), cleanup)
}

fn combine(first: Result<()>, second: Result<()>) -> Result<()> {
    match (first, second) {
        (Err(first), Err(second)) => Err(anyhow!("{first:#}；{second:#}")),
        (Err(error), _) | (_, Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_exact_daemon_stale_rejection_is_a_disconnect() {
        for message in [
            "KMS connection ID is stale but write failed",
            "device is not connected",
            "KMS input queue is full or closed",
        ] {
            let error =
                anyhow::Error::new(CastRequestRejected(message.to_owned())).context("input");
            assert!(connection_result::<()>(Err(error)).is_err());
        }
        assert!(connection_result::<()>(Err(anyhow!("KMS connection ID is stale"))).is_err());
        assert!(connection_result::<()>(Err(anyhow!("request timed out"))).is_err());
    }
}
