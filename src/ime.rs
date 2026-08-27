//! Workaround for IME `Enter` handling in egui 0.31 single-line text edits.
//!
//! On Windows, pressing `Enter` while an IME composition is open (for example
//! Sogou in Chinese mode, where `Enter` commits the raw pinyin as English
//! text) delivers several events to egui in one frame:
//!
//! 1. the `Enter` keydown (the IME never hides it from the window),
//! 2. an empty preedit clearing the inline composition text,
//! 3. the commit string.
//!
//! egui 0.31's single-line `TextEdit` reacts to the `Enter` keydown by
//! surrendering focus and skipping the rest of the frame's events
//! (`Event::Key` arm in `widgets/text_edit/builder.rs`). Depending on the
//! event ordering this also drops the committed text (the empty preedit
//! wipes the inline composition while the stale `ime_cursor_range` guard
//! turns the following `ImeEvent::Commit` into a no-op). The visible effect
//! is the reported bug: the field loses focus and, with IMEs that do not
//! keep an inline preedit, the committed text never lands.
//!
//! [`ImeEnterGuard`] runs once per frame, before any widget is shown, and
//! rewrites those events so egui never sees the conflicting key:
//!
//! * `Enter` presses that belong to an IME commit (or arrive while a
//!   composition is open) are removed,
//! * in frames containing a commit, the empty preedit is dropped and the
//!   commit is turned into a plain [`egui::Event::Text`], which the text
//!   edit inserts reliably regardless of its IME bookkeeping.
//!
//! Everything else passes through untouched, so regular `Enter` handling
//! (e.g. submitting the connect dialog via `lost_focus() && key_pressed`)
//! keeps working.
//!
//! The guard is global for the frame, so every single-line field in the app
//! is covered: the alias dialog, the logcat-args dialog, the connect dialog,
//! and the settings-page adb path and log directory inputs.

use eframe::egui::{Context, Event, ImeEvent, Key};

/// Tracks IME composition state across frames.
#[derive(Debug, Default)]
pub struct ImeEnterGuard {
    /// Whether the previous frame ended with an open (non-empty) preedit.
    composing: bool,
}

impl ImeEnterGuard {
    /// Call once at the top of `App::update`, before any widget is shown.
    pub fn frame(&mut self, ctx: &Context) {
        let mut composing = self.composing;
        ctx.input_mut(|i| {
            // An IME can end a composition without winit reporting it when
            // the window loses focus (e.g. alt-tab). A stuck `composing`
            // flag would keep swallowing real Enter presses afterwards.
            if !i.focused {
                composing = false;
            }
            transform_events(&mut i.events, &mut composing);
        });
        self.composing = composing;
    }
}

/// Rewrite a frame's input events in place, and update `composing` from the
/// IME events seen.
fn transform_events(events: &mut Vec<Event>, composing: &mut bool) {
    let composing_at_frame_start = *composing;
    let mut commit_seen = false;

    for event in events.iter() {
        match event {
            Event::Ime(ImeEvent::Preedit(text)) => *composing = !text.is_empty(),
            Event::Ime(ImeEvent::Commit(_)) => {
                *composing = false;
                commit_seen = true;
            }
            Event::Ime(ImeEvent::Disabled) => *composing = false,
            _ => {}
        }
    }

    if !commit_seen && !composing_at_frame_start {
        return;
    }

    if commit_seen {
        // Replace commits with plain text inserts: the text edit's IME
        // bookkeeping cannot be trusted to insert the committed string once
        // an empty preedit (or a stray key press) got processed first. Drop
        // the empty preedit so it does not wipe the composition text before
        // the insert runs.
        let mut rewritten = Vec::with_capacity(events.len());
        for event in std::mem::take(events) {
            match event {
                Event::Ime(ImeEvent::Preedit(ref text)) if text.is_empty() => {}
                Event::Ime(ImeEvent::Commit(text)) => {
                    if !text.is_empty() && text != "\n" && text != "\r" {
                        rewritten.push(Event::Text(text));
                    }
                }
                other => rewritten.push(other),
            }
        }
        *events = rewritten;
    }

    if commit_seen || composing_at_frame_start {
        // This Enter went to the IME (it either triggered the commit seen in
        // this frame or was pressed while a composition was open), so it must
        // not reach the text edit.
        events.retain(|event| {
            !matches!(
                event,
                Event::Key {
                    key: Key::Enter,
                    pressed: true,
                    ..
                }
            )
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eframe::egui;
    use egui::{PointerButton, Pos2};

    fn key(key: Key) -> Event {
        Event::Key {
            key,
            physical_key: Some(key),
            pressed: true,
            repeat: false,
            modifiers: Default::default(),
        }
    }

    #[derive(Default)]
    struct FrameOut {
        focused: bool,
        lost_focus: bool,
        enter_pressed: bool,
        rect: Option<egui::Rect>,
    }

    fn run_frame(
        ctx: &egui::Context,
        guard: &mut Option<ImeEnterGuard>,
        text: &mut String,
        events: Vec<Event>,
    ) -> FrameOut {
        let mut input = egui::RawInput::default();
        input.events = events;
        run_frame_raw(ctx, guard, text, input)
    }

    fn run_frame_raw(
        ctx: &egui::Context,
        guard: &mut Option<ImeEnterGuard>,
        text: &mut String,
        input: egui::RawInput,
    ) -> FrameOut {
        let mut out = FrameOut::default();
        ctx.run(input, |ctx| {
            if let Some(guard) = guard.as_mut() {
                guard.frame(ctx);
            }
            egui::CentralPanel::default().show(ctx, |ui| {
                let response =
                    ui.add(egui::TextEdit::singleline(text).id(egui::Id::new("test_edit")));
                out.focused = response.has_focus();
                out.lost_focus = response.lost_focus();
                out.rect = Some(response.rect);
                out.enter_pressed = ui.input(|i| i.key_pressed(Key::Enter));
            });
        });
        out
    }

    fn click(pos: Pos2) -> Vec<Event> {
        vec![
            Event::PointerMoved(pos),
            Event::PointerButton {
                pos,
                button: PointerButton::Primary,
                pressed: true,
                modifiers: Default::default(),
            },
            Event::PointerButton {
                pos,
                button: PointerButton::Primary,
                pressed: false,
                modifiers: Default::default(),
            },
        ]
    }

    // The Sogou "type pinyin, press Enter to commit raw text" stream as egui
    // receives it on Windows: Enter keydown first, then the empty preedit
    // that clears the composition, the commit, and the IME disable.
    fn enter_commit_frame(commit: &str) -> Vec<Event> {
        vec![
            key(Key::Enter),
            Event::Ime(ImeEvent::Preedit(String::new())),
            Event::Ime(ImeEvent::Commit(commit.to_owned())),
            Event::Ime(ImeEvent::Disabled),
        ]
    }

    fn focused_field_with_preedit(
        ctx: &egui::Context,
        guard: &mut Option<ImeEnterGuard>,
        text: &mut String,
    ) {
        let rect = run_frame(ctx, guard, text, Vec::new()).rect.unwrap();
        let click_out = run_frame(ctx, guard, text, click(rect.center()));
        assert!(click_out.focused, "click should focus the text edit");
        run_frame(
            ctx,
            guard,
            text,
            vec![
                Event::Ime(ImeEvent::Enabled),
                Event::Ime(ImeEvent::Preedit("nihao".to_owned())),
            ],
        );
    }

    #[test]
    fn egui_031_loses_focus_on_enter_commit_without_guard() {
        let ctx = egui::Context::default();
        let mut guard = None;
        let mut text = String::new();
        focused_field_with_preedit(&ctx, &mut guard, &mut text);
        assert_eq!(text, "nihao");

        let out = run_frame(&ctx, &mut guard, &mut text, enter_commit_frame("nihao"));

        // Without the guard egui 0.31 surrenders focus on the Enter that the
        // IME already consumed, which is the reported bug.
        assert!(!out.focused);
    }

    #[test]
    fn egui_031_drops_char_commit_after_enter_without_guard() {
        // IMEs that deliver the committed string as character input (WM_CHAR)
        // after the Enter keydown lose the text as well: the unguarded Enter
        // surrenders focus and breaks out of the event loop before the
        // characters are inserted.
        let ctx = egui::Context::default();
        let mut guard = None;
        let mut text = String::new();
        let rect = run_frame(&ctx, &mut guard, &mut text, Vec::new())
            .rect
            .unwrap();
        assert!(run_frame(&ctx, &mut guard, &mut text, click(rect.center())).focused);
        run_frame(
            &ctx,
            &mut guard,
            &mut text,
            vec![Event::Ime(ImeEvent::Enabled)],
        );

        let out = run_frame(
            &ctx,
            &mut guard,
            &mut text,
            vec![key(Key::Enter), Event::Text("nihao".to_owned())],
        );

        assert!(!out.focused);
        assert_eq!(text, "");
    }

    #[test]
    fn guard_restores_char_commit_after_enter() {
        let ctx = egui::Context::default();
        let mut guard = Some(ImeEnterGuard::default());
        let mut text = String::new();
        focused_field_with_preedit(&ctx, &mut guard, &mut text);

        let out = run_frame(
            &ctx,
            &mut guard,
            &mut text,
            vec![key(Key::Enter), Event::Text("nihao".to_owned())],
        );

        assert!(out.focused);
        assert_eq!(text, "nihao");
    }

    #[test]
    fn guard_restores_commit_without_inline_preedit() {
        let ctx = egui::Context::default();
        let mut guard = Some(ImeEnterGuard::default());
        let mut text = String::new();
        let rect = run_frame(&ctx, &mut guard, &mut text, Vec::new())
            .rect
            .unwrap();
        assert!(run_frame(&ctx, &mut guard, &mut text, click(rect.center())).focused);
        run_frame(
            &ctx,
            &mut guard,
            &mut text,
            vec![Event::Ime(ImeEvent::Enabled)],
        );

        let out = run_frame(
            &ctx,
            &mut guard,
            &mut text,
            vec![
                key(Key::Enter),
                Event::Ime(ImeEvent::Commit("nihao".to_owned())),
            ],
        );

        assert!(out.focused);
        assert_eq!(text, "nihao");
    }

    #[test]
    fn guard_keeps_focus_and_committed_text() {
        let ctx = egui::Context::default();
        let mut guard = Some(ImeEnterGuard::default());
        let mut text = String::new();
        focused_field_with_preedit(&ctx, &mut guard, &mut text);

        let out = run_frame(&ctx, &mut guard, &mut text, enter_commit_frame("nihao"));

        assert!(out.focused, "field must keep focus after IME commit");
        assert!(
            !out.enter_pressed,
            "IME Enter must not be visible to the UI"
        );
        assert_eq!(text, "nihao");

        // Chinese candidate commits (no leading Enter keydown) work too.
        let out = run_frame(
            &ctx,
            &mut guard,
            &mut text,
            vec![
                Event::Ime(ImeEvent::Preedit("nihao".to_owned())),
                Event::Ime(ImeEvent::Commit("你好".to_owned())),
                Event::Ime(ImeEvent::Disabled),
            ],
        );
        assert!(out.focused);
        // The preedit inserted the raw pinyin first, the commit replaces it.
        assert_eq!(text, "nihao你好");
    }

    #[test]
    fn guard_swallows_enter_while_composition_open() {
        // Cross-frame safety: the Enter arrives while the preedit is still
        // open, the commit lands in a later frame.
        let ctx = egui::Context::default();
        let mut guard = Some(ImeEnterGuard::default());
        let mut text = String::new();
        focused_field_with_preedit(&ctx, &mut guard, &mut text);

        let out = run_frame(&ctx, &mut guard, &mut text, vec![key(Key::Enter)]);
        assert!(out.focused, "Enter during composition must not drop focus");
        assert!(!out.enter_pressed);

        let out = run_frame(
            &ctx,
            &mut guard,
            &mut text,
            vec![
                Event::Ime(ImeEvent::Preedit(String::new())),
                Event::Ime(ImeEvent::Commit("nihao".to_owned())),
                Event::Ime(ImeEvent::Disabled),
            ],
        );
        assert!(out.focused);
        assert_eq!(text, "nihao");
    }

    #[test]
    fn plain_enter_still_submits_connect_dialog() {
        // The connect dialog submits on `lost_focus() && key_pressed(Enter)`,
        // the only app-level Enter consumer. Verify the guard keeps that
        // idiom intact while blocking the IME variant of the same key.
        let ctx = egui::Context::default();
        let mut guard = Some(ImeEnterGuard::default());
        let mut text = String::new();
        focused_field_with_preedit(&ctx, &mut guard, &mut text);

        let out = run_frame(&ctx, &mut guard, &mut text, enter_commit_frame("192.168"));
        assert!(
            !(out.lost_focus && out.enter_pressed),
            "IME Enter must not submit"
        );

        // A real Enter outside any composition must pass through untouched.
        let out = run_frame(&ctx, &mut guard, &mut text, vec![key(Key::Enter)]);
        assert!(
            out.lost_focus && out.enter_pressed,
            "real Enter must submit"
        );
    }

    #[test]
    fn composing_resets_when_viewport_unfocused() {
        // A composition left open when the window loses focus must not keep
        // swallowing Enter after the window regains focus.
        let ctx = egui::Context::default();
        let mut guard = Some(ImeEnterGuard::default());
        let mut text = String::new();
        focused_field_with_preedit(&ctx, &mut guard, &mut text);

        let mut input = egui::RawInput::default();
        input.focused = false;
        run_frame_raw(&ctx, &mut guard, &mut text, input);

        let out = run_frame(&ctx, &mut guard, &mut text, vec![key(Key::Enter)]);
        assert!(out.enter_pressed, "Enter must pass after focus reset");
        assert!(!out.focused);
    }

    #[test]
    fn transform_events_unit() {
        // No IME activity: events pass through unchanged.
        let mut composing = false;
        let mut events = vec![key(Key::Enter), Event::Text("x".to_owned())];
        transform_events(&mut events, &mut composing);
        assert_eq!(events.len(), 2);
        assert!(matches!(
            events[0],
            Event::Key {
                key: Key::Enter,
                pressed: true,
                ..
            }
        ));

        // Commit frame: empty preedit dropped, commit becomes text, Enter
        // removed, everything else kept in order.
        let mut composing = false;
        let mut events = vec![
            key(Key::Enter),
            Event::Ime(ImeEvent::Preedit(String::new())),
            Event::Ime(ImeEvent::Commit("hi".to_owned())),
            Event::Ime(ImeEvent::Disabled),
        ];
        transform_events(&mut events, &mut composing);
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], Event::Text(t) if t == "hi"));
        assert!(matches!(events[1], Event::Ime(ImeEvent::Disabled)));

        // Newline/empty commits are dropped instead of becoming text.
        let mut composing = false;
        let mut events = vec![Event::Ime(ImeEvent::Commit("\n".to_owned()))];
        transform_events(&mut events, &mut composing);
        assert!(events.is_empty());

        // Composition open without a commit: only the Enter is swallowed.
        let mut composing = true;
        let mut events = vec![
            key(Key::Enter),
            Event::Ime(ImeEvent::Preedit("ni".to_owned())),
            Event::Text("x".to_owned()),
        ];
        transform_events(&mut events, &mut composing);
        assert_eq!(events.len(), 2);
        assert!(matches!(&events[0], Event::Ime(ImeEvent::Preedit(t)) if t == "ni"));
        assert!(matches!(&events[1], Event::Text(_)));
    }
}
