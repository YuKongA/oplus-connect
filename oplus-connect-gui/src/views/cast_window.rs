use crate::cast_input;
use crate::cast_media::VideoFrame;
use crate::cast_session::InputSender;
use gpui_kit::component::TitleBar as KitTitleBar;
use gpui_kit::*;
use smallvec::SmallVec;
use std::sync::{Arc, Mutex};

// Drain each wheel notch gradually to preserve the phone's continuous scrolling.
#[derive(Default)]
struct WheelState {
    budget: f32,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
}

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
    wheel: Arc<Mutex<WheelState>>,
    wheel_task: Option<Task<()>>,
    keyboard: cast_input::KeyboardState,
    subscriptions: Vec<Subscription>,
}

impl CastWindow {
    pub fn new(input: Option<InputSender>, cx: &mut Context<Self>) -> Self {
        let wheel = Arc::new(Mutex::new(WheelState::default()));
        let wheel_task = input.clone().map(|sender| {
            let shared = wheel.clone();
            let executor = cx.background_executor().clone();
            cx.background_executor().spawn(async move {
                loop {
                    executor.timer(std::time::Duration::from_millis(16)).await;
                    if sender.is_closed() {
                        break;
                    }
                    let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
                    let delta = if state.budget >= 1.0 {
                        1
                    } else if state.budget <= -1.0 {
                        -1
                    } else {
                        continue;
                    };
                    state.budget -= delta as f32;
                    if sender
                        .send(cast_input::scroll(
                            0,
                            delta,
                            state.x,
                            state.y,
                            state.width,
                            state.height,
                        ))
                        .is_err()
                    {
                        break;
                    }
                }
            })
        });
        Self {
            frame: None,
            size: (720, 1560),
            input,
            input_enabled: crate::cast_settings::input(),
            focus: cx.focus_handle(),
            pressed: false,
            last_pointer: None,
            release_at: None,
            wheel,
            wheel_task,
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
        self.wheel.lock().unwrap_or_else(|e| e.into_inner()).budget = 0.0;
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
        if action == cast_input::ACTION_HOVER_MOVE {
            if let Some(at) = self.release_at {
                if at.elapsed() < std::time::Duration::from_millis(250) {
                    return;
                }
            }
        }
        if action == cast_input::ACTION_UP {
            self.release_at = Some(now);
        }
        if throttle {
            if let Some((last_action, last_x, last_y, at)) = self.last_pointer {
                let unchanged = last_action == action && last_x == x && last_y == y;
                let too_soon = at.elapsed() < std::time::Duration::from_millis(16);
                if unchanged || too_soon {
                    return;
                }
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

    /// Map through the displayed picture, excluding the titlebar and letterbox.
    fn phone_point(&self, position: Point<Pixels>, window: &Window) -> (i32, i32) {
        let bounds = window.bounds();
        cast_input::to_phone(
            (position.x.into(), position.y.into()),
            (bounds.size.width.into(), bounds.size.height.into()),
            f32::from(gpui_kit::component::TITLE_BAR_HEIGHT),
            self.size,
        )
    }

    pub fn dimensions(&self) -> (u32, u32) {
        self.size
    }

    /// Transfer the frame buffer without a second per-frame copy.
    pub fn set_frame(&mut self, frame: VideoFrame, cx: &mut Context<Self>) {
        let Some(buffer) = image::ImageBuffer::from_raw(frame.width, frame.height, frame.data)
        else {
            return;
        };
        self.size = (frame.width, frame.height);
        self.frame = Some(Arc::new(RenderImage::new(SmallVec::from_const([
            image::Frame::new(buffer),
        ]))));
        cx.notify();
    }
}

impl Drop for CastWindow {
    fn drop(&mut self) {
        self.wheel_task.take();
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
            .child(
                img(crate::assets::asset_path("logo.png"))
                    .size(px(18.0))
                    .rounded(px(3.0)),
            )
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
                    .on_scroll(cx.listener(|this, event: &ScrollEvent, window, _cx| {
                        if !this.input_enabled {
                            return;
                        }
                        // Three desktop lines make one phone scroll unit.
                        let lines = match event.delta {
                            ScrollDelta::Lines(lines) => lines.y,
                            ScrollDelta::Pixels(pixels) => f32::from(pixels.y) / 40.0,
                        };
                        if lines == 0.0 {
                            return;
                        }
                        let (x, y) = this.phone_point(event.position, window);
                        let mut state = this.wheel.lock().unwrap_or_else(|e| e.into_inner());
                        state.budget = (state.budget + lines / 3.0).clamp(-6.0, 6.0);
                        state.x = x;
                        state.y = y;
                        state.width = this.size.0;
                        state.height = this.size.1;
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
    }
}
