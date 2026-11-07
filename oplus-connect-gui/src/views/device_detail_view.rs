use crate::client::DaemonClient;
use crate::expand_cast_session::{ExpandCastPhase, ExpandCastViewState};
use crate::kms_session::{KmsPhase, KmsViewState};
use crate::models::*;
use crate::state::AppState;
use crate::theme::Theme;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::Disableable;
use gpui_kit::component::Icon;
use gpui_kit::*;

pub struct DeviceDetailView;

impl DeviceDetailView {
    pub fn render(
        state: &AppState,
        kms: &KmsViewState,
        expand_cast: &ExpandCastViewState,
        _window: &mut Window,
        cx: &mut Context<crate::RootView>,
    ) -> impl IntoElement {
        let device_count = state.known_devices.len();
        let connected_count = state
            .known_devices
            .iter()
            .filter(|device| device.is_connected)
            .count();
        div()
            .id("device-detail-scroll")
            .flex_1()
            .h_full()
            .overflow_y_scroll()
            .p(px(20.0))
            .flex()
            .flex_col()
            .gap(px(16.0))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_xl()
                            .font_weight(FontWeight::BOLD)
                            .text_color(Theme::text_primary())
                            .child("设备详情"),
                    )
                    .child(div().text_xs().text_color(Theme::text_muted()).child(
                        if device_count == 0 {
                            "当前无已连接设备".to_string()
                        } else {
                            format!("已连接 {connected_count} 台 · 已保存 {device_count} 台")
                        },
                    )),
            )
            .child(if state.known_devices.is_empty() {
                Self::render_no_device_view().into_any_element()
            } else {
                Self::render_device_list(state, kms, expand_cast, cx).into_any_element()
            })
    }

    fn render_device_list(
        state: &AppState,
        kms: &KmsViewState,
        expand_cast: &ExpandCastViewState,
        cx: &mut Context<crate::RootView>,
    ) -> impl IntoElement {
        let mut list = div().flex().flex_col().gap(px(16.0));
        for (index, device) in state.known_devices.iter().enumerate() {
            list = list.child(Self::render_device_card(
                device,
                state,
                kms,
                expand_cast,
                cx,
                index,
            ));
        }
        list
    }

    fn render_device_card(
        device: &DeviceInfo,
        state: &AppState,
        kms: &KmsViewState,
        expand_cast: &ExpandCastViewState,
        cx: &mut Context<crate::RootView>,
        index: usize,
    ) -> impl IntoElement {
        let is_connected = device.is_connected;
        let ip_display = device
            .ip_address
            .as_deref()
            .filter(|address| !address.is_empty())
            .map(|address| format!("{address}:10150"))
            .unwrap_or_else(|| "未提供".to_string());
        let link = if device.connect_type == "usb" {
            "USB"
        } else {
            "Wi-Fi"
        };
        let meta = format!("{link} · {ip_display}");
        let status_label = if is_connected {
            "已连接"
        } else {
            "未连接"
        };
        let status_color = if is_connected {
            Theme::primary()
        } else {
            Theme::text_muted()
        };
        let icon_color = if is_connected {
            Theme::primary()
        } else {
            Theme::text_muted()
        };
        let status_bg = if is_connected {
            Theme::primary_subtle()
        } else {
            Theme::bg_header()
        };

        let mut card = div()
            .p(px(16.0))
            .rounded_xl()
            .bg(Theme::bg_card())
            .border_1()
            .border_color(Theme::border_card())
            .flex()
            .items_center()
            .justify_between()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(16.0))
                    .min_w_0()
                    .child(
                        Icon::new(IconName::Smartphone)
                            .size(px(40.0))
                            .text_color(icon_color)
                            .flex_shrink_0(),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(4.0))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(8.0))
                                    .child(
                                        div()
                                            .text_lg()
                                            .font_weight(FontWeight::BOLD)
                                            .text_color(Theme::text_primary())
                                            .child(device.device_name.clone()),
                                    )
                                    .child(
                                        div()
                                            .px(px(8.0))
                                            .py(px(2.0))
                                            .rounded_full()
                                            .bg(status_bg)
                                            .text_xs()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(status_color)
                                            .child(status_label),
                                    ),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(Theme::text_secondary())
                                    .child(meta),
                            ),
                    ),
            );

        if is_connected {
            let device_id = device.device_id.clone();
            let mut actions = div().flex().items_center().gap(px(8.0));
            if device.device_type == "phone" {
                let casting = state.cast.device_id == device.device_id && state.cast.active();
                let cast_label = if casting {
                    match state.cast.phase {
                        CastPhase::Stopping => "停止中…",
                        CastPhase::Displaying => "停止投屏",
                        _ => "投屏启动中",
                    }
                } else {
                    "开始投屏"
                };
                let cast_id = device_id.clone();
                actions = actions.child(
                    Button::new(("btn-cast", index))
                        .primary()
                        .disabled(
                            (state.cast.active() && !casting)
                                || kms.active()
                                || expand_cast.active()
                                || state.cast.phase == CastPhase::Stopping,
                        )
                        .label(cast_label)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if this.state.cast.active() {
                                this.stop_cast(cx);
                            } else {
                                this.start_cast(cast_id.clone(), cx);
                            }
                            cx.notify();
                        })),
                );
            } else if device.device_type == "tablet" {
                let sharing = kms.device_id == device.device_id && kms.active();
                let label = if sharing {
                    match kms.phase {
                        KmsPhase::Stopping => "停止中…",
                        KmsPhase::Armed | KmsPhase::Sharing => "停止键鼠共享",
                        _ => "共享启动中",
                    }
                } else {
                    "键鼠共享"
                };
                let kms_id = device_id.clone();
                actions = actions.child(
                    Button::new(("btn-kms", index))
                        .primary()
                        .disabled(
                            (kms.active() && !sharing)
                                || kms.phase == KmsPhase::Stopping
                                || state.cast.active()
                                || expand_cast.active(),
                        )
                        .label(label)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if this.kms.active() {
                                this.stop_kms(cx);
                            } else {
                                this.start_kms(kms_id.clone(), cx);
                            }
                        })),
                );

                let ext_active = expand_cast.device_id == device.device_id && expand_cast.active();
                let ext_label = if ext_active {
                    match expand_cast.phase {
                        ExpandCastPhase::Stopping => "停止中…",
                        ExpandCastPhase::Streaming => "停止屏幕共享",
                        _ => "共享启动中",
                    }
                } else {
                    "屏幕共享"
                };
                let ext_id = device_id.clone();
                actions = actions.child(
                    Button::new(("btn-expand-cast", index))
                        .primary()
                        .disabled(
                            (expand_cast.active() && !ext_active)
                                || expand_cast.phase == ExpandCastPhase::Stopping
                                || state.cast.active()
                                || kms.active(),
                        )
                        .label(ext_label)
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if this.expand_cast.active() {
                                this.stop_expand_cast(cx);
                            } else {
                                this.start_expand_cast(ext_id.clone(), cx);
                            }
                        })),
                );
            }
            actions = actions.child(
                Button::new(("btn-disconnect", index))
                    .secondary()
                    .label("断开")
                    .on_click(cx.listener(move |this, _, _, cx| {
                        if this.state.cast.device_id == device_id {
                            this.stop_cast(cx);
                        }
                        if this.kms.device_id == device_id {
                            this.stop_kms(cx);
                        }
                        if this.expand_cast.device_id == device_id {
                            this.stop_expand_cast(cx);
                        }
                        let device_id = device_id.clone();
                        cx.spawn(async move |this, app| {
                            let client = DaemonClient::new();
                            let result = app
                                .background_executor()
                                .spawn(async move { client.disconnect_device(&device_id).await })
                                .await;
                            let _ = this.update(app, |view, cx| {
                                view.state.set_toast(if result.is_ok() {
                                    "正在断开…"
                                } else {
                                    "断开失败"
                                });
                                cx.notify();
                            });
                        })
                        .detach();
                    })),
            );
            card = card.child(actions);
        } else {
            card = card.child(Self::render_disconnected_actions(device, state, cx, index));
        }

        div().flex().flex_col().gap(px(8.0)).child(card)
    }

    fn render_disconnected_actions(
        device: &DeviceInfo,
        state: &AppState,
        cx: &mut Context<crate::RootView>,
        index: usize,
    ) -> impl IntoElement {
        let pending = state.connect_pending;
        let address = device
            .ip_address
            .clone()
            .filter(|address| !address.trim().is_empty());

        let Some(address) = address else {
            return div()
                .text_xs()
                .text_color(Theme::text_muted())
                .child("缺少上次连接地址，请在手机控制中心选择本机");
        };

        let click_address = address.clone();
        let click_id = device.device_id.clone();
        div().flex().flex_col().gap(px(8.0)).child(
            Button::new(("btn-reverse-connect", index))
                .primary()
                .disabled(pending)
                .label(if pending {
                    "等待手机确认…"
                } else {
                    "连接"
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    if this.state.connect_pending {
                        return;
                    }
                    this.state.connect_pending = true;
                    this.state.set_toast("正在连接，请在手机上确认");
                    let address = click_address.clone();
                    let device_id = click_id.clone();
                    cx.notify();
                    cx.spawn(async move |this, app| {
                        let client = DaemonClient::new();
                        let result = app
                            .background_executor()
                            .spawn(async move { client.connect_device(&address, &device_id).await })
                            .await;
                        if let Err(error) = result {
                            let _ = this.update(app, |view, cx| {
                                view.state.connect_pending = false;
                                view.state.set_toast(format!("连接失败：{error}"));
                                cx.notify();
                            });
                        }
                    })
                    .detach();
                })),
        )
    }

    fn render_no_device_view() -> impl IntoElement {
        div()
            .flex_1()
            .min_h(px(320.0))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(12.0))
            .child(
                Icon::new(IconName::Smartphone)
                    .size(px(40.0))
                    .text_color(Theme::text_muted()),
            )
            .child(
                div()
                    .text_base()
                    .font_weight(FontWeight::BOLD)
                    .text_color(Theme::text_secondary())
                    .child("未连接设备"),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(Theme::text_muted())
                    .child("连接同一 Wi-Fi 后，在手机控制中心选择本机"),
            )
    }
}
