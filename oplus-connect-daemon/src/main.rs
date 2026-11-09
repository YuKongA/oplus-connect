#[cfg(unix)]
use oplus_connect_daemon::ipc::{
    event_hub::EventHub, runtime::RuntimeDir, uds_http::UdsHttpServer,
};
use oplus_connect_daemon::{clipboard::ClipboardSync, discovery, probe, state::AppState};
use tokio::{net::TcpListener, task::JoinSet};
use tracing::info;
use tracing_subscriber::FmtSubscriber;

type Error = Box<dyn std::error::Error + Send + Sync>;

#[tokio::main]
async fn main() -> Result<(), Error> {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(oplus_connect_daemon::log_level_from_env())
        .with_target(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    let state = AppState::new();
    #[cfg(unix)]
    let directory = RuntimeDir::new()?;
    #[cfg(unix)]
    let event_hub = EventHub::bind(state.clone(), &directory)?;
    #[cfg(unix)]
    let uds_server = UdsHttpServer::bind(state.clone(), &directory, "oplus-dsp-ipc")?;
    #[cfg(unix)]
    let rc_server = UdsHttpServer::bind(state.clone(), &directory, "dsprc-ipc")?;

    let mut control = Vec::new();
    let mut data = Vec::new();
    // OPLUS_NO_PROBE=1 leaves control/data ports to an external daemon.
    if std::env::var("OPLUS_NO_PROBE").as_deref() != Ok("1") {
        probe::initialize_ksc()?;
        for port in [10150u16, 10152, 10155] {
            control.push(TcpListener::bind(("0.0.0.0", port)).await?);
        }
        for port in [46888u16, 46999, 47888, 47999, 48999, 48888] {
            data.push(TcpListener::bind(("0.0.0.0", port)).await?);
        }
    }
    let announcer = discovery::Announcer::new(10150)?;
    let mut servers = JoinSet::<Result<(), Error>>::new();
    #[cfg(unix)]
    {
        servers.spawn(event_hub.run());
        servers.spawn(uds_server.run());
        servers.spawn(rc_server.run());
        info!("IPC runtime directory: {}", directory.path().display());
    }
    for listener in control {
        let state = state.clone();
        servers.spawn(async move {
            probe::serve_port_probe(listener, state).await;
            Ok(())
        });
    }
    for listener in data {
        let state = state.clone();
        servers.spawn(async move {
            probe::serve_data_listener_with_state(listener, state).await;
            Ok(())
        });
    }
    let peer_state = state.clone();
    let announcer = announcer.spawn(move || {
        peer_state.devices.try_read().ok().and_then(|devices| {
            devices
                .values()
                .filter_map(|device| device.ip_address.as_deref())
                .find_map(|ip| ip.parse().ok())
        })
    });
    servers.spawn(async move {
        ClipboardSync::new(state).run().await;
        Ok(())
    });
    info!("OPLUS CONNECT daemon is ready and listening.");

    let result = tokio::select! {
        result = oplus_connect_daemon::shutdown_signal() => result.map_err(Error::from),
        Some(result) = servers.join_next(), if !servers.is_empty() => {
            match result {
                Ok(Ok(())) => Err(std::io::Error::other("Server stopped unexpectedly").into()),
                Ok(Err(error)) => Err(error),
                Err(error) => Err(error.into()),
            }
        }
    };
    info!("Shutting down OPLUS CONNECT daemon...");
    servers.abort_all();
    while servers.join_next().await.is_some() {}
    let stopped = announcer.shutdown().await.map_err(Error::from);
    info!("Cleanup complete.");
    result.and(stopped)
}
