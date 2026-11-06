pub mod cast_window;
pub mod clipboard_view;
pub mod confirm_modal;
pub mod device_detail_view;
pub mod notification_view;
pub mod settings_view;
pub mod window_frame;

use crate::models::NavTab;
use crate::state::AppState;
use crate::theme::Theme;
use gpui_kit::component::resizable::{h_resizable, resizable_panel};
use gpui_kit::component::WindowExt;
use gpui_kit::*;

pub struct MainContainer;

impl MainContainer {
    pub fn render(
        state: &AppState,
        kms: &crate::kms_session::KmsViewState,
        expand_cast: &crate::expand_cast_session::ExpandCastViewState,
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
        expand_cast_fps: Option<&Entity<gpui_kit::component::input::InputState>>,
        expand_cast_bitrate: Option<&Entity<gpui_kit::component::input::InputState>>,
        window: &mut Window,
        cx: &mut Context<crate::RootView>,
    ) -> impl IntoElement {
        let active_view: AnyElement = match state.active_tab {
            NavTab::Clipboard => {
                clipboard_view::ClipboardView::render(state, window, cx).into_any_element()
            }
            NavTab::Notifications => {
                notification_view::NotificationView::render(state, window, cx).into_any_element()
            }
            NavTab::DeviceDetail => {
                device_detail_view::DeviceDetailView::render(state, kms, expand_cast, window, cx)
                    .into_any_element()
            }
            NavTab::Settings => settings_view::SettingsView::render(
                state,
                codec,
                kms_direction,
                expand_cast_fps,
                expand_cast_bitrate,
                window,
                cx,
            )
            .into_any_element(),
        };

        div()
            .size_full()
            .bg(Theme::bg_base())
            .flex()
            .flex_col()
            .overflow_hidden()
            .child(window_frame::WindowFrame::render_titlebar(
                state, window, cx,
            ))
            .child(
                div().flex_1().min_h_0().overflow_hidden().child(
                    h_resizable("main-sidebar-split")
                        .child(
                            resizable_panel()
                                .size(px(230.0))
                                .size_range(px(200.0)..px(360.0))
                                .fixed()
                                .child(window_frame::WindowFrame::render_sidebar(
                                    state, window, cx,
                                )),
                        )
                        .child(
                            resizable_panel().child(
                                div()
                                    .size_full()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .child(active_view),
                            ),
                        ),
                ),
            )
            .child(
                canvas(
                    |_, _, _| (),
                    |bounds, _, window, cx| {
                        if window.has_active_dialog(cx) {
                            window.paint_quad(fill(bounds, Theme::bg_modal_backdrop()));
                        }
                    },
                )
                .absolute()
                .inset_0(),
            )
    }
}
