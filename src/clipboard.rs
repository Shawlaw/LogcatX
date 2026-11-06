//! Manual device-clipboard bridge.
//!
//! Android 10+ does not let an ordinary background app read the clipboard, but
//! the shell UID still may. LogcatX embeds a small dex (see
//! `assets/clipboard-helper/`) and runs it as `shell` through `app_process`.
//! Each action pushes the dex, runs one short process, and parses one line:
//! `CLIP <base64>`, `EMPTY`, `ACK`, or `ERROR <message>`.

use crate::adb_executor::{AdbExecutor, ExecOptions};
use std::{fs, path::PathBuf, time::Duration};

const HELPER_DEX: &[u8] = include_bytes!("../assets/clipboard-helper.dex");
const HELPER_DEVICE_PATH: &str = "/data/local/tmp/logcatx-clipboard.dex";
const PUSH_TIMEOUT: Duration = Duration::from_secs(20);
const RUN_TIMEOUT: Duration = Duration::from_secs(20);
/// Binder transactions and the device shell both dislike unbounded arguments;
/// this is far above any normal clipboard text and keeps failures predictable.
pub const MAX_TEXT_BYTES: usize = 256 * 1024;

/// Read the device's text clipboard. `None` means the clipboard is empty or
/// contains no text (for example an image-only clip).
pub fn read_text(adb_path: &str, serial: &str) -> Result<Option<String>, String> {
    push_helper(adb_path, serial)?;
    let output = run_helper(adb_path, serial, "get", None)?;
    match parse_helper_output(&output)? {
        HelperReply::Clip(text) => Ok(Some(text)),
        HelperReply::Empty => Ok(None),
        HelperReply::Ack => {
            Err("The clipboard helper unexpectedly acknowledged a read.".to_owned())
        }
    }
}

/// Write text into the device clipboard. The user still decides when to paste.
pub fn write_text(adb_path: &str, serial: &str, text: &str) -> Result<(), String> {
    if text.len() > MAX_TEXT_BYTES {
        return Err(format!(
            "Clipboard text is too large ({} bytes; limit is {MAX_TEXT_BYTES}).",
            text.len()
        ));
    }
    push_helper(adb_path, serial)?;
    let encoded = encode_base64(text.as_bytes());
    // Windows command lines are capped at 32K; keep small writes on the
    // original argument path and pipe larger text through adb shell stdin.
    let output = if encoded.len() <= 8 * 1024 {
        run_helper(adb_path, serial, &format!("set {encoded}"), None)?
    } else {
        run_helper(adb_path, serial, "set-stdin", Some(encoded))?
    };
    match parse_helper_output(&output)? {
        HelperReply::Ack => Ok(()),
        HelperReply::Empty | HelperReply::Clip(_) => {
            Err("The clipboard helper unexpectedly returned clipboard text.".to_owned())
        }
    }
}

fn push_helper(adb_path: &str, serial: &str) -> Result<(), String> {
    let path = temporary_dex_path();
    fs::write(&path, HELPER_DEX)
        .map_err(|err| format!("Failed to stage the clipboard helper: {err}"))?;
    let local = path.to_string_lossy().into_owned();
    let result = AdbExecutor::new(adb_path).execute_with_timeout(
        &["-s", serial, "push", &local, HELPER_DEVICE_PATH],
        PUSH_TIMEOUT,
    );
    let _ = fs::remove_file(&path);

    let output = result.map_err(|err| format!("Failed to push the clipboard helper: {err}"))?;
    if output.success() {
        Ok(())
    } else {
        Err(format!(
            "Failed to push the clipboard helper to {serial}: {}",
            combined_output(&output.stdout, &output.stderr)
        ))
    }
}

fn run_helper(
    adb_path: &str,
    serial: &str,
    args: &str,
    stdin_line: Option<String>,
) -> Result<String, String> {
    let command = format!(
        "CLASSPATH={HELPER_DEVICE_PATH} app_process / com.logcatx.clipboard.ClipboardBridge {args}"
    );
    let output = AdbExecutor::new(adb_path)
        .execute_with_options(
            &["-s", serial, "shell", &command],
            ExecOptions {
                timeout: Some(RUN_TIMEOUT),
                stdout_limit: Some(2 * 1024 * 1024),
                stdin_line,
                ..Default::default()
            },
        )
        .map_err(|err| format!("Failed to run the clipboard helper on {serial}: {err}"))?;

    if !output.success() {
        return Err(format!(
            "The clipboard helper failed on {serial}: {}",
            combined_output(&output.stdout, &output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

enum HelperReply {
    Clip(String),
    Empty,
    Ack,
}

fn parse_helper_output(output: &str) -> Result<HelperReply, String> {
    let line = output
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .ok_or_else(|| "The clipboard helper returned no output.".to_owned())?;

    if line == "EMPTY" {
        return Ok(HelperReply::Empty);
    }
    if line == "ACK" {
        return Ok(HelperReply::Ack);
    }
    if let Some(encoded) = line.strip_prefix("CLIP ") {
        let bytes = decode_base64(encoded)?;
        let text = String::from_utf8(bytes)
            .map_err(|err| format!("The device clipboard is not valid UTF-8: {err}"))?;
        return Ok(HelperReply::Clip(text));
    }
    if let Some(message) = line.strip_prefix("ERROR ") {
        return Err(format!("The clipboard helper failed: {message}"));
    }
    Err(format!("Unexpected clipboard helper output: {line}"))
}

fn temporary_dex_path() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    std::env::temp_dir().join(format!(
        "logcatx-clipboard-{}-{nanos}.dex",
        std::process::id()
    ))
}

fn combined_output(stdout: &[u8], stderr: &[u8]) -> String {
    let output = String::from_utf8_lossy(stdout).trim().to_owned();
    let error = String::from_utf8_lossy(stderr).trim().to_owned();
    match (output.is_empty(), error.is_empty()) {
        (false, false) => format!("{output}\n{error}"),
        (false, true) => output,
        (true, false) => error,
        (true, true) => "unknown error".to_owned(),
    }
}

const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn encode_base64(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        output.push(BASE64_ALPHABET[(first >> 2) as usize] as char);
        output.push(BASE64_ALPHABET[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
        if chunk.len() > 1 {
            output.push(BASE64_ALPHABET[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char);
        } else {
            output.push('=');
        }
        if chunk.len() > 2 {
            output.push(BASE64_ALPHABET[(third & 0x3f) as usize] as char);
        } else {
            output.push('=');
        }
    }
    output
}

fn decode_base64(encoded: &str) -> Result<Vec<u8>, String> {
    let mut cleaned: Vec<u8> = encoded
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    if cleaned.len() % 4 == 1 {
        return Err("Invalid base64 length.".to_owned());
    }
    while !cleaned.len().is_multiple_of(4) {
        cleaned.push(b'=');
    }

    let mut output = Vec::with_capacity(cleaned.len() / 4 * 3);
    for (index, chunk) in cleaned.chunks(4).enumerate() {
        let mut values = [0u8; 4];
        let mut padding = 0usize;
        for (position, byte) in chunk.iter().enumerate() {
            if *byte == b'=' {
                padding += 1;
                values[position] = 0;
            } else {
                if padding > 0 {
                    return Err("Invalid base64 padding.".to_owned());
                }
                values[position] = base64_value(*byte)
                    .ok_or_else(|| format!("Invalid base64 character at chunk {index}."))?;
            }
        }
        if padding > 2 {
            return Err("Invalid base64 padding.".to_owned());
        }
        output.push((values[0] << 2) | (values[1] >> 4));
        if padding < 2 {
            output.push((values[1] << 4) | (values[2] >> 2));
        }
        if padding == 0 {
            output.push((values[2] << 6) | values[3]);
        }
    }
    Ok(output)
}

fn base64_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{decode_base64, encode_base64, parse_helper_output};

    #[test]
    fn base64_round_trips_ascii_and_utf8() {
        for text in ["", "hello", "LogcatX 剪贴板 ✓", "a\nb\tc"] {
            let encoded = encode_base64(text.as_bytes());
            let decoded = decode_base64(&encoded).expect("valid base64");
            assert_eq!(decoded, text.as_bytes());
        }
        assert_eq!(encode_base64(b"hello"), "aGVsbG8=");
        assert_eq!(encode_base64(b"a"), "YQ==");
    }

    #[test]
    fn base64_rejects_invalid_input() {
        assert!(decode_base64("!!!!").is_err());
        assert!(decode_base64("a").is_err());
        assert!(decode_base64("AB=C").is_err());
    }

    #[test]
    fn helper_replies_are_parsed() {
        let clip = parse_helper_output("CLIP aGVsbG8=").expect("clip");
        assert!(matches!(clip, super::HelperReply::Clip(text) if text == "hello"));
        assert!(matches!(
            parse_helper_output("EMPTY").expect("empty"),
            super::HelperReply::Empty
        ));
        assert!(matches!(
            parse_helper_output("ACK").expect("ack"),
            super::HelperReply::Ack
        ));
        assert!(parse_helper_output("ERROR nope").is_err());
        assert!(parse_helper_output("").is_err());
    }

    #[test]
    fn embedded_helper_is_a_dex() {
        assert!(super::HELPER_DEX.starts_with(b"dex\n"));
    }
}
