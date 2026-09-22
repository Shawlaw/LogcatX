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
    response.context_menu(|ui| {
        let id = response.id;
        let selection = cursor_char_pair(ui.ctx(), id);
        let has_selection = selection.is_some_and(|(a, b)| a < b);

        if ui
            .add_enabled(has_selection, egui::Button::new(i18n.tr("menu.copy")))
            .clicked()
        {
            if let Some((a, b)) = selection {
                ui.ctx().copy_text(char_slice(buf, a, b).to_owned());
            }
            ui.close_menu();
        }
        if ui
            .add_enabled(has_selection, egui::Button::new(i18n.tr("menu.cut")))
            .clicked()
        {
            if let Some((a, b)) = selection {
                ui.ctx().copy_text(char_slice(buf, a, b).to_owned());
                replace_char_range(buf, (a, b), "");
                set_char_cursor(ui.ctx(), id, a, a);
            }
            ui.close_menu();
        }
        // Reading the system clipboard on every open-menu frame is cheap
        // compared to creating the popup itself, and it keeps the paste
        // item's enabled state honest.
        let clipboard = read_clipboard_text();
        if ui
            .add_enabled(
                clipboard.is_some(),
                egui::Button::new(i18n.tr("menu.paste")),
            )
            .clicked()
        {
            if let Some(text) = clipboard {
                let (a, b) = selection.unwrap_or((0, 0));
                let cursor = a + text.chars().count();
                replace_char_range(buf, (a, b), &text);
                set_char_cursor(ui.ctx(), id, cursor, cursor);
            }
            ui.close_menu();
        }
        if ui.button(i18n.tr("menu.select_all")).clicked() {
            let end = buf.chars().count();
            set_char_cursor(ui.ctx(), id, 0, end);
            ui.close_menu();
        }
    });
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

fn read_clipboard_text() -> Option<String> {
    arboard::Clipboard::new()
        .ok()
        .and_then(|mut c| c.get_text().ok())
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
}
