//! Windows-style right-click context menus for text (field-trial round 9).
//!
//! egui 0.31 text edits handle Ctrl+C/X/V but ship no context menu, and
//! Windows users expect right-click → 剪切/复制/粘贴/全选 on every input.
//! [`text_edit_menu`] attaches such a menu to any text-edit response. All
//! actions mutate the same `String` buffer the edit renders and keep egui's
//! cursor state in sync — cursor indices count characters, not bytes, so
//! every buffer edit goes through the char→byte conversion helpers below.
//!
//! Buffer edits made through the menu bypass the edit's built-in undo stack
//! (egui 0.31 offers no public undo API); Ctrl+Z will not revert them.

use crate::i18n::I18n;
use eframe::egui::{self, epaint::text::cursor::CCursor, text_selection::CCursorRange};

/// Attach cut/copy/paste/select-all to a text-edit `response` rendering
/// `buf`. Call it right after building the widget; `response.id` is the
/// edit's widget id, under which egui persists the cursor state.
pub(crate) fn text_edit_menu(response: &egui::Response, buf: &mut String, i18n: &I18n) {
    // egui 0.31 collapses the text cursor to a single point on ANY pointer
    // press (text_cursor_state.rs: pointer_interaction uses any_pressed) —
    // including the right click that opens this menu — and the collapse
    // happens inside the TextEdit build, BEFORE this function runs. So the
    // selection must be tracked continuously in persistent storage (egui's
    // `temp` store is wiped at frame end), and restored on the press frame
    // itself: the menu opens on the release frame and reads whatever the
    // cursor state holds then, which is the range restored here.
    let id = response.id;
    if secondary_press_on(response) {
        // Re-opening the menu hands truth to the visible selection; any
        // pending menu-action range is stale by definition.
        clear_pending_selection(&response.ctx, id);
        restore_selection(&response.ctx, id);
    } else {
        if primary_press_on(response) {
            // A direct press supersedes any pending menu-action selection.
            clear_pending_selection(&response.ctx, id);
        }
        apply_pending_selection(&response.ctx, id);
        let selection = cursor_char_pair(&response.ctx, id);
        response
            .ctx
            .data_mut(|d| d.insert_persisted(prior_selection_id(id), selection));
    }

    response.context_menu(|ui| {
        show_menu_items(ui, id, buf, i18n);
    });
}

/// The four menu items, extracted from the popup closure so tests can drive
/// them directly. Returns each item's response for hit-testing.
#[derive(Clone)]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct MenuItemResponses {
    pub copy: egui::Response,
    pub cut: egui::Response,
    pub paste: egui::Response,
    pub select_all: egui::Response,
}

pub(crate) fn show_menu_items(
    ui: &mut egui::Ui,
    id: egui::Id,
    buf: &mut String,
    i18n: &I18n,
) -> MenuItemResponses {
    let selection = cursor_char_pair(ui.ctx(), id);
    let has_selection = selection.is_some_and(|(a, b)| a < b);

    let copy = ui.add_enabled(has_selection, egui::Button::new(i18n.tr("menu.copy")));
    if copy.clicked() {
        if let Some((a, b)) = selection {
            ui.ctx().copy_text(char_slice(buf, a, b).to_owned());
        }
        refocus_and_close(ui, id);
    }
    let cut = ui.add_enabled(has_selection, egui::Button::new(i18n.tr("menu.cut")));
    if cut.clicked() {
        if let Some((a, b)) = selection {
            ui.ctx().copy_text(char_slice(buf, a, b).to_owned());
            replace_char_range(buf, (a, b), "");
            set_char_cursor(ui.ctx(), id, a, a);
        }
        refocus_and_close(ui, id);
    }
    // Reading the system clipboard on every open-menu frame is cheap
    // compared to creating the popup itself, and it keeps the paste
    // item's enabled state honest.
    let clipboard = read_clipboard_text();
    let paste = ui.add_enabled(
        clipboard.is_some(),
        egui::Button::new(i18n.tr("menu.paste")),
    );
    if paste.clicked() {
        if let Some(text) = clipboard {
            let (a, b) = selection.unwrap_or((0, 0));
            let cursor = a + text.chars().count();
            replace_char_range(buf, (a, b), &text);
            set_char_cursor(ui.ctx(), id, cursor, cursor);
        }
        refocus_and_close(ui, id);
    }
    let select_all = ui.button(i18n.tr("menu.select_all"));
    if select_all.clicked() {
        let end = buf.chars().count();
        set_char_cursor(ui.ctx(), id, 0, end);
        // The menu-button press surrenders the edit's focus and reclaiming
        // it costs one unfocused frame; on the following focus-transition
        // frame egui's IME guard (builder.rs: gained_focus || lost_focus)
        // collapses any range to a point — which used to wipe this very
        // selection (field-trial bug 26). Re-apply it once focus is stable.
        ui.ctx().data_mut(|d| {
            d.insert_persisted(
                pending_selection_id(id),
                Some(PendingSelection {
                    range: (0, end),
                    frames_left: PENDING_SELECTION_FRAMES,
                }),
            );
        });
        refocus_and_close(ui, id);
    }
    MenuItemResponses {
        copy,
        cut,
        paste,
        select_all,
    }
}

/// A menu click must hand keyboard focus back to the edit: pressing a menu
/// button lands outside the edit, and egui surrenders focus on any outside
/// press (context.rs) — which hides the selection (only painted when
/// focused) and strands further typing (field-trial bugs 24/25).
fn refocus_and_close(ui: &mut egui::Ui, id: egui::Id) {
    ui.ctx().memory_mut(|mem| mem.request_focus(id));
    ui.close_menu();
}

/// Persistent per-edit slot holding the last live selection (or cursor
/// point). Survives frame boundaries, unlike egui's `temp` store.
fn prior_selection_id(id: egui::Id) -> egui::Id {
    egui::Id::new(("text_menu_prior_selection", id))
}

/// Did a secondary (right) press land on this edit in the current frame?
/// The raw event position decides, so sibling inputs sharing the frame do
/// not mistake the press for their own.
fn secondary_press_on(response: &egui::Response) -> bool {
    press_on(response, egui::PointerButton::Secondary)
}

fn primary_press_on(response: &egui::Response) -> bool {
    press_on(response, egui::PointerButton::Primary)
}

fn press_on(response: &egui::Response, button: egui::PointerButton) -> bool {
    response.ctx.input(|input| {
        input.events.iter().any(|event| {
            matches!(event, egui::Event::PointerButton { pos, button: b, pressed: true, .. } if *b == button && response.rect.contains(*pos))
        })
    })
}

/// Placeholder/hint text style for text edits. The app-wide
/// `visuals.override_text_color` flattens egui's weak hint color into the
/// body text color, so every hint site must carry this explicit gray
/// (field-trial bug 23).
pub(crate) const HINT_TEXT_COLOR: egui::Color32 = egui::Color32::from_rgb(160, 160, 160);

/// Styled hint for `TextEdit::hint_text` (see [`HINT_TEXT_COLOR`]).
pub(crate) fn hint(text: impl Into<String>) -> egui::RichText {
    egui::RichText::new(text).weak().color(HINT_TEXT_COLOR)
}

/// A menu-action selection waiting for focus to stabilize. It only needs to
/// survive the surrender→refocus transition (a couple of frames), so it
/// expires: an immortal slot would resurrect a stale range when focus
/// returns much later via Tab navigation (round-11 review finding R1).
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct PendingSelection {
    range: (usize, usize),
    frames_left: u32,
}

const PENDING_SELECTION_FRAMES: u32 = 5;

/// Slot for a menu-action selection that must be re-applied once focus is
/// stable (see `show_menu_items`).
fn pending_selection_id(id: egui::Id) -> egui::Id {
    egui::Id::new(("text_menu_pending_selection", id))
}

fn clear_pending_selection(ctx: &egui::Context, id: egui::Id) {
    ctx.data_mut(|d| {
        d.insert_persisted(pending_selection_id(id), None::<PendingSelection>);
    });
}

/// Re-apply a menu-action selection after the focus-transition frame (whose
/// IME guard collapses ranges). Counts down every frame, applies once
/// focus arrives, and expires after [`PENDING_SELECTION_FRAMES`] frames.
fn apply_pending_selection(ctx: &egui::Context, id: egui::Id) {
    let Some(mut pending) = ctx
        .data_mut(|d| d.get_persisted::<Option<PendingSelection>>(pending_selection_id(id)))
        .flatten()
    else {
        return;
    };
    if pending.frames_left == 0 {
        clear_pending_selection(ctx, id);
        return;
    }
    pending.frames_left -= 1;
    if ctx.memory(|m| m.has_focus(id)) {
        set_char_cursor(ctx, id, pending.range.0, pending.range.1);
        clear_pending_selection(ctx, id);
        return;
    }
    ctx.data_mut(|d| d.insert_persisted(pending_selection_id(id), Some(pending)));
}

/// Put the tracked pre-press selection back so 复制/剪切 can read it.
fn restore_selection(ctx: &egui::Context, id: egui::Id) {
    let selection = ctx
        .data_mut(|d| d.get_persisted::<Option<(usize, usize)>>(prior_selection_id(id)))
        .flatten();
    if let Some((a, b)) = selection {
        set_char_cursor(ctx, id, a, b);
    }
}

/// The text-edit cursor as an ordered (start, end) character pair.
fn cursor_char_pair(ctx: &egui::Context, id: egui::Id) -> Option<(usize, usize)> {
    let range = egui::TextEdit::load_state(ctx, id)?.cursor.char_range()?;
    let (a, b) = (range.primary.index, range.secondary.index);
    Some((a.min(b), a.max(b)))
}

/// Point the edit's cursor at a character range (also selects it).
fn set_char_cursor(ctx: &egui::Context, id: egui::Id, a: usize, b: usize) {
    if let Some(mut state) = egui::TextEdit::load_state(ctx, id) {
        state
            .cursor
            .set_char_range(Some(CCursorRange::two(CCursor::new(a), CCursor::new(b))));
        egui::TextEdit::store_state(ctx, id, state);
    }
}

#[cfg(not(test))]
fn read_clipboard_text() -> Option<String> {
    arboard::Clipboard::new()
        .ok()
        .and_then(|mut c| c.get_text().ok())
}

/// Tests never touch the real OS clipboard: parallel harness threads
/// hammering the Win32 clipboard corrupted the heap (observed as
/// STATUS_HEAP_CORRUPTION in parallel `cargo test --lib` runs), and tests
/// mutating the user's clipboard is rude anyway. Each test thread gets an
/// isolated stand-in.
#[cfg(test)]
fn read_clipboard_text() -> Option<String> {
    TEST_CLIPBOARD.with(|c| c.borrow().clone())
}

#[cfg(test)]
thread_local! {
    static TEST_CLIPBOARD: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn set_test_clipboard(text: Option<&str>) {
    TEST_CLIPBOARD.with(|c| *c.borrow_mut() = text.map(str::to_owned));
}

/// Character range → byte bounds on `s`, clamped to the text length.
fn char_to_byte_bounds(s: &str, a: usize, b: usize) -> (usize, usize) {
    let total = s.chars().count();
    let (a, b) = (a.min(total), b.min(total));
    let mut bounds = (s.len(), s.len());
    for (index, (byte, _)) in s.char_indices().enumerate() {
        if index == a {
            bounds.0 = byte;
        }
        if index == b {
            bounds.1 = byte;
        }
    }
    bounds
}

fn char_slice(s: &str, a: usize, b: usize) -> &str {
    let (ba, bb) = char_to_byte_bounds(s, a, b);
    &s[ba..bb]
}

fn replace_char_range(s: &mut String, (a, b): (usize, usize), replacement: &str) {
    let (ba, bb) = char_to_byte_bounds(s, a, b);
    s.replace_range(ba..bb, replacement);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn char_bounds_handle_multibyte_text() {
        // 中 = 3 bytes, 文 = 3 bytes, then ascii.
        let s = "中文ab";
        assert_eq!(char_to_byte_bounds(s, 0, 2), (0, 6));
        assert_eq!(char_to_byte_bounds(s, 1, 3), (3, 7));
        assert_eq!(char_to_byte_bounds(s, 4, 9), (s.len(), s.len()));
        assert_eq!(char_slice(s, 0, 2), "中文");
        assert_eq!(char_slice(s, 2, 4), "ab");
    }

    #[test]
    fn replace_char_range_splices_characters_not_bytes() {
        let mut s = "中文ab".to_owned();
        replace_char_range(&mut s, (1, 3), "x");
        assert_eq!(s, "中xb");
        replace_char_range(&mut s, (0, 1), "");
        assert_eq!(s, "xb");
    }

    #[test]
    fn replace_out_of_range_clamps_to_text_end() {
        let mut s = "ab".to_owned();
        replace_char_range(&mut s, (1, 99), "c");
        assert_eq!(s, "ac");
    }

    #[test]
    fn secondary_press_restores_selection_across_frames() {
        let ctx = egui::Context::default();
        let i18n = I18n::new("en");
        let mut buf = "hello world".to_owned();
        let edit_rect = std::sync::Arc::new(std::sync::Mutex::new(egui::Rect::NOTHING));
        let edit_id = std::sync::Arc::new(std::sync::Mutex::new(egui::Id::NULL));

        let mut run_frame = |events: Vec<egui::Event>, time: f64| {
            let rect_cell = edit_rect.clone();
            let id_cell = edit_id.clone();
            let input = egui::RawInput {
                time: Some(time),
                events,
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 600.0),
                )),
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let response = ui.add(egui::TextEdit::singleline(&mut buf));
                    *rect_cell.lock().unwrap() = response.rect;
                    *id_cell.lock().unwrap() = response.id;
                    text_edit_menu(&response, &mut buf, &i18n);
                });
            });
        };

        // F1: plain frame; the menu code caches the (empty) cursor state.
        run_frame(vec![], 0.0);
        let id = *edit_id.lock().unwrap();
        // Select "hello" outside any frame; the next frame syncs the cache.
        set_char_cursor(&ctx, id, 0, 5);
        run_frame(vec![], 0.1);

        // F3: right press lands on the edit. The widget collapses the
        // cursor while building; the menu code must restore (0, 5).
        let pos = edit_rect.lock().unwrap().center();
        run_frame(
            vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Secondary,
                    pressed: true,
                    modifiers: egui::Modifiers::default(),
                },
            ],
            0.2,
        );
        assert_eq!(
            cursor_char_pair(&ctx, id),
            Some((0, 5)),
            "right press must not drop the selection"
        );

        // F4: release — the frame the menu actually opens on. It reads the
        // cursor state, which must still hold the restored selection.
        run_frame(
            vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Secondary,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            }],
            0.3,
        );
        assert_eq!(
            cursor_char_pair(&ctx, id),
            Some((0, 5)),
            "menu-open frame must still see the selection"
        );
    }

    #[test]
    fn primary_press_resyncs_cache_so_later_right_click_revives_nothing() {
        let ctx = egui::Context::default();
        let i18n = I18n::new("en");
        let mut buf = "hello world".to_owned();
        let edit_rect = std::sync::Arc::new(std::sync::Mutex::new(egui::Rect::NOTHING));
        let edit_id = std::sync::Arc::new(std::sync::Mutex::new(egui::Id::NULL));

        let mut run_frame = |events: Vec<egui::Event>, time: f64| {
            let rect_cell = edit_rect.clone();
            let id_cell = edit_id.clone();
            let input = egui::RawInput {
                time: Some(time),
                events,
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 600.0),
                )),
                ..Default::default()
            };
            let _ = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let response = ui.add(egui::TextEdit::singleline(&mut buf));
                    *rect_cell.lock().unwrap() = response.rect;
                    *id_cell.lock().unwrap() = response.id;
                    text_edit_menu(&response, &mut buf, &i18n);
                });
            });
        };

        run_frame(vec![], 0.0);
        let id = *edit_id.lock().unwrap();
        set_char_cursor(&ctx, id, 0, 5);
        run_frame(vec![], 0.1);

        // Left press collapses the selection to a point; the cache follows.
        let pos = edit_rect.lock().unwrap().center();
        run_frame(
            vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::default(),
                },
            ],
            0.2,
        );
        run_frame(
            vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            }],
            0.3,
        );

        // A later right press must revive only the point, not (0, 5).
        run_frame(
            vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Secondary,
                    pressed: true,
                    modifiers: egui::Modifiers::default(),
                },
            ],
            0.4,
        );
        let pair = cursor_char_pair(&ctx, id).expect("cursor state exists");
        assert_eq!(
            pair.0, pair.1,
            "a left click collapsed the selection; right click must not resurrect it"
        );
    }

    /// Drives a TextEdit plus the menu items rendered right below it, so
    /// tests can click the real buttons and observe focus/selection state
    /// across frames.
    struct MenuHarness {
        ctx: egui::Context,
        buf: String,
        i18n: I18n,
        edit_id: egui::Id,
        edit_rect: egui::Rect,
        items: Option<MenuItemResponses>,
        time: f64,
    }

    impl MenuHarness {
        fn new(text: &str) -> Self {
            Self {
                ctx: egui::Context::default(),
                buf: text.to_owned(),
                i18n: I18n::new("en"),
                edit_id: egui::Id::NULL,
                edit_rect: egui::Rect::NOTHING,
                items: None,
                time: 0.0,
            }
        }

        fn frame(&mut self, events: Vec<egui::Event>) {
            self.time += 0.05;
            let mut buf = std::mem::take(&mut self.buf);
            let mut edit_id = self.edit_id;
            let mut edit_rect = self.edit_rect;
            let mut items = None;
            let i18n = &self.i18n;
            let input = egui::RawInput {
                time: Some(self.time),
                events,
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(800.0, 600.0),
                )),
                ..Default::default()
            };
            let _ = self.ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    let response = ui.add(egui::TextEdit::singleline(&mut buf));
                    edit_id = response.id;
                    edit_rect = response.rect;
                    text_edit_menu(&response, &mut buf, i18n);
                    items = Some(show_menu_items(ui, response.id, &mut buf, i18n));
                });
            });
            self.buf = buf;
            self.edit_id = edit_id;
            self.edit_rect = edit_rect;
            self.items = items;
        }

        fn click(&mut self, rect: egui::Rect) {
            let pos = rect.center();
            self.frame(vec![
                egui::Event::PointerMoved(pos),
                egui::Event::PointerButton {
                    pos,
                    button: egui::PointerButton::Primary,
                    pressed: true,
                    modifiers: egui::Modifiers::default(),
                },
            ]);
            self.frame(vec![egui::Event::PointerButton {
                pos,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            }]);
        }

        fn selection(&self) -> Option<(usize, usize)> {
            cursor_char_pair(&self.ctx, self.edit_id)
        }

        fn has_focus(&self) -> bool {
            self.ctx.memory(|m| m.has_focus(self.edit_id))
        }

        fn item(&self, pick: impl Fn(&MenuItemResponses) -> &egui::Response) -> egui::Rect {
            pick(self.items.as_ref().expect("items rendered")).rect
        }
    }

    #[test]
    fn menu_actions_reclaim_focus_and_select_all_keeps_its_selection() {
        let mut h = MenuHarness::new("hello world");
        h.frame(vec![]);
        // Focus the edit first (as if the user was typing).
        h.click(h.edit_rect);
        assert!(h.has_focus());

        // 全选 surrenders focus (the press lands outside the edit); the
        // action must reclaim it, and the selection must survive the
        // focus-transition frame (field-trial bugs 24/25).
        let select_all = h.item(|i| &i.select_all);
        h.click(select_all);
        h.frame(vec![]);
        assert!(
            h.has_focus(),
            "menu action must hand keyboard focus back to the edit"
        );
        assert_eq!(h.selection(), Some((0, 11)));

        // 复制 keeps focus too.
        let copy = h.item(|i| &i.copy);
        h.click(copy);
        assert!(h.has_focus());
    }

    #[test]
    fn select_all_survives_ime_focus_transition_and_full_cut_empties_buffer() {
        let mut h = MenuHarness::new("hello world");
        h.frame(vec![]);
        h.click(h.edit_rect);
        // Simulate an IME user (Sogou etc.): once ImeEvent::Enabled arrived,
        // egui's IME guard collapses ranges on focus-transition frames.
        h.frame(vec![egui::Event::Ime(egui::ImeEvent::Enabled)]);

        h.click(h.item(|i| &i.select_all));
        h.frame(vec![]); // gained-focus frame: the IME guard fires here
        assert_eq!(
            h.selection(),
            Some((0, 11)),
            "IME focus-transition must not wipe the select-all range (bug 26)"
        );

        // The user's bug-26 flow end to end: full selection via the menu,
        // then 剪切 must actually delete everything.
        h.click(h.item(|i| &i.cut));
        assert!(
            h.buf.is_empty(),
            "cut on a full selection must empty the buffer"
        );
        assert!(h.has_focus());
    }

    #[test]
    fn cut_partial_selection_splices_the_buffer() {
        let mut h = MenuHarness::new("hello world");
        h.frame(vec![]);
        set_char_cursor(&h.ctx, h.edit_id, 0, 5);
        h.frame(vec![]);
        h.click(h.item(|i| &i.cut));
        assert_eq!(h.buf, " world");
        assert!(h.has_focus());
    }

    #[test]
    fn paste_click_keeps_focus_and_inserts_at_the_cursor() {
        set_test_clipboard(Some("PASTED"));
        let mut h = MenuHarness::new("hello world");
        h.frame(vec![]);
        set_char_cursor(&h.ctx, h.edit_id, 5, 5);
        h.frame(vec![]);
        h.click(h.item(|i| &i.paste));
        assert_eq!(h.buf, "helloPASTED world");
        assert!(h.has_focus(), "paste must keep focus (bug 24)");
    }

    #[test]
    fn pending_selection_expires_instead_of_resurrecting_later() {
        let mut h = MenuHarness::new("hello world");
        h.frame(vec![]);
        // Seed a pending select-all as if the menu action had just run.
        h.ctx.data_mut(|d| {
            d.insert_persisted(
                pending_selection_id(h.edit_id),
                Some(PendingSelection {
                    range: (0, 11),
                    frames_left: PENDING_SELECTION_FRAMES,
                }),
            );
        });

        // Focus never arrives; frames tick past the expiry window.
        for _ in 0..(PENDING_SELECTION_FRAMES + 2) {
            h.frame(vec![]);
        }
        assert!(
            h.ctx
                .data_mut(|d| {
                    d.get_persisted::<Option<PendingSelection>>(pending_selection_id(h.edit_id))
                })
                .flatten()
                .is_none(),
            "pending selection must expire"
        );

        // Focus arriving much later (Tab navigation) must not re-select.
        h.ctx.memory_mut(|m| m.request_focus(h.edit_id));
        h.frame(vec![]);
        let pair = h.selection().expect("cursor state exists");
        assert_eq!(
            pair.0, pair.1,
            "expired pending must not resurrect the range on late focus"
        );
    }
}
