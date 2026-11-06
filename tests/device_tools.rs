//! Integration coverage for the text/clipboard device tools through the
//! process-level fake adb double (no device required).

mod common;

use logcatx::{clipboard, text_input};
use std::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn use_scenario(contents: &str) -> common::Scenario {
    let scenario = common::scenario(contents);
    // The library spawns fake_adb as a child; it inherits this environment.
    // SAFETY: this test binary runs its scenarios sequentially and is the only
    // process reading/writing this variable.
    unsafe {
        std::env::set_var("FAKE_ADB_SCRIPT", &scenario.script);
    }
    scenario
}

#[test]
fn ascii_text_sends_input_text_and_enter_without_clipboard_fallback() {
    let _guard = ENV_LOCK.lock().expect("serialize fake adb env");
    let _scenario = use_scenario(
        "shell input text * => out:\n\
         shell input keyevent * => out:\n\
         push * => out:pushed\n\
         shell CLASSPATH=* => out:ACK\n",
    );
    let adb = common::exe();
    let outcome = text_input::send_text(adb, "serial", "hello world\nsecond line", true)
        .expect("ASCII text is accepted");
    assert_eq!(outcome.clipboard_pastes, 0);
    assert_eq!(outcome.chars, "hello world\nsecond line".chars().count());
}

#[test]
fn rejected_unicode_falls_back_to_the_device_clipboard_and_paste() {
    let _guard = ENV_LOCK.lock().expect("serialize fake adb env");
    let _scenario = use_scenario(
        "shell input text * => err:java.lang.NullPointerException | exit:1\n\
         shell input keyevent 279 => out:\n\
         push * => out:pushed\n\
         shell CLASSPATH=* => out:ACK\n",
    );
    let adb = common::exe();
    let outcome = text_input::send_text(adb, "serial", "中文", true)
        .expect("unicode falls back to clipboard paste");
    assert_eq!(outcome.clipboard_pastes, 1);
    assert_eq!(outcome.chars, 2);
}

#[test]
fn clipboard_read_parses_the_helper_protocol() {
    let _guard = ENV_LOCK.lock().expect("serialize fake adb env");
    let _scenario = use_scenario(
        "push * => out:pushed\n\
         shell CLASSPATH=* => out:CLIP aGVsbG8=\n",
    );
    let adb = common::exe();
    let text = clipboard::read_text(adb, "serial").expect("read through fake adb");
    assert_eq!(text.as_deref(), Some("hello"));
}
