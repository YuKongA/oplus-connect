use crate::models::*;
use crate::state::AppState;
use crate::theme::Theme;
use gpui_kit::assets::IconName;
use gpui_kit::component::sidebar::{
    Sidebar as KitSidebar, SidebarGroup, SidebarHeader, SidebarMenu, SidebarMenuItem,
};
use gpui_kit::component::Icon;
use gpui_kit::component::Selectable;
use gpui_kit::component::Theme as KitTheme;
use gpui_kit::component::TitleBar as KitTitleBar;
use gpui_kit::*;

pub struct WindowFrame;
impl WindowFrame {
    pub fn render_titlebar(
        _state: &AppState,
        _window: &mut Window,
        _cx: &mut Context<crate::RootView>,
    ) -> impl IntoElement {
        let title_content = div()
            .id("titlebar-content")
            .flex()
            .items_center()
            .gap(px(8.0))
            .min_w_0()
            .child(
                img(crate::assets::asset_path("logo.png"))
                    .size(px(18.0))
                    .rounded(px(3.0)),
            )
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::BOLD)
                    .text_color(Theme::text_primary())
                    .child("OPLUS 互联"),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(Theme::text_muted())
                    .child(crate::APP_VERSION),
            );

        KitTitleBar::new().child(title_content)
    }

    pub fn render_sidebar(
        state: &AppState,
        window: &mut Window,
        cx: &mut Context<crate::RootView>,
    ) -> impl IntoElement {
        KitSidebar::new("app-sidebar")
            .w_full()
            .border_0()
            .collapsible(false)
            .header(
                SidebarHeader::new()
                    .selected(state.active_tab == NavTab::DeviceDetail)
                    .child(Self::render_device_card(state, window, cx)),
            )
            .child(
                SidebarGroup::new("导航").child(
                    SidebarMenu::new()
                        .child(Self::nav_item(
                            NavTab::Clipboard,
                            IconName::Clipboard,
                            "剪贴板",
                            Some(state.clipboard_history.len()),
                            state.active_tab == NavTab::Clipboard,
                            cx,
                        ))
                        .child(Self::nav_item(
                            NavTab::Notifications,
                            IconName::Bell,
                            "通知",
                            Some(state.notification_history.len()),
                            state.active_tab == NavTab::Notifications,
                            cx,
                        ))
                        .child(Self::nav_item(
                            NavTab::Settings,
                            IconName::Settings,
                            "设置",
                            None,
                            state.active_tab == NavTab::Settings,
                            cx,
                        )),
                ),
            )
            .footer(Self::render_daemon_status(state))
    }

    fn render_daemon_status(state: &AppState) -> impl IntoElement {
        let (status_color, status_label) = if state.daemon_running {
            (Theme::success(), "运行中")
        } else if state.daemon_starting {
            (Theme::warning(), "启动中")
        } else {
            (Theme::danger(), "未运行")
        };

        div()
            .id("sidebar-daemon-status")
            .w_full()
            .flex()
            .items_center()
            .gap(px(8.0))
            .px(px(8.0))
            .py(px(6.0))
            .rounded_lg()
            .bg(Theme::bg_card())
            .border_1()
            .border_color(Theme::border_card())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .text_color(Theme::text_secondary())
                    .child("守护进程"),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_xs()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(status_color)
                    .child(status_label),
            )
    }

    pub fn render_device_card(
        state: &AppState,
        _window: &mut Window,
        cx: &mut Context<crate::RootView>,
    ) -> impl IntoElement {
        let kit_theme = KitTheme::global(cx);
        let icon_bg = kit_theme.success;
        // Keep the glyph light in both themes; dark success_foreground is green.
        let icon_color: Hsla = rgb(0xfafafa).into();
        let icon_radius = kit_theme.radius;
        let (name, status, name_color, status_color) = if let Some(dev) = &state.connected_device {
            (
                dev.device_name.clone(),
                Some("已连接"),
                Theme::text_primary(),
                Theme::primary(),
            )
        } else {
            (
                "未连接设备".to_string(),
                None,
                Theme::text_secondary(),
                Theme::text_muted(),
            )
        };

        div()
            .id("sidebar-device-card")
            .w_full()
            .flex()
            .items_center()
            .gap(px(8.0))
            .cursor_pointer()
            .on_click(cx.listener(|this, _, _, cx| {
                this.state.active_tab = NavTab::DeviceDetail;
                cx.notify();
            }))
            .child(
                div()
                    .size_8()
                    .rounded(icon_radius)
                    .bg(icon_bg)
                    .text_color(icon_color)
                    .flex()
                    .items_center()
                    .justify_center()
                    .flex_shrink_0()
                    .child(Icon::new(IconName::Smartphone)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .font_weight(FontWeight::BOLD)
                    .text_color(name_color)
                    .child(name),
            )
            .children(status.map(|status| {
                div()
                    .flex_shrink_0()
                    .text_xs()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(status_color)
                    .child(status)
            }))
    }

    fn nav_item(
        tab: NavTab,
        icon: IconName,
        label: &'static str,
        badge_count: Option<usize>,
        is_active: bool,
        cx: &mut Context<crate::RootView>,
    ) -> SidebarMenuItem {
        let item = SidebarMenuItem::new(label)
            .icon(icon)
            .active(is_active)
            .accessibility_label(label)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.state.active_tab = tab;
                cx.notify();
            }));

        if let Some(count) = badge_count.filter(|count| *count > 0) {
            item.suffix(move |_, _| {
                div()
                    .px(px(6.0))
                    .rounded_full()
                    .text_xs()
                    .text_color(Theme::text_muted())
                    .child(if count > 99 {
                        "99+".to_string()
                    } else {
                        count.to_string()
                    })
            })
        } else {
            item
        }
    }
}
