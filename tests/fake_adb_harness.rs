//! Validates the `fake_adb` test double itself (PRD §43): every scenario
//! primitive it offers must behave exactly as the M1+ suites will assume.

mod common;

use common::run;
use std::{
    thread::sleep,
    time::{Duration, Instant},
};

#[test]
fn prints_stdout_scenario_and_exits_zero() {
    let output = run(
        "devices => out:List of devices attached\\nZX1ABC\tdevice\\n",
        &["devices"],
    );
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        "List of devices attached\nZX1ABC\tdevice\n"
    );
}

#[test]
fn respects_exit_code_and_stderr_actions() {
    let output = run(
        "shell pm clear com.x => err:failed to clear\\n | exit:1",
        &["-s", "serial", "shell", "pm", "clear", "com.x"],
    );
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(String::from_utf8_lossy(&output.stderr), "failed to clear\n");
    assert!(output.stdout.is_empty());
}

#[test]
fn serial_is_transparent_for_scenario_matching() {
    // The scenario is keyed without `-s <serial>`; passing a serial must match.
    let output = run(
        "shell echo hi => out:hi\\n",
        &["-s", "192.168.1.9:5555", "shell", "echo", "hi"],
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "hi\n");
}

#[test]
fn prefix_wildcard_matches_family_of_commands() {
    let output = run(
        "shell getprop * => out:pixel\\n",
        &["-s", "s", "shell", "getprop", "ro.product.model"],
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "pixel\n");
}

#[test]
fn unmatched_command_fails_loudly() {
    let output = run("devices => out:ok", &["connect", "1.2.3.4:5555"]);
    assert_eq!(output.status.code(), Some(64));
    assert!(String::from_utf8_lossy(&output.stderr).contains("no scenario"));
}

#[test]
fn missing_script_env_fails_immediately() {
    let output = std::process::Command::new(common::exe())
        .arg("devices")
        .output()
        .expect("spawn fake_adb without script");
    assert_eq!(output.status.code(), Some(2));
}

#[test]
fn sleep_action_delays_completion() {
    let started = Instant::now();
    let output = run("devices => sleep:300 | out:done", &["devices"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(started.elapsed() >= Duration::from_millis(300));
}

#[test]
fn flood_emits_exact_byte_count() {
    let total = 100 * 1024 + 7;
    let output = run(&format!("devices => flood:{total}"), &["devices"]);
    assert_eq!(output.stdout.len(), total);
    assert!(output.stdout.iter().all(|&byte| byte == b'x'));
}

#[test]
fn hang_never_exits_until_killed() {
    let (scenario, mut child) = common::spawn("devices => hang", &["devices"]);
    sleep(Duration::from_millis(200));
    assert!(
        child.try_wait().expect("try_wait hang child").is_none(),
        "hang scenario must keep running"
    );
    child.kill().expect("kill hang child");
    let status = child.wait().expect("reap hang child");
    assert!(!status.success());
    drop(scenario);
}

#[test]
fn stream_emits_chunks_over_time() {
    let started = Instant::now();
    let output = run("logcat => stream:80:3:line", &["-s", "s", "logcat"]);
    let text = String::from_utf8_lossy(&output.stdout);
    assert_eq!(text, "line\nline\nline\n");
    assert!(
        started.elapsed() >= Duration::from_millis(240),
        "three chunks at 80ms must take at least 240ms"
    );
}
