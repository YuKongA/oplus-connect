use crate::kms_settings::{self, ExitDirection};
use crate::persisted::{self, ExpandCastSettings};
use crate::views::{SelectState, SettingsControls};
use crate::{NavTab, RootView};
use gpui_kit::component::IndexPath;
use gpui_kit::component::input::{InputEvent, InputState};
use gpui_kit::component::searchable_list::SearchableVec;
use gpui_kit::component::select::SelectEvent;
use gpui_kit::*;
use std::{io, sync::mpsc, thread};
use tokio::sync::mpsc as async_mpsc;

#[derive(Clone, Copy, Debug)]
enum Save {
    Tablet(ExpandCastSettings),
    Direction(ExitDirection),
    Cast {
        audio: bool,
        input: bool,
        h265: bool,
    },
}

impl Save {
    fn write(self) -> io::Result<()> {
        match self {
            Self::Tablet(settings) => persisted::save_expand_cast_settings(settings),
            Self::Direction(direction) => persisted::save_kms_direction(direction),
            Self::Cast { audio, input, h265 } => persisted::save_cast_settings(audio, input, h265),
        }
    }
}

// Both channels are FIFO. Always build the next commit from the latest requested
// snapshot, even when an earlier write is still in flight. Only successful writes
// become the settings that a new sharing session will use.
#[derive(Default)]
struct Pending<T> {
    latest: Option<T>,
    outstanding: usize,
}

impl<T: Copy> Pending<T> {
    fn enqueue(&mut self, value: T) {
        self.latest = Some(value);
        self.outstanding += 1;
    }

    fn complete(&mut self) -> bool {
        self.outstanding -= 1;
        if self.outstanding == 0 {
            self.latest = None;
        }
        self.outstanding == 0
    }
}

type SaveResult = (Save, io::Result<()>);

pub(super) struct SettingsWriter {
    sender: Option<mpsc::Sender<Save>>,
    worker: Option<thread::JoinHandle<()>>,
    tablet: Pending<ExpandCastSettings>,
    direction: Pending<ExitDirection>,
    cast_outstanding: usize,
}

impl SettingsWriter {
    pub(super) fn new(cx: &mut Context<RootView>) -> Self {
        let (writer, mut results) = Self::start(Save::write);
        cx.spawn(async move |this, app| {
            while let Some((save, result)) = results.recv().await {
                if this
                    .update(app, |view, cx| {
                        view.settings_saved(save, result);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        writer
    }

    fn start(
        mut write: impl FnMut(Save) -> io::Result<()> + Send + 'static,
    ) -> (Self, async_mpsc::UnboundedReceiver<SaveResult>) {
        let (sender, requests) = mpsc::channel();
        let (done, results) = async_mpsc::unbounded_channel();
        let worker = thread::spawn(move || {
            for save in requests {
                let result = write(save);
                if let Err(error) = &result {
                    tracing::warn!(?save, %error, "Cannot persist settings");
                }
                // Even if the GUI is already gone, drain all accepted commits.
                let _ = done.send((save, result));
            }
        });
        (
            Self {
                sender: Some(sender),
                worker: Some(worker),
                tablet: Pending::default(),
                direction: Pending::default(),
                cast_outstanding: 0,
            },
            results,
        )
    }

    fn submit(&mut self, save: Save) {
        self.sender
            .as_ref()
            .expect("settings writer is open")
            .send(save)
            .expect("settings writer stopped");
        match save {
            Save::Tablet(value) => self.tablet.enqueue(value),
            Save::Direction(value) => self.direction.enqueue(value),
            Save::Cast { .. } => self.cast_outstanding += 1,
        }
    }

    pub(super) fn desired_tablet(&self, saved: ExpandCastSettings) -> ExpandCastSettings {
        self.tablet.latest.unwrap_or(saved)
    }

    fn request_tablet(&mut self, saved: ExpandCastSettings, next: ExpandCastSettings) {
        if next != self.desired_tablet(saved) {
            self.submit(Save::Tablet(next));
        }
    }

    fn finish_tablet(
        &mut self,
        saved: &mut ExpandCastSettings,
        feedback: &mut Option<String>,
        attempted: ExpandCastSettings,
        result: io::Result<()>,
    ) {
        if result.is_ok() {
            *saved = attempted;
        }
        if self.tablet.complete() {
            *feedback = result.err().map(|e| format!("保存失败，设置未应用：{e}"));
        }
    }

    pub(super) fn finish(&mut self) {
        // Only final shutdown waits for disk. GPUI gives async quit callbacks
        // a short timeout, so don't leave the final fsync in a detached task.
        self.sender.take();
        if let Some(worker) = self.worker.take()
            && worker.join().is_err()
        {
            tracing::error!("Settings writer panicked");
        }
    }
}

impl Drop for SettingsWriter {
    fn drop(&mut self) {
        self.finish();
    }
}

#[derive(Clone, Copy)]
enum Number {
    Fps,
    Bitrate,
}

impl Number {
    fn parse(self, text: &str) -> Result<u32, &'static str> {
        let (range, message) = match self {
            Self::Fps => (
                persisted::FPS_MIN..=persisted::FPS_MAX,
                "帧率必须为 1–144 的整数，未保存",
            ),
            Self::Bitrate => (
                persisted::BITRATE_MIN..=persisted::BITRATE_MAX,
                "码率必须为 500–50000 的整数，未保存",
            ),
        };
        text.parse::<u32>()
            .ok()
            .filter(|value| range.contains(value))
            .ok_or(message)
    }

    fn value(self, s: ExpandCastSettings) -> u32 {
        match self {
            Self::Fps => s.fps,
            Self::Bitrate => s.bitrate_kbps,
        }
    }

    fn set(self, s: &mut ExpandCastSettings, value: u32) {
        match self {
            Self::Fps => s.fps = value,
            Self::Bitrate => s.bitrate_kbps = value,
        }
    }
}

struct NumberDraft {
    last_commit: Option<u32>,
    validation: Option<&'static str>,
}

impl NumberDraft {
    fn event(&mut self, field: Number, text: &str, event: &InputEvent) -> Option<u32> {
        let value = field.parse(text);
        self.validation = value.as_ref().err().copied();
        if matches!(event, InputEvent::Change) && self.last_commit != value.as_ref().ok().copied() {
            // A fresh edit can retry on blur, even when the user types the
            // same number again after a failed save.
            self.last_commit = None;
        }
        let value = value.ok()?;
        match event {
            // Explicit Enter also lets the user retry a failed write. The
            // pending/confirmed comparison downstream filters unchanged values.
            InputEvent::PressEnter { .. } => {}
            InputEvent::Blur if self.last_commit != Some(value) => {}
            _ => return None,
        }
        self.last_commit = Some(value);
        Some(value)
    }
}

pub(super) struct SettingsWindow {
    pub(super) id: WindowId,
    visible: bool,
    codec: Entity<SelectState>,
    direction: Entity<SelectState>,
    numbers: [Entity<InputState>; 2],
    drafts: [NumberDraft; 2],
    _events: [Subscription; 4],
}

impl SettingsWindow {
    pub(super) fn controls(&self, adaptive: bool) -> SettingsControls<'_> {
        SettingsControls {
            codec: &self.codec,
            kms_direction: &self.direction,
            expand_cast_fps: &self.numbers[0],
            expand_cast_bitrate: &self.numbers[1],
            adaptive,
            validation: self.drafts.each_ref().map(|draft| draft.validation),
        }
    }

    fn new(view: &RootView, window: &mut Window, cx: &mut Context<RootView>) -> Self {
        type Choice = SelectEvent<SearchableVec<&'static str>>;
        let id = window.window_handle().window_id();
        let mut select = |labels, index| {
            cx.new(|cx| {
                SelectState::new(
                    SearchableVec::new(labels),
                    Some(IndexPath::new(index)),
                    window,
                    cx,
                )
            })
        };
        let codec = select(vec!["H.264", "H.265"], usize::from(view.state.cast_h265));
        let direction = select(
            vec!["向左", "向右"],
            usize::from(view.desired_direction() == ExitDirection::Right),
        );
        let codec_events = cx.subscribe(&codec, |view, _, event: &Choice, cx| {
            let SelectEvent::Confirm(Some(codec)) = event else {
                return;
            };
            view.set_cast_h265(*codec == "H.265");
            cx.notify();
        });
        let direction_events = cx.subscribe(&direction, |view, _, event: &Choice, cx| {
            let SelectEvent::Confirm(Some(label)) = event else {
                return;
            };
            if let Some(value) = ExitDirection::from_label(label)
                && value != view.desired_direction()
            {
                view.settings_writer.submit(Save::Direction(value));
                cx.notify();
            }
        });
        let settings = view
            .settings_writer
            .desired_tablet(view.state.expand_cast_settings);
        let mut number = |field: Number| {
            let value = field.value(settings);
            let state = cx.new(|cx| InputState::new(window, cx).default_value(value.to_string()));
            let events = cx.subscribe(&state, move |view, state, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Focus) {
                    return;
                }
                let text = state.read(cx).value();
                view.number_event(field, &text, event, cx);
            });
            (
                state,
                events,
                NumberDraft {
                    last_commit: Some(value),
                    validation: None,
                },
            )
        };
        let (fps, fps_events, fps_draft) = number(Number::Fps);
        let (bitrate, bitrate_events, bitrate_draft) = number(Number::Bitrate);
        Self {
            id,
            visible: true,
            codec,
            direction,
            numbers: [fps, bitrate],
            drafts: [fps_draft, bitrate_draft],
            _events: [codec_events, direction_events, fps_events, bitrate_events],
        }
    }
}

impl RootView {
    pub(super) fn save_cast_preferences(&mut self) {
        self.settings_writer.submit(Save::Cast {
            audio: self.state.cast_audio,
            input: self.state.cast_input,
            h265: self.state.cast_h265,
        });
    }

    fn desired_direction(&self) -> ExitDirection {
        self.settings_writer
            .direction
            .latest
            .unwrap_or_else(kms_settings::direction)
    }

    fn settings_saved(&mut self, save: Save, result: io::Result<()>) {
        match save {
            Save::Tablet(value) => {
                self.settings_writer.finish_tablet(
                    &mut self.state.expand_cast_settings,
                    &mut self.state.expand_cast_settings_feedback,
                    value,
                    result,
                );
            }
            Save::Direction(value) => {
                let latest = self.settings_writer.direction.complete();
                if result.is_ok() {
                    kms_settings::set_direction(value);
                }
                if latest {
                    match result {
                        Ok(()) => self.state.set_toast("共享方向已保存，下次启动键鼠共享生效"),
                        Err(e) => self.state.set_toast(format!("保存共享方向失败：{e}")),
                    }
                }
            }
            Save::Cast { .. } => {
                self.settings_writer.cast_outstanding -= 1;
                if self.settings_writer.cast_outstanding == 0
                    && let Err(e) = result
                {
                    self.state
                        .set_toast(format!("投屏设置仅在本次运行生效，保存失败：{e}"));
                }
            }
        }
    }

    pub(super) fn prepare_settings_controls(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = window.window_handle().window_id();
        if self.settings_window.as_ref().is_some_and(|s| s.id != id) {
            self.close_settings_controls(cx);
        }
        if self.state.active_tab != NavTab::Settings {
            if self.settings_window.as_ref().is_some_and(|s| s.visible) {
                self.blur_settings_controls(cx);
            }
            return;
        }
        if self.settings_window.is_none() {
            self.settings_window = Some(SettingsWindow::new(self, window, cx));
        }
        let direction = self.desired_direction().label();
        let controls = self.settings_window.as_mut().unwrap();
        controls.visible = true;
        // Restore the confirmed selection on failure without replacing a text
        // editor, stealing focus, or emitting another selection event.
        if controls.direction.read(cx).selected_value() != Some(&direction) {
            controls.direction.update(cx, |select, cx| {
                select.set_selected_value(&direction, window, cx)
            });
        }
    }

    fn blur_settings_controls(&mut self, cx: &mut Context<Self>) {
        // Removal/quit can bypass the toolkit's blur callback. Use the actual
        // input values and the same commit gate, including Enter/blur dedup.
        for field in [Number::Fps, Number::Bitrate] {
            let Some(controls) = self.settings_window.as_mut() else {
                return;
            };
            controls.visible = false;
            let value = controls.numbers[field as usize].read(cx).value();
            self.number_event(field, &value, &InputEvent::Blur, cx);
        }
    }

    pub(super) fn close_settings_controls(&mut self, cx: &mut Context<Self>) {
        self.blur_settings_controls(cx);
        self.settings_window = None;
    }

    fn number_event(
        &mut self,
        field: Number,
        text: &str,
        event: &InputEvent,
        cx: &mut Context<Self>,
    ) {
        let Some(controls) = self.settings_window.as_mut() else {
            return;
        };
        let draft = &mut controls.drafts[field as usize];
        let old_error = draft.validation;
        if let Some(value) = draft.event(field, text, event) {
            let mut next = self
                .settings_writer
                .desired_tablet(self.state.expand_cast_settings);
            field.set(&mut next, value);
            self.settings_writer
                .request_tablet(self.state.expand_cast_settings, next);
        }
        if old_error != draft.validation {
            cx.notify();
        }
    }

    pub(super) fn update_expand_cast_adaptive(&mut self, enabled: bool, cx: &mut Context<Self>) {
        let mut next = self
            .settings_writer
            .desired_tablet(self.state.expand_cast_settings);
        next.adaptive_bitrate = enabled;
        self.settings_writer
            .request_tablet(self.state.expand_cast_settings, next);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ExpandCastSettings, InputEvent, Number, NumberDraft, Save, SettingsWriter, async_mpsc, io,
        mpsc,
    };

    #[test]
    fn validate_each_change_commit_once_on_enter_or_blur() {
        let mut draft = NumberDraft {
            last_commit: Some(30),
            validation: None,
        };
        let field = Number::Fps;
        let enter = InputEvent::PressEnter {
            secondary: false,
            shift: false,
        };
        for text in ["", "0", "145", "4.5"] {
            assert_eq!(draft.event(field, text, &InputEvent::Change), None);
            assert!(draft.validation.is_some());
            assert_eq!(draft.event(field, text, &enter), None);
        }
        for text in ["1", "14", "144"] {
            assert_eq!(draft.event(field, text, &InputEvent::Change), None);
            assert_eq!(draft.validation, None);
        }
        assert_eq!(draft.event(field, "144", &enter), Some(144));
        assert_eq!(draft.event(field, "144", &InputEvent::Blur), None);
        assert_eq!(draft.event(field, "60", &InputEvent::Blur), Some(60));
        assert_eq!(draft.event(field, "060", &InputEvent::Blur), None);
        assert_eq!(draft.event(field, "", &InputEvent::Change), None);
        assert_eq!(draft.event(field, "60", &InputEvent::Change), None);
        assert_eq!(draft.event(field, "60", &InputEvent::Blur), Some(60));
        for (text, ok) in [
            ("499", false),
            ("500", true),
            ("50000", true),
            ("50001", false),
        ] {
            assert_eq!(Number::Bitrate.parse(text).is_ok(), ok);
        }
    }

    #[test]
    fn rapid_commits_compose_and_quit_drains_after_failure() {
        let (release, blocked) = mpsc::channel();
        let mut first_write = Some(blocked);
        let (mut writer, mut results) = SettingsWriter::start(move |save| {
            if let Some(gate) = first_write.take() {
                gate.recv().unwrap();
            }
            match save {
                Save::Tablet(s) if s.fps == 45 && s.bitrate_kbps == 500 => {
                    Err(io::Error::other("full"))
                }
                _ => Ok(()),
            }
        });
        let initial = ExpandCastSettings::default();
        let first = ExpandCastSettings { fps: 45, ..initial };
        writer.request_tablet(initial, first);
        writer.request_tablet(initial, first); // Repeated Enter during a write.
        let second = ExpandCastSettings {
            bitrate_kbps: 500,
            ..writer.desired_tablet(initial)
        };
        writer.request_tablet(initial, second);
        let third = ExpandCastSettings {
            fps: 90,
            ..writer.desired_tablet(initial)
        };
        writer.request_tablet(initial, third);
        release.send(()).unwrap();
        writer.finish();
        let mut saved = initial;
        let mut feedback = None;
        for (expected, ok) in [(first, true), (second, false), (third, true)] {
            let (Save::Tablet(snapshot), result) = results.try_recv().unwrap() else {
                panic!()
            };
            assert_eq!(snapshot, expected);
            assert_eq!(result.is_ok(), ok);
            writer.finish_tablet(&mut saved, &mut feedback, snapshot, result);
            assert!(feedback.is_none());
            assert_eq!(writer.desired_tablet(saved), third);
        }
        assert_eq!(saved.bitrate_kbps, 500);
        assert!(matches!(
            results.try_recv(),
            Err(async_mpsc::error::TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn final_failure_falls_back_to_last_confirmed_snapshot() {
        let initial = ExpandCastSettings::default();
        let first = ExpandCastSettings { fps: 45, ..initial };
        let (mut writer, mut results) = SettingsWriter::start(move |save| match save {
            Save::Tablet(s) if s == initial => Err(io::Error::other("disk full")),
            _ => Ok(()),
        });
        writer.request_tablet(initial, first);
        writer.request_tablet(initial, initial); // Revert while first write is outstanding.
        writer.finish();
        let mut applied = initial;
        let mut feedback = None;
        for expected in [first, initial] {
            let (Save::Tablet(snapshot), result) = results.try_recv().unwrap() else {
                panic!()
            };
            assert_eq!(snapshot, expected);
            writer.finish_tablet(&mut applied, &mut feedback, snapshot, result);
        }
        assert_eq!(applied, first);
        assert_eq!(writer.desired_tablet(applied), first);
        assert!(feedback.unwrap().contains("disk full"));
    }
}
