//! Send text to the focused field on one device.
//!
//! The primary path is exactly the command users type by hand:
//! `adb shell input text "..."`. `input text` is ASCII-only on AOSP: a
//! non-ASCII string makes its `KeyCharacterMap.getEvents()` return null and
//! the command fails without typing anything. LogcatX therefore tries the
//! user's command first, and only on failure falls back to writing that run
//! into the device clipboard through the embedded helper and sending
//! `KEYCODE_PASTE` (279). Devices/ROMs where `input text` accepts Unicode keep
//! using it unchanged.

use crate::{adb_executor::AdbExecutor, clipboard};
use std::time::Duration;

const SHELL_TIMEOUT: Duration = Duration::from_secs(10);
/// A single adb shell command line; chunks stay far below any platform limit.
const ASCII_CHUNK_CHARS: usize = 800;
const UNICODE_CHUNK_CHARS: usize = 300;
pub const MAX_TEXT_CHARS: usize = 20_000;

const KEYCODE_ENTER: &str = "66";
const KEYCODE_TAB: &str = "61";
const KEYCODE_PASTE: &str = "279";

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TextInputOutcome {
    pub chars: usize,
    /// Number of non-ASCII runs that had to go through clipboard paste.
    pub clipboard_pastes: usize,
}

/// Type `text` into the currently focused field of `serial`.
///
/// New lines become Enter key events and tabs become Tab key events. When
/// `allow_clipboard_fallback` is true, a non-ASCII run rejected by
/// `input text` is written to the device clipboard and pasted (overwriting the
/// device clipboard with that last run).
pub fn send_text(
    adb_path: &str,
    serial: &str,
    text: &str,
    allow_clipboard_fallback: bool,
) -> Result<TextInputOutcome, String> {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    if normalized.is_empty() {
        return Ok(TextInputOutcome::default());
    }
    if normalized.chars().count() > MAX_TEXT_CHARS {
        return Err(format!(
            "Text is too long ({} characters; limit is {MAX_TEXT_CHARS}).",
            normalized.chars().count()
        ));
    }

    let executor = AdbExecutor::new(adb_path);
    let mut outcome = TextInputOutcome::default();
    for segment in split_segments(&normalized) {
        match segment {
            Segment::Newline => send_keyevent(&executor, serial, KEYCODE_ENTER)?,
            Segment::Tab => send_keyevent(&executor, serial, KEYCODE_TAB)?,
            Segment::Text(run) => {
                let ascii = run.is_ascii();
                let chunk_chars = if ascii {
                    ASCII_CHUNK_CHARS
                } else {
                    UNICODE_CHUNK_CHARS
                };
                for chunk in chunks(&run, chunk_chars) {
                    match send_text_chunk(&executor, serial, &chunk) {
                        Ok(()) => {}
                        Err(err) if !ascii => {
                            if !allow_clipboard_fallback {
                                return Err(format!(
                                    "adb input text rejected non-ASCII text and the clipboard fallback is disabled: {err}"
                                ));
                            }
                            clipboard::write_text(adb_path, serial, &chunk).map_err(|fallback| {
                                format!(
                                    "adb input text rejected this text and the clipboard fallback failed: {fallback} (input text: {err})"
                                )
                            })?;
                            send_keyevent(&executor, serial, KEYCODE_PASTE).map_err(|paste| {
                                format!(
                                    "The text was written to the device clipboard but pasting failed: {paste}"
                                )
                            })?;
                            outcome.clipboard_pastes += 1;
                        }
                        Err(err) => return Err(err),
                    }
                }
            }
        }
    }
    outcome.chars = normalized.chars().count();
    Ok(outcome)
}

enum Segment {
    Text(String),
    Newline,
    Tab,
}

fn split_segments(text: &str) -> Vec<Segment> {
    let mut segments = Vec::new();
    let mut current = String::new();
    for character in text.chars() {
        match character {
            '\n' => {
                if !current.is_empty() {
                    segments.push(Segment::Text(std::mem::take(&mut current)));
                }
                segments.push(Segment::Newline);
            }
            '\t' => {
                if !current.is_empty() {
                    segments.push(Segment::Text(std::mem::take(&mut current)));
                }
                segments.push(Segment::Tab);
            }
            _ => current.push(character),
        }
    }
    if !current.is_empty() {
        segments.push(Segment::Text(current));
    }
    segments
}

fn chunks(text: &str, max_chars: usize) -> Vec<String> {
    if max_chars == 0 {
        return vec![text.to_owned()];
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    let mut count = 0usize;
    for character in text.chars() {
        current.push(character);
        count += 1;
        if count >= max_chars {
            chunks.push(std::mem::take(&mut current));
            count = 0;
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

fn send_text_chunk(executor: &AdbExecutor, serial: &str, text: &str) -> Result<(), String> {
    let quoted = quote_input_text(text);
    let output = executor
        .execute_with_timeout(
            &["-s", serial, "shell", "input", "text", &quoted],
            SHELL_TIMEOUT,
        )
        .map_err(|err| format!("Failed to run adb input text on {serial}: {err}"))?;
    if output.success() {
        Ok(())
    } else {
        Err(format!(
            "adb input text failed on {serial}: {}",
            combined_output(&output.stdout, &output.stderr)
        ))
    }
}

fn send_keyevent(executor: &AdbExecutor, serial: &str, code: &str) -> Result<(), String> {
    let output = executor
        .execute_with_timeout(
            &["-s", serial, "shell", "input", "keyevent", code],
            SHELL_TIMEOUT,
        )
        .map_err(|err| format!("Failed to send key event {code} to {serial}: {err}"))?;
    if output.success() {
        Ok(())
    } else {
        Err(format!(
            "Failed to send key event {code} to {serial}: {}",
            combined_output(&output.stdout, &output.stderr)
        ))
    }
}

/// Quote one `input text` argument for the device shell. Double quotes match
/// the command users type by hand; the four shell-expandable characters are
/// escaped, while spaces and Unicode pass through literally.
fn quote_input_text(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('"');
    for character in text.chars() {
        if matches!(character, '"' | '\\' | '$' | '`') {
            quoted.push('\\');
        }
        quoted.push(character);
    }
    quoted.push('"');
    quoted
}

fn combined_output(stdout: &[u8], stderr: &[u8]) -> String {
    let output = String::from_utf8_lossy(stdout).trim().to_owned();
    let error = String::from_utf8_lossy(stderr).trim().to_owned();
    let combined = match (output.is_empty(), error.is_empty()) {
        (false, false) => format!("{output} {error}"),
        (false, true) => output,
        (true, false) => error,
        (true, true) => "unknown error".to_owned(),
    };
    // A failed Unicode `input text` dumps a Java stack trace; keep the status
    // message readable.
    combined.chars().take(300).collect()
}

#[cfg(test)]
mod tests {
    use super::{Segment, chunks, quote_input_text, split_segments};

    #[test]
    fn quote_preserves_spaces_and_unicode_but_escapes_shell_specials() {
        assert_eq!(quote_input_text("hello world"), "\"hello world\"");
        assert_eq!(quote_input_text("中文"), "\"中文\"");
        assert_eq!(quote_input_text(r#"a"b$c`d\e"#), "\"a\\\"b\\$c\\`d\\\\e\"");
    }

    #[test]
    fn segments_split_newlines_and_tabs_without_losing_text() {
        let segments = split_segments("a\n\nb\tc");
        assert_eq!(segments.len(), 6);
        assert!(matches!(&segments[0], Segment::Text(text) if text == "a"));
        assert!(matches!(segments[1], Segment::Newline));
        assert!(matches!(segments[2], Segment::Newline));
        assert!(matches!(&segments[3], Segment::Text(text) if text == "b"));
        assert!(matches!(segments[4], Segment::Tab));
        assert!(matches!(&segments[5], Segment::Text(text) if text == "c"));
    }

    #[test]
    fn chunks_do_not_split_utf8_characters() {
        let split = chunks("中a文b", 2);
        assert_eq!(split, vec!["中a".to_owned(), "文b".to_owned()]);
        assert_eq!(chunks("abc", 10), vec!["abc".to_owned()]);
        assert!(chunks("", 10).is_empty());
    }
}
