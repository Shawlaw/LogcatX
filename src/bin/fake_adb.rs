//! Process-level ADB test double (PRD §43).
//!
//! The real product spawns `adb` as a child process, so the fake must be a
//! real executable too. Behavior is driven by a scenario script referenced
//! through the `FAKE_ADB_SCRIPT` environment variable:
//!
//! ```text
//! # comment lines are ignored
//! <command pattern> => <action> | <action> | ...
//! ```
//!
//! The command pattern matches the argv after the adb executable name, with
//! any `-s <serial>` pair removed. `*` matches everything and a trailing `*`
//! makes a prefix match. Actions run in order:
//!
//! - `out:<text>`      write text to stdout (`\n` escapes become newlines)
//! - `err:<text>`      write text to stderr
//! - `flood:<bytes>`   write that many `x` bytes to stdout in chunks
//! - `sleep:<ms>`      sleep
//! - `stream:<ms>:<count>:<text>` write text every interval, count times
//! - `hang`            never exit (timeout/cancel tests kill it externally)
//! - `exit:<code>`     final exit status (default 0)
//!
//! Unmatched commands fail loudly (exit 64) so tests notice missing
//! scenarios instead of silently passing.

use std::{io::Write, process::exit, thread::sleep, time::Duration};

const EXIT_NO_SCRIPT_ENV: i32 = 2;
const EXIT_SCRIPT_UNREADABLE: i32 = 3;
const EXIT_NO_SCENARIO: i32 = 64;

fn main() {
    let script_path = match std::env::var("FAKE_ADB_SCRIPT") {
        Ok(path) => path,
        Err(_) => {
            eprintln!("fake_adb: FAKE_ADB_SCRIPT is not set");
            exit(EXIT_NO_SCRIPT_ENV);
        }
    };
    let script = match std::fs::read_to_string(&script_path) {
        Ok(script) => script,
        Err(err) => {
            eprintln!("fake_adb: cannot read script {script_path}: {err}");
            exit(EXIT_SCRIPT_UNREADABLE);
        }
    };

    let canonical = canonical_args(std::env::args().skip(1).collect());
    let actions = match find_scenario(&script, &canonical) {
        Some(actions) => actions,
        None => {
            eprintln!("fake_adb: no scenario for '{canonical}'");
            exit(EXIT_NO_SCENARIO);
        }
    };

    let mut exit_code = 0;
    for action in actions {
        match execute_action(action) {
            Ok(ActionOutcome::Continue) => {}
            Ok(ActionOutcome::Exit(code)) => exit_code = code,
            Err(message) => {
                eprintln!("fake_adb: {message}");
                exit(EXIT_SCRIPT_UNREADABLE);
            }
        }
    }
    exit(exit_code);
}

/// Join argv into the canonical command key: `-s <serial>` pairs removed,
/// single-space separated (e.g. `shell getprop ro.product.model`).
fn canonical_args(args: Vec<String>) -> String {
    let mut cleaned = Vec::new();
    let mut skip_next = false;
    for arg in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if arg == "-s" {
            skip_next = true;
            continue;
        }
        cleaned.push(arg);
    }
    cleaned.join(" ")
}

fn find_scenario<'a>(script: &'a str, canonical: &str) -> Option<Vec<&'a str>> {
    for line in script.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((pattern, actions)) = line.split_once("=>") else {
            continue;
        };
        let pattern = pattern.trim();
        if pattern == "*" || pattern == canonical || pattern_strip_star(pattern, canonical) {
            return Some(
                actions
                    .split('|')
                    .map(str::trim)
                    .filter(|action| !action.is_empty())
                    .collect(),
            );
        }
    }
    None
}

/// A trailing `*` in the pattern makes it a prefix match.
fn pattern_strip_star(pattern: &str, canonical: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => !prefix.is_empty() && canonical.starts_with(prefix),
        None => false,
    }
}

enum ActionOutcome {
    Continue,
    Exit(i32),
}

fn execute_action(action: &str) -> Result<ActionOutcome, String> {
    if action == "hang" {
        loop {
            sleep(Duration::from_secs(1));
        }
    }
    let (verb, payload) = action
        .split_once(':')
        .ok_or_else(|| format!("malformed action '{action}'"))?;

    match verb {
        "out" => {
            write_stdout(&unescape(payload));
            Ok(ActionOutcome::Continue)
        }
        "err" => {
            let mut stderr = std::io::stderr();
            stderr
                .write_all(unescape(payload).as_bytes())
                .map_err(|err| format!("stderr write failed: {err}"))?;
            Ok(ActionOutcome::Continue)
        }
        "flood" => {
            let total: usize = payload
                .parse()
                .map_err(|_| format!("flood needs a byte count, got '{payload}'"))?;
            flood_stdout(total)?;
            Ok(ActionOutcome::Continue)
        }
        "sleep" => {
            let ms: u64 = payload
                .parse()
                .map_err(|_| format!("sleep needs milliseconds, got '{payload}'"))?;
            sleep(Duration::from_millis(ms));
            Ok(ActionOutcome::Continue)
        }
        "stream" => {
            let mut parts = payload.splitn(3, ':');
            let interval: u64 = parts
                .next()
                .and_then(|v| v.parse().ok())
                .ok_or_else(|| format!("stream needs interval_ms:count:text, got '{payload}'"))?;
            let count: u64 = parts
                .next()
                .and_then(|v| v.parse().ok())
                .ok_or_else(|| format!("stream needs interval_ms:count:text, got '{payload}'"))?;
            let text = parts.next().unwrap_or("");
            for _ in 0..count {
                sleep(Duration::from_millis(interval));
                write_stdout(&format!("{}\n", unescape(text)));
            }
            Ok(ActionOutcome::Continue)
        }
        "exit" => {
            let code: i32 = payload
                .parse()
                .map_err(|_| format!("exit needs a code, got '{payload}'"))?;
            Ok(ActionOutcome::Exit(code))
        }
        other => Err(format!("unknown action '{other}'")),
    }
}

fn write_stdout(text: &str) {
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    // The double must keep going even if the parent stops reading, so a
    // broken pipe aborts the process instead of panicking on the lock.
    if lock.write_all(text.as_bytes()).is_err() {
        exit(0);
    }
    let _ = lock.flush();
}

/// Emit `total` bytes of `x` without allocating the whole buffer at once.
fn flood_stdout(total: usize) -> Result<(), String> {
    const CHUNK: usize = 4 * 1024;
    let chunk = "x".repeat(CHUNK);
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let mut written = 0;
    while written < total {
        let take = CHUNK.min(total - written);
        lock.write_all(&chunk.as_bytes()[..take])
            .map_err(|err| format!("flood write failed: {err}"))?;
        written += take;
    }
    lock.flush()
        .map_err(|err| format!("flood flush failed: {err}"))
}

/// Turn `\n` (and `\\`) escapes in scenario text into real characters.
fn unescape(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('n') => result.push('\n'),
                Some('\\') => result.push('\\'),
                Some(other) => {
                    result.push('\\');
                    result.push(other);
                }
                None => result.push('\\'),
            }
        } else {
            result.push(ch);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_args_strips_serial() {
        let args = vec![
            "-s".to_owned(),
            "192.168.1.5:5555".to_owned(),
            "shell".to_owned(),
            "getprop".to_owned(),
            "ro.product.model".to_owned(),
        ];
        assert_eq!(canonical_args(args), "shell getprop ro.product.model");
    }

    #[test]
    fn canonical_args_keeps_other_flags() {
        let args = vec!["devices".to_owned(), "-l".to_owned()];
        assert_eq!(canonical_args(args), "devices -l");
    }

    #[test]
    fn scenario_exact_and_wildcard_match() {
        let script = "devices => out:ok\nshell getprop * => exit:1\n* => out:fallback";
        assert_eq!(find_scenario(script, "devices"), Some(vec!["out:ok"]));
        assert_eq!(
            find_scenario(script, "shell getprop ro.build.id"),
            Some(vec!["exit:1"])
        );
        assert_eq!(
            find_scenario(script, "connect 1.2.3.4:5555"),
            Some(vec!["out:fallback"])
        );
    }

    #[test]
    fn unmatched_returns_none() {
        assert_eq!(find_scenario("devices => out:ok", "connect x"), None);
    }

    #[test]
    fn unescape_handles_newlines() {
        assert_eq!(unescape("a\\nb"), "a\nb");
        assert_eq!(unescape("a\\\\b"), "a\\b");
        assert_eq!(unescape("plain"), "plain");
    }
}
