use crate::RootView;
use crate::client::DaemonClient;
use crate::models::ConnectionRequest;
use crate::state::PendingApproval;
use crate::theme::Theme;
use gpui_kit::base::{Dialog, DialogPopup};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::{Disableable, WindowExt};
use gpui_kit::*;
use std::rc::Rc;

#[derive(Default)]
pub struct ConfirmModal {
    shown: Option<ShownApproval>,
}

struct ShownApproval {
    window_id: WindowId,
    request: Rc<ConnectionRequest>,
    focus: FocusHandle,
    restore: Option<WeakFocusHandle>,
}

#[derive(Clone, Copy)]
enum Decision {
    Accept,
    Reject,
    Dismiss,
}

impl ConfirmModal {
    // Mount our own dialog from state. WindowExt::close_dialog can only pop the
    // topmost dialog, which may belong to an unrelated action.
    pub(crate) fn render(
        &mut self,
        pending: Option<&PendingApproval>,
        window: &mut Window,
        cx: &mut Context<RootView>,
    ) -> Option<AnyElement> {
        let id = window.window_handle().window_id();
        let visible = pending.filter(|_| !window.has_active_dialog(cx));
        if let Some(shown) = &self.shown
            && (shown.window_id != id
                || visible.is_none_or(|p| !Rc::ptr_eq(&p.request, &shown.request)))
        {
            if shown.window_id == id && shown.focus.contains_focused(window, cx) {
                if let Some(previous) = shown.restore.as_ref().and_then(WeakFocusHandle::upgrade) {
                    previous.focus(window, cx);
                } else {
                    window.blur(cx);
                }
            }
            self.shown = None;
        }
        let pending = visible?;
        let req = &pending.request;
        let shown = self.shown.get_or_insert_with(|| {
            let restore = window.focused(cx).map(|focus| focus.downgrade());
            let focus = cx.focus_handle();
            focus.focus(window, cx);
            ShownApproval {
                window_id: id,
                request: req.clone(),
                focus,
                restore,
            }
        });

        let respond = |decision| {
            let request = req.clone();
            cx.listener(move |view, _: &ClickEvent, _, cx| {
                Self::respond(view, &request, decision, cx);
            })
        };
        let viewport = window.viewport_size();
        let popup = DialogPopup::new()
            .w(px(420.).min(viewport.width))
            .max_h(viewport.height)
            .rounded_lg()
            .bg(Theme::bg_modal())
            .border_1()
            .border_color(Theme::border_card())
            .child(
                div()
                    .id("approval-content")
                    .max_h(viewport.height)
                    .overflow_y_scroll()
                    .p_5()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_lg()
                                    .text_color(Theme::text_primary())
                                    .child("设备请求连接"),
                            )
                            .child(
                                Button::new("dismiss-connection")
                                    .ghost()
                                    .label("关闭")
                                    .disabled(pending.responding)
                                    .on_click(respond(Decision::Dismiss)),
                            ),
                    )
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
                    .child(Self::row(
                        "地址",
                        req.device_lan_address
                            .as_deref()
                            .unwrap_or(&req.device_address),
                    ))
                    .child(Self::row("识别码", &req.connection_id))
                    .child(
                        div()
                            .text_sm()
                            .text_color(Theme::text_secondary())
                            .child("确认后将同步剪贴板和通知。"),
                    )
                    .child(
                        div()
                            .flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("reject-connection")
                                    .label("拒绝")
                                    .disabled(pending.responding)
                                    .on_click(respond(Decision::Reject)),
                            )
                            .child(
                                Button::new("accept-connection")
                                    .primary()
                                    .label(if pending.responding {
                                        "处理中…"
                                    } else {
                                        "允许"
                                    })
                                    .disabled(pending.responding)
                                    .on_click(respond(Decision::Accept)),
                            ),
                    ),
            );
        let dialog = Dialog::new(cx)
            .accessibility_label("设备请求连接")
            .focus_handle(shown.focus.clone())
            .close_on_backdrop_press(false)
            .close_on_escape(false)
            .backdrop(div().size_full().occlude().bg(black().opacity(0.4)))
            .popup(popup);
        Some(
            div()
                .id(("approval", Rc::as_ptr(req) as usize))
                .absolute()
                .child(dialog)
                .into_any_element(),
        )
    }

    fn respond(
        view: &mut RootView,
        request: &Rc<ConnectionRequest>,
        decision: Decision,
        cx: &mut Context<RootView>,
    ) {
        if !view.state.begin_approval_response(request) {
            return;
        }
        if matches!(decision, Decision::Dismiss) {
            view.state.finish_approval(request);
        }
        cx.notify();
        let request = request.clone();
        cx.spawn(async move |this, app| {
            let client = DaemonClient::new();
            let result = match decision {
                Decision::Accept => {
                    client
                        .accept_connection(&request.device_address, &request.connection_id)
                        .await
                }
                Decision::Reject | Decision::Dismiss => {
                    client
                        .reject_connection(&request.device_address, &request.connection_id)
                        .await
                }
            };
            if matches!(decision, Decision::Dismiss) {
                return;
            }
            let _ = this.update(app, |view, cx| {
                if !view.state.finish_approval(&request) {
                    return;
                }
                match result {
                    Ok(true) if matches!(decision, Decision::Accept) => {
                        view.state.set_toast("已允许连接")
                    }
                    Ok(true) => view.state.set_toast("已拒绝连接"),
                    Ok(false) => view.state.set_toast("请求已失效"),
                    Err(error) => view.state.set_toast(format!("处理失败：{error}")),
                }
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
