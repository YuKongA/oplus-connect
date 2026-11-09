pub mod cast;
pub mod clipboard;
pub mod discovery;
pub mod expand_cast;
pub mod kms;
pub mod net_utils;
pub mod panta;
pub mod platform;
pub mod probe;
pub mod reverse;
pub mod scp;
pub mod state;

#[cfg(unix)]
pub mod file_share;
#[cfg(unix)]
pub mod ipc;

pub struct DaemonHandle {
    _shutdown_tx: tokio::sync::oneshot::Sender<()>,
}

/// Log level from `OPLUS_CONNECT_LOG` (`trace`, `debug`, `info`, `warn`, `error`,
/// `off`); unknown or missing values select `INFO`. The GUI and its embedded
/// daemon share one subscriber, so this knob covers both.
pub fn log_level_from_env() -> tracing_subscriber::filter::LevelFilter {
    use tracing_subscriber::filter::LevelFilter;
    match std::env::var("OPLUS_CONNECT_LOG")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "trace" => LevelFilter::TRACE,
        "debug" => LevelFilter::DEBUG,
        "warn" => LevelFilter::WARN,
        "error" => LevelFilter::ERROR,
        "off" => LevelFilter::OFF,
        _ => LevelFilter::INFO,
    }
}

/// Starts the OPLUS CONNECT daemon service in-process on the active Tokio runtime.
/// If an external daemon process is already running (instance lock held), returns Ok(None)
/// so the caller can attach via standard UDS IPC client.
pub async fn start_daemon_service(
    state: state::AppState,
) -> Result<Option<DaemonHandle>, Box<dyn std::error::Error + Send + Sync>> {
    #[cfg(unix)]
    {
        use tokio::{net::TcpListener, task::JoinSet};
        use tracing::info;

        let directory = match ipc::runtime::RuntimeDir::new() {
            Ok(dir) => dir,
            Err(err) => {
                info!(
                    "Daemon runtime directory already locked: {}. Attaching as client.",
                    err
                );
                return Ok(None);
            }
        };

        let event_hub = ipc::event_hub::EventHub::bind(state.clone(), &directory)?;
        let uds_server =
            ipc::uds_http::UdsHttpServer::bind(state.clone(), &directory, "oplus-dsp-ipc")?;
        let rc_server = ipc::uds_http::UdsHttpServer::bind(state.clone(), &directory, "dsprc-ipc")?;

        let mut control = Vec::new();
        let mut data = Vec::new();
        if std::env::var("OPLUS_NO_PROBE").as_deref() != Ok("1") {
            let _ = probe::initialize_ksc();
            for port in [10150u16, 10152, 10155] {
                if let Ok(l) = TcpListener::bind(("0.0.0.0", port)).await {
                    control.push(l);
                }
            }
            for port in [46888u16, 46999, 47888, 47999, 48999, 48888] {
                if let Ok(l) = TcpListener::bind(("0.0.0.0", port)).await {
                    data.push(l);
                }
            }
        }

        let announcer = discovery::Announcer::new(10150)?;
        let mut servers = JoinSet::new();
        servers.spawn(event_hub.run());
        servers.spawn(uds_server.run());
        servers.spawn(rc_server.run());

        for listener in control {
            let s = state.clone();
            servers.spawn(async move {
                probe::serve_port_probe(listener, s).await;
                Ok(())
            });
        }
        for listener in data {
            let s = state.clone();
            servers.spawn(async move {
                probe::serve_data_listener_with_state(listener, s).await;
                Ok(())
            });
        }

        let peer_state = state.clone();
        let announcer_worker = announcer.spawn(move || {
            if let Ok(devices) = peer_state.devices.try_read() {
                if let Some(ip) = devices
                    .values()
                    .filter_map(|device| device.ip_address.as_deref())
                    .find_map(|ip| ip.parse().ok())
                {
                    return Some(ip);
                }
            }
            None
        });

        servers.spawn(async move {
            clipboard::ClipboardSync::new(state).run().await;
            Ok(())
        });
        info!("OPLUS CONNECT in-process daemon service initialized successfully.");

        let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let mut servers = servers;
            tokio::select! {
                _ = &mut shutdown_rx => {}
                Some(res) = servers.join_next() => {
                    tracing::warn!("In-process daemon server task exited: {:?}", res);
                }
            }
            servers.abort_all();
            while servers.join_next().await.is_some() {}
            let _ = announcer_worker.shutdown().await;
        });

        Ok(Some(DaemonHandle {
            _shutdown_tx: shutdown_tx,
        }))
    }
    #[cfg(not(unix))]
    {
        Ok(None)
    }
}

/// Waits for Ctrl+C, or the service manager's SIGTERM on Unix.
pub async fn shutdown_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = signal(SignalKind::terminate())?;
        let mut interrupt = signal(SignalKind::interrupt())?;
        tokio::select! {
            _ = terminate.recv() => {},
            _ = interrupt.recv() => {},
        }
        Ok(())
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await
}
