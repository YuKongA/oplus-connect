use crate::ipc::runtime::{BoundSocket, RuntimeDir};
use crate::state::AppState;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::broadcast;
use tokio::task::JoinSet;
use tracing::{info, warn};

const MAX_SUBSCRIBERS: usize = 16;

pub struct EventHub {
    listeners: Vec<(BoundSocket, broadcast::Receiver<String>)>,
}

impl EventHub {
    pub fn bind(state: AppState, directory: &Arc<RuntimeDir>) -> std::io::Result<Self> {
        let mut listeners = Vec::with_capacity(3);
        for name in ["dsp-ipc-event", "dsprc-ipc-event", "ex-ipc-event"] {
            let socket = directory.bind(name)?;
            listeners.push((socket, state.event_sender.subscribe()));
        }
        Ok(Self { listeners })
    }

    pub async fn run(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut workers = JoinSet::new();
        for (socket, receiver) in self.listeners {
            workers.spawn(run_event_listener(socket, receiver));
        }
        match workers.join_next().await {
            Some(result) => result?,
            None => Ok(()),
        }
    }
}

async fn run_event_listener(
    socket: BoundSocket,
    rx: broadcast::Receiver<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut clients = JoinSet::new();
    loop {
        tokio::select! {
            accepted = socket.listener.accept() => {
                let (stream, _) = accepted?;
                while let Some(result) = clients.try_join_next() {
                    if let Err(error) = result {
                        warn!("Event subscriber task failed: {}", error);
                    }
                }
                if let Err(error) = crate::ipc::runtime::verify_peer(&stream) {
                    warn!("Rejected event subscriber: {}", error);
                    continue;
                }
                if clients.len() >= MAX_SUBSCRIBERS {
                    warn!(
                        "Rejected event subscriber: {} already connected",
                        clients.len()
                    );
                    continue;
                }
                let mut client_rx = rx.resubscribe();
                clients.spawn(async move {
                    if let Err(error) = stream_events_to_client(stream, &mut client_rx).await {
                        warn!("Subscriber stream disconnected: {}", error);
                    }
                });
            }
            Some(result) = clients.join_next(), if !clients.is_empty() => {
                if let Err(error) = result {
                    warn!("Event subscriber task failed: {}", error);
                }
            }
        }
    }
}

async fn stream_events_to_client(
    mut stream: UnixStream,
    rx: &mut broadcast::Receiver<String>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Optional client handshake/subscription line.
    let mut init_buf = [0u8; 1024];
    tokio::select! {
        read_res = stream.read(&mut init_buf) => {
            if let Ok(n) = read_res
                && n > 0 {
                    let msg = String::from_utf8_lossy(&init_buf[..n]);
                    info!("Event subscriber handshake: {}", msg.trim());
                }
        }
        _ = tokio::time::sleep(tokio::time::Duration::from_millis(50)) => {}
    }

    loop {
        match rx.recv().await {
            Ok(event_str) => {
                let line = format!("{}\n", event_str.trim());
                if let Err(e) = stream.write_all(line.as_bytes()).await {
                    return Err(Box::new(e));
                }
                let _ = stream.flush().await;
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                warn!("Subscriber lagged by {} events", skipped);
            }
            Err(broadcast::error::RecvError::Closed) => {
                break;
            }
        }
    }

    Ok(())
}
