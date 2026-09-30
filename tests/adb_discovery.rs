//! Discovery-level behavior against the fake adb double: transient daemon
//! faults, restart readiness probing, and the list_devices happy path.

mod common;

use logcatx::adb::{list_devices, restart_server};
use std::{
    sync::{Mutex, OnceLock},
    time::Instant,
};

/// The fake-adb script is injected through the process environment, so the
/// tests in this file serialize on one guard and swap the script per test.
static ENV_GUARD: OnceLock<Mutex<()>> = OnceLock::new();

fn with_script<T>(script: &str, body: impl FnOnce() -> T) -> T {
    let guard = ENV_GUARD.get_or_init(|| Mutex::new(()));
    let _lock = guard.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let dir = tempfile::tempdir().expect("tempdir");
    let script_path = dir.path().join("scenario.txt");
    std::fs::write(&script_path, script).expect("write script");
    // The tempdir must outlive the body: children spawned inside read the
    // script lazily, per invocation.
    std::mem::forget(dir);
    unsafe {
        std::env::set_var("FAKE_ADB_SCRIPT", &script_path);
    }
    body()
}

#[test]
fn list_devices_parses_devices_output() {
    with_script(
        "devices => out:List of devices attached\\nABC123\tdevice\\n",
        || {
            let outcome = list_devices(common::exe(), Default::default(), false)
                .expect("discovery succeeds");
            assert_eq!(outcome.devices.len(), 1);
            assert_eq!(outcome.devices[0].serial, "ABC123");
            assert_eq!(outcome.devices[0].state, "device");
        },
    );
}

#[test]
fn list_devices_retries_transient_protocol_fault() {
    let started = Instant::now();
    with_script(
        "devices => err:adb.exe: failed to check server version: protocol fault (couldn't read status): connection reset | exit:1",
        || {
            let err = list_devices(common::exe(), Default::default(), false)
                .expect_err("persistent fault must surface");
            assert!(
                err.contains("protocol fault"),
                "error should carry the adb diagnostic: {err}"
            );
        },
    );
    assert!(
        started.elapsed() >= std::time::Duration::from_millis(600),
        "the transient fault must have been retried once"
    );
}

#[test]
fn list_devices_fails_without_retry_on_real_errors() {
    with_script(
        "devices => err:adb: unrecognized arguments | exit:1",
        || {
            let err = list_devices(common::exe(), Default::default(), false)
                .expect_err("non-transient failure surfaces directly");
            assert!(
                err.contains("unrecognized arguments"),
                "error should carry the adb diagnostic: {err}"
            );
        },
    );
}

#[test]
fn restart_server_succeeds_and_probes_readiness() {
    with_script(
        "kill-server => exit:0\ndevices => out:List of devices attached\\n",
        || {
            // `start-server` is fake_adb's built-in silent success; the
            // readiness probe after it hits the scripted `devices`.
            restart_server(common::exe()).expect("restart succeeds against the fake");
        },
    );
}
