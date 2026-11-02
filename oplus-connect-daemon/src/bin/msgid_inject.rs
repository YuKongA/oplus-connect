//! Headless Windows harness for live OAF/DBRouter experiments.
//!
//! This intentionally excludes the Unix-socket IPC and desktop integrations.
//! It owns the official control/data ports, so stop the official client before
//! starting it.
//!
//! Examples:
//!   cargo run --bin msgid_inject -- --send "PC-HARNESS-CLIP-1"
//!   cargo run --bin msgid_inject -- --wait-secs 60

use oplus_connect_daemon::{discovery, panta, platform, probe, state};
use tokio::{net::TcpListener, task::JoinSet};

use std::net::Ipv4Addr;
use tracing::{Level, info};
use tracing_subscriber::FmtSubscriber;

struct HarnessArgs {
    csp: u16,
    wait_secs: Option<u64>,
    send: Option<String>,
    send_delay: Option<String>,
    session: Option<u16>,
    pdid: Option<String>,
    log_dir: Option<String>,
    allow_unconfirmed: bool,
}

fn parse_args() -> Result<HarnessArgs, String> {
    let mut csp = std::env::var("OPLUS_CSP")
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(10155);
    let mut wait_secs = None;
    let mut send = None;
    let mut send_delay = None;
    let mut session = None;
    let mut pdid = None;
    let mut log_dir = None;
    let mut allow_unconfirmed = false;
    let mut args = std::env::args().skip(1);

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--send" => {
                send = Some(args.next().ok_or("--send requires a value")?);
            }
            "--send-delay" => {
                send_delay = Some(args.next().ok_or("--send-delay requires seconds")?);
            }
            "--session" => {
                let value = args.next().ok_or("--session requires a number")?;
                let parsed = value
                    .parse::<u16>()
                    .map_err(|_| format!("invalid session: {value}"))?;
                if !(5..1024).contains(&parsed) {
                    return Err("session must be in 5..=1023".to_string());
                }
                session = Some(parsed);
            }
            "--csp" => {
                let value = args.next().ok_or("--csp requires a port")?;
                csp = value
                    .parse::<u16>()
                    .map_err(|_| format!("invalid csp port: {value}"))?;
            }
            "--pdid" => {
                pdid = Some(args.next().ok_or("--pdid requires a hex device id")?);
            }
            "--log-dir" => {
                log_dir = Some(args.next().ok_or("--log-dir requires a path")?);
            }
            "--allow-unconfirmed" => {
                allow_unconfirmed = true;
            }
            "--wait-secs" => {
                let value = args.next().ok_or("--wait-secs requires seconds")?;
                wait_secs = Some(
                    value
                        .parse::<u64>()
                        .map_err(|_| format!("invalid wait seconds: {value}"))?,
                );
            }
            "--help" | "-h" => {
                println!(
                    "usage: msgid_inject [--send TEXT] [--send-delay SECS] \
                     [--session 5..1023] [--csp PORT] [--pdid HEX] \
                     [--log-dir DIR] [--allow-unconfirmed] [--wait-secs SECS]"
                );
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }

    Ok(HarnessArgs {
        csp,
        wait_secs,
        send,
        send_delay,
        session,
        pdid,
        log_dir,
        allow_unconfirmed,
    })
}

fn random_session() -> u16 {
    panta::allocate_session_id()
}

fn set_var(name: &str, value: impl AsRef<std::ffi::OsStr>) {
    // SAFETY: the harness applies its configuration before creating the Tokio
    // runtime, so no other process thread can concurrently read the environment.
    unsafe { std::env::set_var(name, value) };
}

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args = parse_args()?;
    let session = args.session.unwrap_or_else(random_session);
    set_var("OPLUS_AUTO_ACCEPT", "1");
    set_var("OPLUS_BUS_SESSION", session.to_string());
    if let Some(value) = &args.send {
        set_var("OPLUS_CLIPBOARD_INJECT", value);
    }
    if let Some(value) = &args.send_delay {
        set_var("OPLUS_CLIPBOARD_INJECT_DELAY_SECS", value);
    }
    if let Some(value) = &args.pdid {
        set_var("OPLUS_PDID", value);
    }
    if std::env::var_os("OPLUS_PDID").is_none() {
        set_var("OPLUS_PDID", "e1c0dde98ace");
    }
    if std::env::var_os("OPLUS_DN").is_none() {
        set_var(
            "OPLUS_DN",
            std::env::var("COMPUTERNAME").unwrap_or_else(|_| "PC_LINUX".to_string()),
        );
    }
    if let Some(value) = &args.log_dir {
        set_var("OPLUS_PROTOCOL_LOG_DIR", value);
        std::fs::create_dir_all(value)?;
    }
    if args.allow_unconfirmed {
        set_var("OPLUS_CLIPBOARD_ALLOW_UNCONFIRMED", "1");
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run(args))
}

async fn run(args: HarnessArgs) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::DEBUG)
        .with_target(false)
        .finish();
    tracing::subscriber::set_global_default(subscriber)?;

    let state = state::AppState::new();
    let csp = args.csp;
    if let Some(clipboard) = platform::clipboard::read_text() {
        info!("msgid-inject: current clipboard ({}B)", clipboard.len());
    } else {
        info!("msgid-inject: clipboard has no readable text");
    }
    info!(
        "msgid-inject: csp={} session={} auto_accept=1",
        csp,
        std::env::var("OPLUS_BUS_SESSION").unwrap_or_else(|_| "<unset>".to_string())
    );

    let mut control_ports = vec![10150u16, 10152, 10155];
    if !control_ports.contains(&csp) {
        control_ports.push(csp);
    }
    probe::initialize_ksc()?;
    let mut control = Vec::new();
    let mut data = Vec::new();
    for port in control_ports {
        control.push(TcpListener::bind(("0.0.0.0", port)).await?);
    }
    for port in [46888u16, 46999, 47888, 47999, 48999, 48888] {
        data.push(TcpListener::bind(("0.0.0.0", port)).await?);
    }
    let announcer = discovery::Announcer::new(csp)?;
    let mut servers = JoinSet::new();
    for listener in control {
        let state = state.clone();
        servers.spawn(probe::serve_port_probe(listener, state));
    }
    for listener in data {
        servers.spawn(probe::serve_data_listener(listener));
    }
    let announcer = announcer.spawn(move || {
        state.devices.try_read().ok().and_then(|devices| {
            devices
                .values()
                .filter_map(|device| device.ip_address.as_deref())
                .find_map(|ip| ip.parse::<Ipv4Addr>().ok())
        })
    });
    info!("msgid-inject: ready");
    let result = if let Some(secs) = args.wait_secs {
        tokio::select! {
            result = oplus_connect_daemon::shutdown_signal() => result,
            _ = tokio::time::sleep(std::time::Duration::from_secs(secs)) => {
                info!("msgid-inject: wait elapsed, exiting");
                Ok(())
            }
        }
    } else {
        oplus_connect_daemon::shutdown_signal().await
    };
    servers.abort_all();
    while servers.join_next().await.is_some() {}
    announcer.shutdown().await?;
    result?;
    Ok(())
}
