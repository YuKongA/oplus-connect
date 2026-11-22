//! Platform clipboard adapter used by the protocol core.
//!
//! Linux uses arboard's native backend: Wayland data-control is preferred when
//! available and X11/XWayland is used as the fallback. Windows uses the native
//! Win32 backend exposed by the same crate.

#[cfg(unix)]
fn read_text_impl() -> Option<String> {
    let mut clipboard = arboard::Clipboard::new().ok()?;
    clipboard.get_text().ok()
}

#[cfg(unix)]
fn write_text_impl(text: &str) -> bool {
    let Ok(mut clipboard) = arboard::Clipboard::new() else {
        return false;
    };
    clipboard.set_text(text).is_ok()
}

/// Returns the current clipboard text, or `None` when the backend cannot read
/// text (empty clipboard, unsupported format, or unavailable desktop session).
pub fn read_text() -> Option<String> {
    read_text_impl()
}

/// Replaces the clipboard with `text`. Returns `true` when the backend accepted
/// the write.
pub fn write_text(text: &str) -> bool {
    write_text_impl(text)
}

#[cfg(windows)]
fn read_text_impl() -> Option<String> {
    let mut clipboard = arboard::Clipboard::new().ok()?;
    clipboard.get_text().ok()
}

#[cfg(windows)]
fn write_text_impl(text: &str) -> bool {
    let Ok(mut clipboard) = arboard::Clipboard::new() else {
        return false;
    };
    clipboard.set_text(text).is_ok()
}

use std::sync::{Arc, LazyLock, Mutex, mpsc};
use std::time::Duration;
use tokio::sync::{Semaphore, oneshot, watch};
use tracing::warn;

// Share results across sessions and the frontend poller without copying text.
struct Sample {
    text: Arc<String>,
    revision: u64,
}

#[derive(Default)]
struct ReadState {
    pending: Option<ReadRequest>,
    revision: u64,
}

#[derive(Clone)]
struct ReadRequest {
    result: watch::Receiver<Option<Sample>>,
    started: tokio::time::Instant,
}

type ReadJob = watch::Sender<Option<Sample>>;

struct ReadWorker {
    state: Arc<Mutex<ReadState>>,
    queue: mpsc::SyncSender<ReadJob>,
}

impl ReadWorker {
    fn new(read: impl Fn() -> Option<String> + Send + 'static) -> std::io::Result<Self> {
        let (queue, jobs) = mpsc::sync_channel::<ReadJob>(1);
        let state = Arc::new(Mutex::new(ReadState::default()));
        let worker_state = Arc::clone(&state);
        // Do not register with Tokio: a backend read can outlive the runtime.
        std::thread::Builder::new()
            .name("oplus-clipboard-read".into())
            .spawn(move || {
                while let Ok(reply) = jobs.recv() {
                    let revision = worker_state.lock().unwrap().revision;
                    let text = read().map(Arc::new);
                    let mut state = worker_state.lock().unwrap();
                    if let Some(text) = text.filter(|_| revision == state.revision) {
                        let _ = reply.send(Some(Sample { text, revision }));
                    }
                    state.pending = None;
                }
            })?;
        Ok(Self { state, queue })
    }

    fn request(&self, slow_after: Duration) -> Option<PendingRead> {
        let mut state = self.state.lock().unwrap();
        let request = match &state.pending {
            Some(request) => request.clone(),
            None => {
                let (reply, result) = watch::channel(None);
                let request = ReadRequest {
                    result,
                    started: tokio::time::Instant::now(),
                };
                self.queue.try_send(reply).ok()?;
                state.pending = Some(request.clone());
                request
            }
        };
        Some(PendingRead {
            state: Arc::clone(&self.state),
            result: request.result,
            warn_at: Some(request.started + slow_after),
        })
    }
}

pub(crate) struct PendingRead {
    state: Arc<Mutex<ReadState>>,
    result: watch::Receiver<Option<Sample>>,
    warn_at: Option<tokio::time::Instant>,
}

impl PendingRead {
    // Cancellation-safe: the receiver stays with the caller when a socket
    // read or command wins select. A slow read still delivers its result.
    pub(crate) async fn finish(&mut self) -> Option<Arc<String>> {
        let changed = if let Some(deadline) = self.warn_at {
            match tokio::time::timeout_at(deadline, self.result.changed()).await {
                Ok(changed) => changed,
                Err(_) => {
                    self.warn_at = None;
                    warn_slow_read();
                    self.result.changed().await
                }
            }
        } else {
            self.result.changed().await
        };
        changed.ok()?;
        let result = self.result.borrow();
        let sample = result.as_ref()?;
        // Do not send samples captured before a successful clipboard write.
        // Release the watch lock before taking the worker's mutex.
        let revision = sample.revision;
        let text = Arc::clone(&sample.text);
        drop(result);
        (revision == self.state.lock().unwrap().revision).then_some(text)
    }
}

static READER: LazyLock<Option<ReadWorker>> = LazyLock::new(|| match ReadWorker::new(read_text) {
    Ok(worker) => Some(worker),
    Err(error) => {
        warn!("cannot start clipboard read worker: {error}");
        None
    }
});

pub(crate) fn request_read() -> Option<PendingRead> {
    static SLOW_AFTER: LazyLock<Duration> = LazyLock::new(|| {
        match std::env::var("OPLUS_CLIPBOARD_READ_TIMEOUT_MS") {
            Ok(value) => match value.parse::<u64>() {
                Ok(ms) if (100..=30_000).contains(&ms) => return Duration::from_millis(ms),
                _ => warn!("invalid OPLUS_CLIPBOARD_READ_TIMEOUT_MS; expected 100–30000"),
            },
            Err(std::env::VarError::NotPresent) => {}
            Err(_) => warn!("invalid OPLUS_CLIPBOARD_READ_TIMEOUT_MS; expected UTF-8"),
        }
        Duration::from_millis(500)
    });
    READER.as_ref()?.request(*SLOW_AFTER)
}

fn warn_slow_read() {
    static LAST_WARNING: Mutex<Option<std::time::Instant>> = Mutex::new(None);
    let mut last = LAST_WARNING.lock().unwrap();
    if last.is_none_or(|time| time.elapsed() >= Duration::from_secs(30)) {
        *last = Some(std::time::Instant::now());
        warn!("clipboard backend read is slow; still waiting for its result");
    }
}

/// Writes are also isolated from runtime shutdown. Only one write may be in
/// flight, even if the caller disconnects while the platform owns it.
pub(crate) async fn write_text_async(text: Arc<String>) -> bool {
    static WRITE_GATE: Semaphore = Semaphore::const_new(1);
    let Ok(permit) = WRITE_GATE.acquire().await else {
        return false;
    };
    let (reply, result) = oneshot::channel();
    if let Err(error) = std::thread::Builder::new()
        .name("oplus-clipboard-write".into())
        .spawn(move || {
            let ok = write_text(&text);
            if ok && let Some(reader) = READER.as_ref() {
                reader.state.lock().unwrap().revision += 1;
            }
            drop(permit);
            let _ = reply.send(ok);
        })
    {
        warn!("cannot start clipboard write worker: {error}");
        return false;
    }
    result.await.unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn slow_read_delivers_one_result_to_both_callers() {
        let (release, blocked) = mpsc::channel();
        let calls = Arc::new(AtomicUsize::new(0));
        let worker = ReadWorker::new({
            let calls = Arc::clone(&calls);
            move || match calls.fetch_add(1, Ordering::Relaxed) {
                0 => blocked.recv().ok().map(|()| "late text".into()),
                _ => Some("fresh text".into()),
            }
        })
        .unwrap();

        let mut first = worker.request(Duration::ZERO).unwrap();
        // Cancel the waiter after the warning deadline, like a socket event.
        assert!(
            tokio::time::timeout(Duration::from_millis(20), first.finish())
                .await
                .is_err()
        );
        let mut second = worker.request(Duration::ZERO).unwrap();
        release.send(()).unwrap();
        for request in [&mut first, &mut second] {
            let text = tokio::time::timeout(Duration::from_secs(2), request.finish())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(text.as_str(), "late text");
        }
        let text = tokio::time::timeout(
            Duration::from_secs(2),
            worker.request(Duration::ZERO).unwrap().finish(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(text.as_str(), "fresh text");
    }

    #[test]
    fn runtime_drops_while_clipboard_backend_is_blocked() {
        let (release, blocked) = mpsc::channel();
        let (started, running) = mpsc::channel();
        let (stopped, done) = mpsc::channel();
        let runtime_thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .unwrap();
            let worker = ReadWorker::new(move || {
                started.send(()).unwrap();
                blocked.recv().ok();
                None
            })
            .unwrap();
            runtime.block_on(async {
                let mut request = worker.request(Duration::ZERO).unwrap();
                tokio::spawn(async move { request.finish().await });
            });
            running.recv_timeout(Duration::from_secs(2)).unwrap();
            drop(worker);
            drop(runtime);
            stopped.send(()).unwrap();
        });
        let stopped_while_blocked = done.recv_timeout(Duration::from_secs(2));
        release.send(()).unwrap();
        runtime_thread.join().unwrap();
        stopped_while_blocked.expect("runtime must shut down independently of clipboard I/O");
    }
}
