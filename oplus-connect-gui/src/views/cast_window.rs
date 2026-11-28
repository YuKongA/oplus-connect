use crate::cast_input;
use crate::cast_media::VideoFrame;
use crate::cast_session::InputSender;
use gpui_kit::component::Icon;
use gpui_kit::component::TitleBar as KitTitleBar;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::*;
use smallvec::SmallVec;
use std::sync::Arc;

// Drain each wheel notch gradually to preserve the phone's continuous scrolling.
#[derive(Default)]
struct WheelState {
    budget: f32,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    task: Option<Task<()>>,
}

impl WheelState {
    fn accumulate(&mut self, lines: f32, point: (i32, i32), size: (u32, u32)) {
        self.budget = (self.budget + lines / 3.0).clamp(-6.0, 6.0);
        (self.x, self.y) = point;
        (self.width, self.height) = size;
    }

    fn has_step(&self) -> bool {
        self.budget.abs() >= 1.0
    }

    fn take_step(&mut self) -> Option<Vec<u8>> {
        if !self.has_step() {
            return None;
        }
        let delta = self.budget.signum();
        self.budget -= delta;
        Some(cast_input::scroll(
            0,
            delta as i32,
            self.x,
            self.y,
            self.width,
            self.height,
        ))
    }

    fn release(&mut self) {
        // Dropping the GPUI task cancels its timer before input is released.
        *self = Self::default();
    }
}

// Navigation glyphs as 16-unit SVGs; GPUI paints the data as an alpha mask
// tinted by the icon color.
const NAV_MENU_GLYPH: &[u8] = include_bytes!("../../assets/nav/ic_sysbar_recent.svg");
const NAV_HOME_GLYPH: &[u8] = include_bytes!("../../assets/nav/ic_sysbar_home.svg");
const NAV_BACK_GLYPH: &[u8] = include_bytes!("../../assets/nav/ic_sysbar_back.svg");

pub struct CastWindow {
    frame: Option<Arc<RenderImage>>,
    size: (u32, u32),
    input: Option<InputSender>,
    input_enabled: bool,
    focus: FocusHandle,
    pressed: bool,
    // Throttle motion only; a release must never be dropped.
    last_pointer: Option<(u8, i32, i32, std::time::Instant)>,
    // Immediate hover cancels the phone's fling.
    release_at: Option<std::time::Instant>,
    wheel: WheelState,
    keyboard: cast_input::KeyboardState,
    subscriptions: Vec<Subscription>,
}

impl CastWindow {
    pub fn new(input: Option<InputSender>, cx: &mut Context<Self>) -> Self {
        Self {
            frame: None,
            size: (720, 1560),
            input,
            input_enabled: crate::cast_settings::input(),
            focus: cx.focus_handle(),
            pressed: false,
            last_pointer: None,
            release_at: None,
            wheel: WheelState::default(),
            keyboard: cast_input::KeyboardState::default(),
            subscriptions: Vec::new(),
        }
    }

    pub fn set_input_enabled(&mut self, enabled: bool) {
        if !enabled {
            self.release_input();
        }
        self.input_enabled = enabled;
    }

    pub fn release_input(&mut self) {
        self.wheel.release();
        if self.pressed {
            self.pressed = false;
            if let Some((_, x, y, _)) = self.last_pointer {
                self.send_pointer(cast_input::ACTION_UP, x, y);
            }
        }
        let input = self.input.as_ref();
        self.keyboard.release(|frame| {
            if let Some(input) = input {
                let _ = input.send(frame.to_vec());
            }
        });
        self.last_pointer = None;
    }

    fn queue_scroll(&mut self, lines: f32, point: (i32, i32), cx: &mut Context<Self>) {
        let Some(input) = self.input.as_ref().filter(|input| !input.is_closed()) else {
            return;
        };
        self.wheel.accumulate(lines, point, self.size);
        if !self.wheel.has_step() {
            self.wheel.task.take();
            return;
        }
        if self.wheel.task.is_some() {
            return;
        }
        let input = input.clone();
        let executor = cx.background_executor().clone();
        self.wheel.task = Some(cx.spawn(async move |this, app| {
            loop {
                executor.timer(std::time::Duration::from_millis(16)).await;
                let draining = this.update(app, |view, _| {
                    if let Some(packet) = view.wheel.take_step()
                        && input.send(packet).is_ok()
                        && view.wheel.has_step()
                    {
                        return true;
                    }
                    view.wheel.task.take();
                    false
                });
                if !matches!(draining, Ok(true)) {
                    break;
                }
            }
        }));
    }

    fn key_changed(&mut self, event: &Keystroke, pressed: bool) {
        if !self.input_enabled {
            return;
        }
        let meta = cast_input::meta_state(
            event.modifiers.shift,
            event.modifiers.control,
            event.modifiers.alt,
        );
        let input = self.input.as_ref();
        self.keyboard.key_changed(
            cast_input::android_keycode(&event.key),
            meta,
            pressed,
            |frame| {
                if let Some(input) = input {
                    let _ = input.send(frame.to_vec());
                }
            },
        );
    }

    fn send_pointer(&mut self, action: u8, x: i32, y: i32) {
        let now = std::time::Instant::now();
        // Always forward presses and releases, even at the same coordinates.
        let throttle = matches!(
            action,
            cast_input::ACTION_MOVE | cast_input::ACTION_HOVER_MOVE
        );
        if action == cast_input::ACTION_HOVER_MOVE
            && let Some(at) = self.release_at
            && at.elapsed() < std::time::Duration::from_millis(250)
        {
            return;
        }
        if action == cast_input::ACTION_UP {
            self.release_at = Some(now);
        }
        if throttle && let Some((last_action, last_x, last_y, at)) = self.last_pointer {
            let unchanged = last_action == action && last_x == x && last_y == y;
            let too_soon = at.elapsed() < std::time::Duration::from_millis(16);
            if unchanged || too_soon {
                return;
            }
        }
        self.last_pointer = Some((action, x, y, now));
        self.send(cast_input::touch(action, x, y, self.size.0, self.size.1));
    }

    fn send(&self, frame: Vec<u8>) {
        if let Some(input) = &self.input {
            let _ = input.send(frame);
        }
    }

    /// Map a window point into phone pixels through the displayed picture.
    fn phone_point(&self, position: Point<Pixels>, window: &Window) -> (i32, i32) {
        let bounds = window.bounds();
        cast_input::to_phone(
            (position.x.into(), position.y.into()),
            (bounds.size.width.into(), bounds.size.height.into()),
            f32::from(gpui_kit::component::TITLE_BAR_HEIGHT),
            cast_input::NAV_BAR_HEIGHT,
            self.size,
        )
    }

    /// Press and release once with an Android navigation keycode.
    fn send_nav_key(&self, keycode: u8) {
        let Some(input) = &self.input else { return };
        let _ = input.send(cast_input::key(keycode, 0, true).to_vec());
        let _ = input.send(cast_input::key(keycode, 0, false).to_vec());
    }

    /// Bottom bar split into three equal, fully clickable thirds.
    fn nav_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let nav_button = |id: &'static str,
                          glyph: &'static [u8],
                          label: &'static str,
                          keycode: u8,
                          cx: &mut Context<Self>| {
            div()
                .id(id)
                .flex_1()
                .h_full()
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .hover(|style| style.bg(crate::theme::Theme::bg_card_hover()))
                .aria_label(label)
                .tooltip(move |window, cx| Tooltip::new(label).build(window, cx))
                .on_click(cx.listener(move |this, _, _, _| this.send_nav_key(keycode)))
                .child(
                    Icon::default()
                        .data(glyph)
                        .size(px(18.0))
                        .text_color(crate::theme::Theme::text_secondary()),
                )
        };

        div()
            .h(px(cast_input::NAV_BAR_HEIGHT))
            .w_full()
            .flex_shrink_0()
            .flex()
            .items_center()
            .bg(crate::theme::Theme::bg_header())
            .border_t_1()
            .border_color(crate::theme::Theme::border_subtle())
            .child(nav_button(
                "cast-nav-menu",
                NAV_MENU_GLYPH,
                "菜单",
                cast_input::KEYCODE_APP_SWITCH,
                cx,
            ))
            .child(nav_button(
                "cast-nav-home",
                NAV_HOME_GLYPH,
                "主页",
                cast_input::KEYCODE_HOME,
                cx,
            ))
            .child(nav_button(
                "cast-nav-back",
                NAV_BACK_GLYPH,
                "返回",
                cast_input::KEYCODE_BACK,
                cx,
            ))
    }

    pub fn dimensions(&self) -> (u32, u32) {
        self.size
    }

    /// Transfer the frame buffer without a second per-frame copy.
    pub fn set_frame(&mut self, frame: VideoFrame, window: &mut Window, cx: &mut Context<Self>) {
        let Some(buffer) = image::ImageBuffer::from_raw(frame.width, frame.height, frame.data)
        else {
            return;
        };
        self.size = (frame.width, frame.height);
        let image = Arc::new(RenderImage::new(SmallVec::from_const([image::Frame::new(
            buffer,
        )])));
        if let Some(old) = self.frame.replace(image)
            && let Err(error) = window.drop_image(old)
        {
            tracing::warn!(%error, "could not evict previous cast image");
        }
        cx.notify();
    }
}

impl Drop for CastWindow {
    fn drop(&mut self) {
        self.release_input();
    }
}

impl Render for CastWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.subscriptions.is_empty() {
            self.subscriptions
                .push(cx.on_blur(&self.focus, window, |this, _, _| {
                    this.release_input();
                }));
            self.subscriptions
                .push(cx.observe_window_activation(window, |this, window, _| {
                    if !window.is_window_active() {
                        this.release_input();
                    }
                }));
        }
        let title = div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .min_w_0()
            .child(img("logo.png").size(px(18.0)).rounded(px(3.0)))
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::BOLD)
                    .text_color(crate::theme::Theme::text_primary())
                    .child("投屏"),
            );

        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(crate::theme::Theme::bg_base())
            .child(KitTitleBar::new().child(title))
            .child(
                div()
                    .flex_1()
                    .w_full()
                    .min_h_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(gpui_kit::black())
                    .track_focus(&self.focus)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseDownEvent, window, cx| {
                            window.focus(&this.focus, cx);
                            if !this.input_enabled || this.pressed {
                                return;
                            }
                            this.pressed = true;
                            let (x, y) = this.phone_point(event.position, window);
                            this.send_pointer(cast_input::ACTION_DOWN, x, y);
                            cx.notify();
                        }),
                    )
                    .on_mouse_up(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseUpEvent, window, cx| {
                            if !this.pressed {
                                return;
                            }
                            this.pressed = false;
                            let (x, y) = this.phone_point(event.position, window);
                            this.send_pointer(cast_input::ACTION_UP, x, y);
                            cx.notify();
                        }),
                    )
                    .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, _cx| {
                        if !this.input_enabled {
                            return;
                        }
                        let (x, y) = this.phone_point(event.position, window);
                        if this.pressed && event.pressed_button != Some(MouseButton::Left) {
                            this.pressed = false;
                            this.send_pointer(cast_input::ACTION_UP, x, y);
                            return;
                        }
                        let action = if this.pressed {
                            cast_input::ACTION_MOVE
                        } else {
                            cast_input::ACTION_HOVER_MOVE
                        };
                        this.send_pointer(action, x, y);
                    }))
                    .on_mouse_up_out(
                        MouseButton::Left,
                        cx.listener(|this, event: &MouseUpEvent, window, cx| {
                            if !this.pressed {
                                return;
                            }
                            this.pressed = false;
                            let (x, y) = this.phone_point(event.position, window);
                            this.send_pointer(cast_input::ACTION_UP, x, y);
                            cx.notify();
                        }),
                    )
                    .on_scroll(cx.listener(|this, event: &ScrollEvent, window, cx| {
                        if !this.input_enabled {
                            return;
                        }
                        // Three desktop lines make one phone scroll unit.
                        let lines = match event.delta {
                            ScrollDelta::Lines(lines) => lines.y,
                            ScrollDelta::Pixels(pixels) => f32::from(pixels.y) / 40.0,
                        };
                        if !lines.is_finite() || lines == 0.0 {
                            return;
                        }
                        let point = this.phone_point(event.position, window);
                        this.queue_scroll(lines, point, cx);
                    }))
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, _cx| {
                        this.key_changed(&event.keystroke, true);
                    }))
                    .on_key_up(cx.listener(|this, event: &KeyUpEvent, _window, _cx| {
                        this.key_changed(&event.keystroke, false);
                    }))
                    .on_modifiers_changed(cx.listener(
                        |this, event: &ModifiersChangedEvent, _window, _cx| {
                            if !this.input_enabled {
                                return;
                            }
                            let meta = cast_input::meta_state(
                                event.modifiers.shift,
                                event.modifiers.control,
                                event.modifiers.alt,
                            );
                            let input = this.input.as_ref();
                            this.keyboard.modifiers_changed(meta, |frame| {
                                if let Some(input) = input {
                                    let _ = input.send(frame.to_vec());
                                }
                            });
                        },
                    ))
                    .children(self.frame.clone().map(|frame| {
                        img(ImageSource::Render(frame))
                            .object_fit(ObjectFit::Contain)
                            .size_full()
                    })),
            )
            .child(self.nav_bar(cx))
    }
}

#[cfg(test)]
mod tests {
    use super::{Task, WheelState, cast_input};

    #[test]
    fn fractional_scroll_waits_for_a_whole_step_and_uses_latest_coordinates() {
        let mut wheel = WheelState::default();
        wheel.accumulate(1.5, (0, 0), (20, 30));
        assert!(wheel.take_step().is_none());
        wheel.accumulate(3.0, (3, 4), (40, 50));
        assert_eq!(
            wheel.take_step(),
            Some(cast_input::scroll(0, 1, 3, 4, 40, 50))
        );
        assert!(!wheel.has_step());
        assert_eq!(wheel.budget, 0.5);
        wheel.accumulate(-4.5, (2, 3), (50, 40));
        assert_eq!(
            wheel.take_step(),
            Some(cast_input::scroll(0, -1, 2, 3, 50, 40))
        );
        assert_eq!(wheel.budget, 0.0);
    }

    #[test]
    fn releasing_wheel_disarms_timer_and_discards_pending_fraction() {
        let mut wheel = WheelState::default();
        wheel.accumulate(4.5, (2, 3), (10, 20));
        wheel.task = Some(Task::ready(()));
        wheel.release();
        assert!(wheel.task.is_none());
        assert!(wheel.take_step().is_none());
        wheel.accumulate(1.5, (1, 1), (10, 20));
        assert!(!wheel.has_step());
    }

    #[test]
    fn wheel_bursts_clamp_both_directions_and_stop_when_drained() {
        let mut wheel = WheelState::default();
        for sign in [-1, 1] {
            wheel.accumulate(sign as f32 * 120.0, (0, 0), (10, 20));
            for _ in 0..6 {
                assert_eq!(
                    wheel.take_step(),
                    Some(cast_input::scroll(0, sign, 0, 0, 10, 20))
                );
            }
            assert!(!wheel.has_step());
            assert!(wheel.take_step().is_none());
        }
    }
}
