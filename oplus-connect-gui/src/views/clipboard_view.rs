use crate::models::*;
use crate::state::AppState;
use crate::theme::Theme;
use gpui_kit::assets::IconName;
use gpui_kit::component::Icon;
use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants};
use gpui_kit::component::tab::{Tab, TabBar};
use gpui_kit::component::{Disableable, WindowExt};
use gpui_kit::*;

pub struct ClipboardView;

impl ClipboardView {
    pub fn render(
        state: &AppState,
        window: &mut Window,
        cx: &mut Context<crate::RootView>,
    ) -> impl IntoElement {
        let rows = state.clipboard_list_state(window.rem_size());
        let total_count = state.clipboard_history.len();
        let has_clearable_records = state
            .clipboard_history
            .iter()
            .any(|record| !record.is_favorite);

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
                                    .child("剪贴板"),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(Theme::text_muted())
                                    .child(format!("共 {} 条", total_count)),
                            ),
                    )
                    .child(
                        Button::new("btn-clear-clipboards")
                            .ghost()
                            .compact()
                            .size(px(28.0))
                            .p(px(0.0))
                            .tooltip("清空剪贴板历史记录")
                            .child(
                                Icon::new(IconName::Trash)
                                    .size(px(16.0))
                                    .text_color(Theme::text_secondary()),
                            )
                            .accessibility_label("清空剪贴板历史记录")
                            .disabled(!has_clearable_records)
                            .on_click(cx.listener(|_, _, window, cx| {
                                Self::confirm_clear(window, cx);
                            })),
                    ),
            )
            .child(
                TabBar::new("clipboard-filter-tabs")
                    .underline()
                    .selected_index(Self::filter_index(state.clipboard_filter))
                    .on_click(cx.listener(|this, index, _, cx| {
                        let filter = Self::filter_from_index(*index);
                        if this.state.clipboard_filter != filter {
                            this.state.clipboard_filter = filter;
                            cx.notify();
                        }
                    }))
                    .child(Tab::new().label("全部"))
                    .child(Tab::new().label("收藏"))
                    .child(Tab::new().label("电脑端"))
                    .child(Tab::new().label("移动端")),
            )
            .child(if rows.item_count() == 0 {
                Self::render_empty_state().into_any_element()
            } else {
                list(
                    rows,
                    cx.processor(|this, index, _, cx| {
                        let row = this.state.clipboard_row(index);
                        div()
                            .w_full()
                            .pb(px(10.0))
                            .child(Self::render_card(row, &this.state, cx))
                            .into_any_element()
                    }),
                )
                .flex_1()
                .min_h_0()
                .pr(px(6.0))
                .into_any_element()
            })
    }

    fn confirm_clear(window: &mut Window, cx: &mut Context<crate::RootView>) {
        let owner = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |dialog, _, _| {
            let owner = owner.clone();
            dialog
                .centered()
                .title("清空剪贴板历史？")
                .description("将删除所有未收藏的历史记录，收藏记录会保留。此操作无法撤销，不会清除系统剪贴板中的当前内容。")
                .confirm()
                .ok_text("确认清空")
                .ok_variant(ButtonVariant::Danger)
                .cancel_text("取消")
                .on_ok(move |_, _, cx| {
                    let _ = owner.update(cx, |view, cx| {
                        view.state.clear_clipboard_records();
                        cx.notify();
                    });
                    true
                })
        });
    }

    fn filter_index(filter: ClipboardFilter) -> usize {
        match filter {
            ClipboardFilter::All => 0,
            ClipboardFilter::Favorites => 1,
            ClipboardFilter::Local => 2,
            ClipboardFilter::Remote => 3,
        }
    }

    fn filter_from_index(index: usize) -> ClipboardFilter {
        match index {
            1 => ClipboardFilter::Favorites,
            2 => ClipboardFilter::Local,
            3 => ClipboardFilter::Remote,
            _ => ClipboardFilter::All,
        }
    }

    fn render_card(
        record: &ClipboardRecord,
        state: &AppState,
        cx: &mut Context<crate::RootView>,
    ) -> impl IntoElement {
        let is_favorite = record.is_favorite;
        let id_for_fav = record.id.clone();
        let id_for_del = record.id.clone();
        let id_for_copy = record.id.clone();
        let is_recently_copied = state.last_copied_id.as_ref() == Some(&record.id);

        let (source_icon, source_label) = match &record.source {
            ClipboardSource::Local => (IconName::Monitor, SharedString::new_static("本机电脑")),
            ClipboardSource::Remote(dev) => {
                let is_tablet = state
                    .known_devices
                    .iter()
                    .any(|d| d.device_name == dev.as_str() && d.device_type == "tablet");
                let icon = if is_tablet {
                    IconName::Tablet
                } else {
                    IconName::Smartphone
                };
                let label = if dev.is_empty() {
                    SharedString::new_static("移动设备")
                } else {
                    dev.clone()
                };
                (icon, label)
            }
        };

        div()
            .id(record.id.clone())
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
                            .child(
                                Icon::new(source_icon)
                                    .size(px(20.0))
                                    .text_color(Theme::text_muted())
                                    .flex_shrink_0(),
                            )
                            .child(
                                div()
                                    .min_w_0()
                                    .truncate()
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(Theme::text_primary())
                                    .child(source_label),
                            )
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_xs()
                                    .text_color(Theme::text_muted())
                                    .child(format!(
                                        "{} · {} 字符",
                                        record.timestamp.format("%H:%M:%S"),
                                        record.char_count
                                    )),
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
                                    .tooltip(if is_recently_copied {
                                        "已复制"
                                    } else {
                                        "复制"
                                    })
                                    .child(
                                        Icon::new(IconName::Copy)
                                            .size(px(16.0))
                                            .text_color(Theme::text_secondary()),
                                    )
                                    .accessibility_label("复制剪贴板内容")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        let Some(row) = this
                                            .state
                                            .clipboard_history
                                            .iter()
                                            .find(|r| r.id == id_for_copy)
                                        else {
                                            return;
                                        };
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            row.content.to_string(),
                                        ));
                                        this.state.last_copied_id = Some(id_for_copy.clone());
                                        this.state.set_toast("已复制");
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("favorite")
                                    .ghost()
                                    .compact()
                                    .toggled(is_favorite)
                                    .size(px(28.0))
                                    .p(px(0.0))
                                    .tooltip(if is_favorite {
                                        "取消收藏"
                                    } else {
                                        "收藏"
                                    })
                                    .child(
                                        Icon::new(if is_favorite {
                                            IconName::StarFill
                                        } else {
                                            IconName::Star
                                        })
                                        .size(px(16.0))
                                        .text_color(
                                            if is_favorite {
                                                Theme::warning()
                                            } else {
                                                Theme::text_secondary()
                                            },
                                        ),
                                    )
                                    .accessibility_label(if is_favorite {
                                        "取消收藏"
                                    } else {
                                        "收藏剪贴板记录"
                                    })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.state.toggle_clipboard_favorite(&id_for_fav);
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
                                    .accessibility_label("删除剪贴板记录")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.state.delete_clipboard_record(&id_for_del);
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(Theme::text_secondary())
                    .child(record.content.clone()),
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
                Icon::new(IconName::Clipboard)
                    .size(px(48.0))
                    .text_color(Theme::text_muted()),
            )
            .child(
                div()
                    .text_base()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(Theme::text_secondary())
                    .child("暂无记录"),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(Theme::text_muted())
                    .child("在任一设备复制文本即可同步"),
            )
    }
}
