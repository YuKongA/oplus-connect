use crate::state::AppState;
use crate::theme::Theme;
use crate::RootView;
use gpui_kit::component::group_box::{GroupBox, GroupBoxVariants};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::select::Select;
use gpui_kit::component::switch::Switch;
use gpui_kit::*;

pub struct SettingsView;

impl SettingsView {
    pub fn render(
        state: &AppState,
        codec: Option<
            &Entity<
                gpui_kit::component::select::SelectState<
                    gpui_kit::component::searchable_list::SearchableVec<&'static str>,
                >,
            >,
        >,
        kms_direction: Option<
            &Entity<
                gpui_kit::component::select::SelectState<
                    gpui_kit::component::searchable_list::SearchableVec<&'static str>,
                >,
            >,
        >,
        expand_cast_fps: Option<&Entity<InputState>>,
        expand_cast_bitrate: Option<&Entity<InputState>>,
        _window: &mut Window,
        cx: &mut Context<RootView>,
    ) -> impl IntoElement {
        div()
            .id("settings-view-scroll")
            .flex_1()
            .h_full()
            .overflow_y_scroll()
            .p(px(20.0))
            .flex()
            .flex_col()
            .gap(px(16.0))
            .child(
                div()
                    .text_xl()
                    .font_weight(FontWeight::BOLD)
                    .text_color(Theme::text_primary())
                    .child("设置"),
            )
            .child(
                GroupBox::new()
                    .id("settings-options")
                    .outline()
                    .title("选项")
                    .child(Self::switch_row(
                        "深色模式",
                        "使用深色主题",
                        Switch::new("toggle-dark-mode")
                            .checked(Theme::is_dark())
                            .accessibility_label("深色模式")
                            .on_change(cx.listener(|this, next, _, cx| {
                                Theme::set_dark(*next, cx);
                                this.state.set_toast(if *next {
                                    "深色模式"
                                } else {
                                    "浅色模式"
                                });
                                cx.notify();
                            })),
                    )),
            )
            .child(
                GroupBox::new()
                    .id("settings-cast")
                    .outline()
                    .title("手机投屏")
                    .child(Self::switch_row(
                        "键盘与鼠标",
                        "在投屏窗口中操作手机",
                        Switch::new("toggle-cast-input")
                            .checked(state.cast_input)
                            .accessibility_label("键鼠反控")
                            .on_change(cx.listener(|this, next, _, cx| {
                                this.set_cast_input(*next, cx);
                                this.state.set_toast(if *next {
                                    "已启用键鼠反控"
                                } else {
                                    "已停用键鼠反控"
                                });
                                cx.notify();
                            })),
                    ))
                    .child(Self::switch_row(
                        "音频",
                        "电脑播放手机声音",
                        Switch::new("toggle-cast-audio")
                            .checked(state.cast_audio)
                            .accessibility_label("投屏音频")
                            .on_change(cx.listener(|this, next, _, cx| {
                                this.set_cast_audio(*next);
                                this.state.set_toast(if *next {
                                    "下次投屏将播放手机声音"
                                } else {
                                    "下次投屏将关闭手机声音"
                                });
                                cx.notify();
                            })),
                    ))
                    .child(Self::control_row(
                        "编码方式",
                        "请优先使用 H.265",
                        // Fixed trigger width so this row lines up with the
                        // switch column; the popup matches it exactly.
                        div().w(px(160.0)).children(codec.map(|state| {
                            Select::new(state)
                                .w_full()
                                .menu_width(px(160.0))
                                .menu_max_h(rems(9.0))
                        })),
                    )),
            )
            .child(
                GroupBox::new()
                    .id("settings-expand-cast")
                    .outline()
                    .title("平板屏幕共享")
                    .child(Self::control_row(
                        "帧率（FPS）",
                        "输入 1–144 的整数",
                        div().w(px(160.0)).children(expand_cast_fps.map(Input::new)),
                    ))
                    .child(Self::control_row(
                        "初始码率（kbps）",
                        "输入 500–50000 的整数",
                        div()
                            .w(px(160.0))
                            .children(expand_cast_bitrate.map(Input::new)),
                    ))
                    .child(Self::switch_row(
                        "动态码率",
                        "开启后码率会随共享状况变化",
                        Switch::new("toggle-expand-cast-adaptive")
                            .checked(state.expand_cast_settings.adaptive_bitrate)
                            .accessibility_label("动态码率")
                            .on_change(cx.listener(|this, next, _, cx| {
                                this.update_expand_cast_adaptive(*next, cx);
                            })),
                    ))
                    .children(state.expand_cast_settings_feedback.as_ref().map(|message| {
                        div()
                            .w_full()
                            .text_xs()
                            .text_color(Theme::text_secondary())
                            .child(message.clone())
                    })),
            )
            .child(
                GroupBox::new()
                    .id("settings-kms")
                    .outline()
                    .title("平板键鼠共享")
                    .child(Self::control_row(
                        "光标移出方向",
                        "从所选方向移出屏幕进入平板",
                        div().w(px(160.0)).children(kms_direction.map(|state| {
                            Select::new(state)
                                .w_full()
                                .menu_width(px(160.0))
                                .menu_max_h(rems(9.0))
                        })),
                    )),
            )
            .child(
                GroupBox::new()
                    .id("settings-about")
                    .outline()
                    .title("关于")
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .flex_col()
                            .gap(px(6.0))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .gap(px(8.0))
                                    .child(
                                        div()
                                            .text_sm()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(Theme::text_primary())
                                            .child("OPLUS 互联"),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(Theme::text_muted())
                                            .child(crate::APP_VERSION),
                                    ),
                            )
                            .child(div().text_xs().text_color(Theme::text_secondary()).child(
                                "ColorOS 设备互联客户端，支持剪贴板、通知、投屏与键鼠共享。",
                            ))
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(Theme::text_muted())
                                    .child("基于官方协议实现，与 OPPO、一加无隶属关系。"),
                            ),
                    ),
            )
    }

    fn switch_row(title: &'static str, desc: &'static str, control: Switch) -> impl IntoElement {
        div()
            .w_full()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(16.0))
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(Theme::text_primary())
                            .child(title),
                    )
                    .child(div().text_xs().text_color(Theme::text_muted()).child(desc)),
            )
            .child(control.color(Theme::primary()))
    }

    fn control_row(
        title: &'static str,
        desc: &'static str,
        control: impl IntoElement,
    ) -> impl IntoElement {
        div()
            .w_full()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(16.0))
            .child(
                div()
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(Theme::text_primary())
                            .child(title),
                    )
                    .child(div().text_xs().text_color(Theme::text_muted()).child(desc)),
            )
            .child(control)
    }
}
