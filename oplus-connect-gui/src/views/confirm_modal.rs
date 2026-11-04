use crate::client::DaemonClient;
use crate::models::ConnectionRequest;
use crate::theme::Theme;
use crate::RootView;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::{Disableable, WindowExt};
use gpui_kit::*;
use std::cell::Cell;
use std::rc::Rc;

pub struct ConfirmModal;

impl ConfirmModal {
    pub fn open(req: ConnectionRequest, window: &mut Window, cx: &mut Context<RootView>) {
        let owner = cx.entity().downgrade();
        let pending = Rc::new(Cell::new(false));
        window.open_dialog(cx, move |dialog, _, _| {
            let accept_owner = owner.clone();
            let reject_owner = owner.clone();
            let close_owner = owner.clone();
            let accept_req = req.clone();
            let reject_req = req.clone();
            let accept_pending = pending.clone();
            let reject_pending = pending.clone();
            dialog
                .centered()
                .title("设备请求连接")
                .close_button(true)
                .on_close(move |_, _, cx| {
                    let _ = close_owner.update(cx, |view, cx| {
                        let Some(request) = view.state.connection_request.take() else {
                            return;
                        };
                        cx.spawn(async move |_, app| {
                            let address = request.device_address.clone();
                            let connection_id = request.connection_id.clone();
                            app.background_executor()
                                .spawn(async move {
                                    let client = DaemonClient::new();
                                    let _ =
                                        client.reject_connection(&address, &connection_id).await;
                                })
                                .await;
                        })
                        .detach();
                        cx.notify();
                    });
                })
                .overlay_closable(false)
                .keyboard(false)
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .gap(px(12.0))
                        .child(Self::row("名称", &req.device_name))
                        .child(Self::row(
                            "类型",
                            match req.device_type.as_str() {
                                "tablet" => "平板",
                                "phone" => "手机",
                                _ => "未知设备",
                            },
                        ))
                        .child(Self::row(
                            "连接",
                            if req.connect_type == "usb" {
                                "USB"
                            } else {
                                "Wi-Fi"
                            },
                        ))
                        .child(Self::row("地址", &req.device_address))
                        .child(Self::row("识别码", &req.connection_id))
                        .child(
                            div()
                                .text_sm()
                                .text_color(Theme::text_secondary())
                                .child("确认后将同步剪贴板和通知。"),
                        ),
                )
                .footer(
                    div()
                        .flex()
                        .justify_end()
                        .gap(px(8.0))
                        .child(
                            Button::new("reject-connection")
                                .label("拒绝")
                                .disabled(pending.get())
                                .on_click(move |_, window, cx| {
                                    let _ = reject_owner.update(cx, |_, cx| {
                                        Self::respond(
                                            reject_req.clone(),
                                            false,
                                            reject_pending.clone(),
                                            window,
                                            cx,
                                        );
                                    });
                                }),
                        )
                        .child(
                            Button::new("accept-connection")
                                .primary()
                                .label(if pending.get() {
                                    "处理中…"
                                } else {
                                    "允许"
                                })
                                .disabled(pending.get())
                                .on_click(move |_, window, cx| {
                                    let _ = accept_owner.update(cx, |_, cx| {
                                        Self::respond(
                                            accept_req.clone(),
                                            true,
                                            accept_pending.clone(),
                                            window,
                                            cx,
                                        );
                                    });
                                }),
                        ),
                )
        });
    }

    fn respond(
        req: ConnectionRequest,
        accept: bool,
        pending: Rc<Cell<bool>>,
        window: &mut Window,
        cx: &mut Context<RootView>,
    ) {
        if pending.replace(true) {
            return;
        }
        window.refresh();
        cx.spawn_in(window, async move |this, app| {
            let address = req.device_address.clone();
            let connection_id = req.connection_id.clone();
            let result = app
                .background_executor()
                .spawn(async move {
                    let client = DaemonClient::new();
                    if accept {
                        client.accept_connection(&address, &connection_id).await
                    } else {
                        client.reject_connection(&address, &connection_id).await
                    }
                })
                .await;
            let _ = this.update_in(app, |view, window, cx| {
                pending.set(false);
                // A late response must not dismiss a newer connection request.
                if !view
                    .state
                    .connection_request
                    .as_ref()
                    .is_some_and(|current| {
                        current.device_address == req.device_address
                            && current.connection_id == req.connection_id
                    })
                {
                    return;
                }
                match result {
                    Ok(true) => {
                        view.state.connection_request = None;
                        window.close_dialog(cx);
                        window.push_notification(
                            if accept {
                                "已允许连接"
                            } else {
                                "已拒绝连接"
                            },
                            cx,
                        );
                    }
                    Ok(false) => {
                        view.state.connection_request = None;
                        window.close_dialog(cx);
                        window.push_notification("请求已失效", cx);
                    }
                    Err(error) => {
                        view.state.connection_request = None;
                        window.close_dialog(cx);
                        window.push_notification(format!("处理失败：{error}"), cx);
                    }
                }
                window.refresh();
                cx.notify();
            });
        })
        .detach();
    }

    fn row(label: &'static str, value: &str) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .child(
                div()
                    .text_xs()
                    .text_color(Theme::text_secondary())
                    .child(label),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(Theme::text_primary())
                    .child(value.to_owned()),
            )
    }
}
