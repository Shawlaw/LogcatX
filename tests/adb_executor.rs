//! AdbExecutor behavior against the process-level fake adb double (PRD §14-16):
//! timeout+kill+reap, cancellation, bounded capture, streaming spawn.

mod common;

use logcatx::adb_executor::{AdbError, AdbExecutor, CancelToken, ExecOptions};
use std::{
    thread,
    time::{Duration, Instant},
};

fn run_with_script(
    script: &str,
    args: &[&str],
    options: ExecOptions,
) -> Result<logcatx::adb_executor::AdbOutput, AdbError> {
    let dir = tempfile::tempdir().expect("tempdir");
    let script_path = dir.path().join("scenario.txt");
    std::fs::write(&script_path, script).expect("write script");
    let executor = AdbExecutor::new(common::exe());
    let mut options = options;
    options.env.push((
        "FAKE_ADB_SCRIPT".to_owned(),
        script_path.to_string_lossy().into_owned(),
    ));
    executor.execute_with_options(args, options)
}

#[test]
fn captures_stdout_stderr_and_exit_code() {
    let output = run_with_script(
        "shell echo hi => out:hi\\n | err:note\\n | exit:0",
        &["shell", "echo", "hi"],
        ExecOptions::default(),
    )
    .expect("execution succeeds");
    assert_eq!(output.exit_code, Some(0));
    assert_eq!(output.stdout_lossy(), "hi\n");
    assert_eq!(output.stderr_lossy(), "note\n");
    assert!(!output.stdout_truncated);
}

#[test]
fn reports_nonzero_exit() {
    let output = run_with_script(
        "shell pm clear com.x => err:denied | exit:1",
        &["shell", "pm", "clear", "com.x"],
        ExecOptions::default(),
    )
    .expect("execution succeeds");
    assert_eq!(output.exit_code, Some(1));
    assert!(!output.success());
}

#[test]
fn timeout_kills_hanging_child() {
    let started = Instant::now();
    let error = run_with_script(
        "devices => hang",
        &["devices"],
        ExecOptions {
            timeout: Some(Duration::from_millis(300)),
            ..Default::default()
        },
    )
    .expect_err("hang must time out");
    assert!(matches!(error, AdbError::Timeout { .. }));
    // Kill+reap must happen promptly after the deadline, not linger.
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[test]
fn cancel_token_beats_timeout() {
    let cancel = CancelToken::new();
    cancel.cancel();
    let error = run_with_script(
        "devices => out:never",
        &["devices"],
        ExecOptions {
            cancel: Some(cancel),
            ..Default::default()
        },
    )
    .expect_err("pre-cancelled token must refuse to run");
    assert!(matches!(error, AdbError::Cancelled { .. }));
}

#[test]
fn mid_execution_cancel_returns_cancelled() {
    let cancel = CancelToken::new();
    let cancel_for_thread = cancel.clone();
    thread::spawn(move || {
        thread::sleep(Duration::from_millis(150));
        cancel_for_thread.cancel();
    });
    let error = run_with_script(
        "devices => sleep:5000 | out:late",
        &["devices"],
        ExecOptions {
            timeout: Some(Duration::from_secs(30)),
            cancel: Some(cancel),
            ..Default::default()
        },
    )
    .expect_err("cancelled execution must fail");
    assert!(matches!(error, AdbError::Cancelled { .. }));
}

#[test]
fn flooding_output_is_truncated_not_fatal() {
    let output = run_with_script(
        "devices => flood:65536",
        &["devices"],
        ExecOptions {
            stdout_limit: Some(4096),
            ..Default::default()
        },
    )
    .expect("flood completes with truncated capture");
    assert_eq!(output.exit_code, Some(0));
    assert_eq!(output.stdout.len(), 4096);
    assert!(output.stdout_truncated);
}

#[test]
fn spawn_error_for_missing_binary() {
    let executor = AdbExecutor::new("definitely-not-a-real-adb-binary");
    let error = executor
        .execute(&["devices"])
        .expect_err("missing binary must fail");
    assert!(matches!(error, AdbError::Spawn { .. }));
}
