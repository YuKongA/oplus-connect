//! Track the compositor's current logical size for the portal-selected output.
use std::{collections::HashMap, os::fd::AsFd, time::Duration};
use tokio::sync::watch;
use wayland_client::{
    Connection, Dispatch, Proxy, QueueHandle,
    backend::WaylandError,
    protocol::{wl_output, wl_registry},
};
use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1};

struct Output {
    proxy: wl_output::WlOutput,
    xdg: Option<zxdg_output_v1::ZxdgOutputV1>,
    name: Option<String>,
    size: Option<(u32, u32)>,
}
#[derive(Default)]
struct State {
    outputs: HashMap<u32, Output>,
    manager: Option<(u32, zxdg_output_manager_v1::ZxdgOutputManagerV1)>,
}
impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } if interface == "wl_output" => {
                let output =
                    registry.bind::<wl_output::WlOutput, _, _>(name, version.min(4), qh, ());
                let xdg = state
                    .manager
                    .as_ref()
                    .map(|(_, manager)| manager.get_xdg_output(&output, qh, name));
                state.outputs.insert(
                    name,
                    Output {
                        proxy: output,
                        xdg,
                        name: None,
                        size: None,
                    },
                );
            }
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } if interface == "zxdg_output_manager_v1" => {
                let manager: zxdg_output_manager_v1::ZxdgOutputManagerV1 =
                    registry.bind(name, version.min(3), qh, ());
                for (id, output) in &mut state.outputs {
                    if output.xdg.is_none() {
                        output.xdg = Some(manager.get_xdg_output(&output.proxy, qh, *id));
                    }
                }
                state.manager = Some((name, manager));
            }
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(output) = state.outputs.remove(&name) {
                    if let Some(xdg) = output.xdg {
                        xdg.destroy();
                    }
                    if output.proxy.version() >= 3 {
                        output.proxy.release();
                    }
                }
                if state.manager.as_ref().is_some_and(|(id, _)| *id == name)
                    && let Some((_, manager)) = state.manager.take()
                {
                    manager.destroy();
                }
            }
            _ => {}
        }
    }
}
impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(
        _: &mut Self,
        _: &wl_output::WlOutput,
        _: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<zxdg_output_manager_v1::ZxdgOutputManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &zxdg_output_manager_v1::ZxdgOutputManagerV1,
        _: zxdg_output_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}
impl Dispatch<zxdg_output_v1::ZxdgOutputV1, u32> for State {
    fn event(
        state: &mut Self,
        _: &zxdg_output_v1::ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        id: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let Some(output) = state.outputs.get_mut(id) {
            match event {
                zxdg_output_v1::Event::Name { name } => output.name = Some(name),
                zxdg_output_v1::Event::LogicalSize { width, height } => {
                    output.size = u32::try_from(width)
                        .ok()
                        .zip(u32::try_from(height).ok())
                        .filter(|(w, h)| *w > 0 && *h > 0)
                }
                _ => {}
            }
        }
    }
}

async fn resolution_timeout(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

pub fn track(name: String) -> watch::Receiver<Option<(u32, u32)>> {
    let (tx, rx) = watch::channel(None);
    std::thread::spawn(move || {
        let result = (|| -> anyhow::Result<()> {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_io()
                .enable_time()
                .build()?;
            runtime.block_on(async {
                let conn = Connection::connect_to_env()?;
                // The owned duplicate remains open and unchanged until AsyncFd drops.
                let fd = unsafe { tokio::io::unix::AsyncFd::register(conn.as_fd().try_clone_to_owned()?)? };
                let mut queue = conn.new_event_queue();
                let qh = queue.handle();
                let _registry = conn.display().get_registry(&qh, ());
                let mut state = State::default();
                let mut deadline = None;
                loop {
                    queue.dispatch_pending(&mut state)?;
                    let size = state
                        .outputs
                        .values()
                        .find(|out| out.name.as_deref() == Some(name.as_str()))
                        .and_then(|out| out.size);
                    if size.is_some() {
                        deadline = None;
                    } else {
                        deadline.get_or_insert_with(|| tokio::time::Instant::now() + Duration::from_secs(10));
                    }
                    tx.send_if_modified(|current| {
                        if *current != size {
                            tracing::info!(output = %name, ?size, "Virtual output logical size changed");
                            *current = size;
                            true
                        } else {
                            false
                        }
                    });
                    loop {
                        match queue.flush() {
                            Ok(()) => break,
                            Err(WaylandError::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                tokio::select! {
                                    _ = tx.closed() => return Ok(()),
                                    _ = resolution_timeout(deadline) => anyhow::bail!("Timed out resolving logical size for output {name}"),
                                    ready = fd.writable() => ready?.clear_ready(),
                                }
                            }
                            Err(error) => return Err(error.into()),
                        }
                    }
                    let Some(guard) = queue.prepare_read() else { continue };
                    tokio::select! {
                        _ = tx.closed() => return Ok(()),
                        _ = resolution_timeout(deadline) => anyhow::bail!("Timed out resolving logical size for output {name}"),
                        ready = fd.readable() => {
                            ready?.clear_ready();
                            match guard.read() {
                                Ok(_) => {},
                                Err(WaylandError::Io(error)) if error.kind() == std::io::ErrorKind::WouldBlock => {},
                                Err(error) => return Err(error.into()),
                            }
                        }
                    }
                }
            })
        })();
        if let Err(error) = result {
            tx.send_replace(None);
            tracing::warn!(%error, "Failed to track virtual output size");
        }
    });
    rx
}
