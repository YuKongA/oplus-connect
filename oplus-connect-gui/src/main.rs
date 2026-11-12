use gpui_kit::*;
use oplus_connect_gui::assets::LocalAssets;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use tracing::{info, warn};

fn main() {
    tracing_subscriber::fmt()
        .with_max_level(oplus_connect_daemon::log_level_from_env())
        .init();
    info!("Starting OPLUS CONNECT GUI (GPUI Kit)");

    gpui_kit::application()
        .with_quit_mode(QuitMode::Explicit)
        .with_assets(LocalAssets::new())
        .run(|cx: &mut App| {
            gpui_kit::init(cx);
            oplus_connect_gui::theme::Theme::init(cx);

            let (tray_tx, mut tray_rx) = tokio::sync::mpsc::unbounded_channel();
            oplus_connect_gui::tray::start(tray_tx);

            let main_window_id = Rc::new(Cell::new(None));
            // Sharing sessions belong to the root entity, not the visible window.
            let root_holder = Rc::new(RefCell::new(None));
            let tray_window_id = main_window_id.clone();
            let tray_root_holder = root_holder.clone();
            cx.spawn(async move |app| {
                while let Some(command) = tray_rx.recv().await {
                    match command {
                        oplus_connect_gui::tray::TrayCommand::ToggleMainWindow => {
                            let existing = app.update(|cx| {
                                let window_id = tray_window_id.get();
                                cx.windows()
                                    .into_iter()
                                    .find(|window| Some(window.window_id()) == window_id)
                            });
                            if let Some(window) = existing {
                                let visible = app
                                    .update(|cx| {
                                        window.update(cx, |_, window, _| {
                                            if window.is_visible() {
                                                window.remove_window();
                                                true
                                            } else {
                                                window.activate_window();
                                                false
                                            }
                                        })
                                    })
                                    .unwrap_or(false);
                                if !visible {
                                    app.update(|cx| cx.activate(true));
                                }
                            } else if let Err(error) = app.update(|cx| {
                                oplus_connect_gui::open_main_window(
                                    cx,
                                    tray_window_id.clone(),
                                    tray_root_holder.clone(),
                                )
                            }) {
                                warn!("cannot reopen main window: {error:#}");
                            } else {
                                app.update(|cx| cx.activate(true));
                            }
                        }
                        oplus_connect_gui::tray::TrayCommand::ShowMainWindow => {
                            let is_open = app.update(|cx| {
                                let window_id = tray_window_id.get();
                                window_id.is_some_and(|window_id| {
                                    cx.windows()
                                        .iter()
                                        .any(|window| window.window_id() == window_id)
                                })
                            });
                            if !is_open
                                && let Err(error) = app.update(|cx| {
                                    oplus_connect_gui::open_main_window(
                                        cx,
                                        tray_window_id.clone(),
                                        tray_root_holder.clone(),
                                    )
                                })
                            {
                                warn!("cannot reopen main window: {error:#}");
                            }
                            app.update(|cx| cx.activate(true));
                        }
                        oplus_connect_gui::tray::TrayCommand::Quit => {
                            app.update(|cx| cx.quit());
                            break;
                        }
                    }
                }
            })
            .detach();

            oplus_connect_gui::open_main_window(cx, main_window_id, root_holder)
                .expect("Failed to open main window");
            cx.activate(true);
        });

    info!("OPLUS CONNECT GUI exiting, shutting down in-process daemon...");
    oplus_connect_gui::shutdown_in_process_daemon();
}
