use gpui_kit::*;
use oplus_connect_gui::RootView;
use oplus_connect_gui::assets::LocalAssets;
use tracing::info;

fn main() {
    tracing_subscriber::fmt::init();
    info!("Starting OPLUS CONNECT GUI (GPUI Kit)");

    gpui_kit::application()
        .with_assets(LocalAssets::new())
        .run(|cx: &mut App| {
            gpui_kit::init(cx);
            oplus_connect_gui::theme::Theme::init(cx);

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

            gpui_kit::open_window(window_options, cx, |_, cx| cx.new(|cx| RootView::new(cx)))
                .expect("Failed to open main window");

            cx.activate(true);
        });

    info!("OPLUS CONNECT GUI exiting, shutting down in-process daemon...");
    oplus_connect_gui::shutdown_in_process_daemon();
}
