use crate::models::*;
use crate::state::AppState;
use crate::theme::Theme;
use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::{Disableable, Icon};
use gpui_kit::*;

pub struct NotificationView;

impl NotificationView {
    pub fn render(
        state: &AppState,
        window: &mut Window,
        cx: &mut Context<crate::RootView>,
    ) -> impl IntoElement {
        let rows = state.notification_list_state(window.rem_size());
        let total_count = state.notification_history.len();

        div()
            .flex_1()
            .h_full()
            .flex()
            .flex_col()
            .p(px(20.0))
            .gap(px(16.0))
            .overflow_hidden()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .min_w_0()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .text_xl()
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(Theme::text_primary())
                                    .child("通知"),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(Theme::text_muted())
                                    .child(format!("共 {} 条", total_count)),
                            ),
                    )
                    .child(
                        Button::new("btn-clear-notifications")
                            .ghost()
                            .compact()
                            .size(px(28.0))
                            .p(px(0.0))
                            .tooltip("清空所有通知")
                            .child(
                                Icon::new(IconName::Trash)
                                    .size(px(16.0))
                                    .text_color(Theme::text_secondary()),
                            )
                            .accessibility_label("清空所有通知")
                            .disabled(total_count == 0)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.state.clear_notifications();
                                cx.notify();
                            })),
                    ),
            )
            .child(if rows.item_count() == 0 {
                Self::render_empty_state().into_any_element()
            } else {
                list(
                    rows,
                    cx.processor(|this, index, _, cx| {
                        let row = this.state.notification_row(index);
                        div()
                            .w_full()
                            .pb(px(10.0))
                            .child(Self::render_card(row, cx))
                            .into_any_element()
                    }),
                )
                .flex_1()
                .min_h_0()
                .pr(px(6.0))
                .into_any_element()
            })
    }

    fn render_card(
        item: &NotificationRecord,
        cx: &mut Context<crate::RootView>,
    ) -> impl IntoElement {
        let id_for_del = item.id.clone();
        let id_for_copy = item.id.clone();
        let app_label = if item.label.is_empty() {
            SharedString::new_static("手机应用")
        } else {
            item.label.clone()
        };

        div()
            .id(item.key.clone())
            .p(px(12.0))
            .rounded_lg()
            .bg(Theme::bg_card())
            .border_1()
            .border_color(Theme::border_card())
            .flex()
            .flex_col()
            .gap(px(8.0))
            .hover(|s| {
                s.bg(Theme::bg_card_hover())
                    .border_color(Theme::border_subtle())
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .min_w_0()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(10.0))
                            .min_w_0()
                            .child(match &item.icon {
                                Some(icon) => img(icon.image().clone())
                                    .size(px(20.0))
                                    .rounded(px(4.0))
                                    .flex_shrink_0()
                                    .into_any_element(),
                                None => Icon::new(IconName::Bell)
                                    .size(px(20.0))
                                    .text_color(Theme::text_muted())
                                    .flex_shrink_0()
                                    .into_any_element(),
                            })
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(Theme::text_primary())
                                    .child(app_label),
                            )
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_xs()
                                    .text_color(Theme::text_muted())
                                    .child(item.timestamp.format("%H:%M:%S").to_string()),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .flex_shrink_0()
                            .child(
                                Button::new("copy")
                                    .ghost()
                                    .compact()
                                    .size(px(28.0))
                                    .p(px(0.0))
                                    .tooltip("复制")
                                    .child(
                                        Icon::new(IconName::Copy)
                                            .size(px(16.0))
                                            .text_color(Theme::text_secondary()),
                                    )
                                    .accessibility_label("复制通知内容")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        let Some(row) = this
                                            .state
                                            .notification_history
                                            .iter()
                                            .find(|r| r.id == id_for_copy)
                                        else {
                                            return;
                                        };
                                        cx.write_to_clipboard(ClipboardItem::new_string(format!(
                                            "{}\n{}",
                                            row.title, row.content
                                        )));
                                        this.state.set_toast("已复制");
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("delete")
                                    .ghost()
                                    .compact()
                                    .size(px(28.0))
                                    .p(px(0.0))
                                    .tooltip("删除")
                                    .child(
                                        Icon::new(IconName::X)
                                            .size(px(16.0))
                                            .text_color(Theme::text_secondary()),
                                    )
                                    .accessibility_label("删除通知")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.state.delete_notification_record(&id_for_del);
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::BOLD)
                            .text_color(Theme::text_primary())
                            .child(if item.title.is_empty() {
                                SharedString::new_static("新通知")
                            } else {
                                item.title.clone()
                            }),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(Theme::text_secondary())
                            .child(item.content.clone()),
                    ),
            )
    }

    fn render_empty_state() -> impl IntoElement {
        div()
            .flex_1()
            .h(px(300.0))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(12.0))
            .child(
                Icon::new(IconName::Bell)
                    .size(px(48.0))
                    .text_color(Theme::text_muted()),
            )
            .child(
                div()
                    .text_base()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(Theme::text_secondary())
                    .child("暂无通知"),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(Theme::text_muted())
                    .child("手机通知会显示在这里"),
            )
    }
}
