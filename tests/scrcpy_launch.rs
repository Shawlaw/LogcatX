//! Process-level proof that scrcpy receives independent argv items without a
//! shell, using the same real `Command::spawn` path as the application.

use logcatx::scrcpy::{self, LaunchMode, NewDisplayOptions, ScrcpyLaunchOptions};
use serde_json::Value;
use std::{
    path::Path,
    sync::{Mutex, MutexGuard},
    time::{Duration, Instant},
};

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct FakeScrcpyEnv {
    _lock: MutexGuard<'static, ()>,
}

impl FakeScrcpyEnv {
    fn new(log: &Path) -> Self {
        let lock = ENV_LOCK.lock().expect("serialize fake scrcpy environment");
        // SAFETY: all tests in this binary hold ENV_LOCK while the child that
        // inherits this variable is running.
        unsafe {
            std::env::set_var("FAKE_SCRCPY_LOG", log);
        }
        Self { _lock: lock }
    }

    fn fail_startup() {
        // SAFETY: the caller holds the ENV_LOCK guard returned by new().
        unsafe {
            std::env::set_var("FAKE_SCRCPY_FAIL_STARTUP", "1");
        }
    }
}

impl Drop for FakeScrcpyEnv {
    fn drop(&mut self) {
        // SAFETY: the matching guard is still held by self.
        unsafe {
            std::env::remove_var("FAKE_SCRCPY_LOG");
            std::env::remove_var("FAKE_SCRCPY_FAIL_STARTUP");
        }
    }
}

fn wait_for_record(path: &Path) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(bytes) = std::fs::read(path)
            && let Ok(record) = serde_json::from_slice(&bytes)
        {
            return record;
        }
        if Instant::now() >= deadline {
            panic!("fake scrcpy did not write {}", path.display());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn args(record: &Value) -> Vec<&str> {
    record["args"]
        .as_array()
        .expect("argv array")
        .iter()
        .map(|arg| arg.as_str().expect("string argv"))
        .collect()
}

#[test]
fn mirror_spawn_preserves_spaces_backslashes_and_adb_environment() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("mirror.json");
    let _env = FakeScrcpyEnv::new(&log);
    let fake = env!("CARGO_BIN_EXE_fake_scrcpy");
    assert_eq!(
        scrcpy::validate_scrcpy_path(fake).unwrap().to_string(),
        "3.3.4"
    );

    scrcpy::launch(
        fake,
        r"C:\Android SDK\platform-tools\adb.exe",
        "DEVICE_TRANSPORT",
        LaunchMode::Mirror,
        &ScrcpyLaunchOptions {
            no_control: true,
            no_audio: true,
            extra_args:
                "--window-title=My Phone\n--video-codec\nh265\n--record=C:\\Temp\\My Phone.mp4"
                    .into(),
        },
    )
    .unwrap();

    let record = wait_for_record(&log);
    assert_eq!(
        args(&record),
        [
            "--serial",
            "DEVICE_TRANSPORT",
            "--no-control",
            "--no-audio",
            "--window-title=My Phone",
            "--video-codec",
            "h265",
            r"--record=C:\Temp\My Phone.mp4",
        ]
    );
    assert_eq!(
        record["adb"].as_str(),
        Some(r"C:\Android SDK\platform-tools\adb.exe")
    );
}

#[test]
fn getopt_selector_bypasses_never_spawn_scrcpy() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("forbidden.json");
    let _env = FakeScrcpyEnv::new(&log);

    for selector in ["--seri=OTHER", "-nsOTHER"] {
        let error = scrcpy::launch(
            env!("CARGO_BIN_EXE_fake_scrcpy"),
            "adb",
            "DEVICE_TRANSPORT",
            LaunchMode::Mirror,
            &ScrcpyLaunchOptions {
                extra_args: selector.into(),
                ..Default::default()
            },
        )
        .expect_err("getopt-compatible selector override must be rejected");
        assert!(error.contains("cannot override the selected device"));
    }
    assert!(!log.exists(), "rejected options must not spawn a process");
}

#[test]
fn immediate_scrcpy_exit_is_not_reported_as_a_successful_launch() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("early-exit.json");
    let _env = FakeScrcpyEnv::new(&log);
    FakeScrcpyEnv::fail_startup();

    let error = scrcpy::launch(
        env!("CARGO_BIN_EXE_fake_scrcpy"),
        "adb",
        "DEVICE_TRANSPORT",
        LaunchMode::Mirror,
        &ScrcpyLaunchOptions::default(),
    )
    .expect_err("early process exit is a failed launch");
    assert!(error.contains("exited during startup"), "{error}");
    assert!(error.contains("synthetic scrcpy stdout"), "{error}");
    assert!(
        error.contains("ERROR: synthetic scrcpy startup failure"),
        "{error}"
    );
    wait_for_record(&log);
}

#[test]
fn start_app_no_control_returns_the_raw_scrcpy_error() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("start-app-no-control.json");
    let _env = FakeScrcpyEnv::new(&log);

    let error = scrcpy::launch(
        env!("CARGO_BIN_EXE_fake_scrcpy"),
        "adb",
        "DEVICE_TRANSPORT",
        LaunchMode::NewDisplay(
            NewDisplayOptions::default()
                .with_start_app("com.android.settings")
                .unwrap(),
        ),
        &ScrcpyLaunchOptions {
            no_control: true,
            ..Default::default()
        },
    )
    .expect_err("scrcpy rejects start-app when control is disabled");
    assert!(
        error.contains("ERROR: Cannot start an Android app if control is disabled"),
        "{error}"
    );
    wait_for_record(&log);
}

#[test]
fn recording_spawn_keeps_its_independent_fixed_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("recording.json");
    let output = dir.path().join("recording.mp4");
    let _env = FakeScrcpyEnv::new(&log);

    let mut process = scrcpy::spawn_recording(
        env!("CARGO_BIN_EXE_fake_scrcpy"),
        "adb",
        "DEVICE_TRANSPORT",
        &output,
        12,
    )
    .unwrap();
    process.child.wait().unwrap();

    let record = wait_for_record(&log);
    assert_eq!(
        args(&record),
        [
            "--serial",
            "DEVICE_TRANSPORT",
            "--no-playback",
            "--no-audio",
            &format!("--record={}", output.display()),
            "--record-format=mp4",
            "--video-bit-rate=12M",
        ]
    );
}

#[test]
fn new_display_spawn_keeps_existing_mode_arguments_and_shared_options() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("new-display.json");
    let _env = FakeScrcpyEnv::new(&log);

    scrcpy::launch(
        env!("CARGO_BIN_EXE_fake_scrcpy"),
        "adb",
        "DEVICE_TRANSPORT",
        LaunchMode::NewDisplay(
            NewDisplayOptions::custom("1280", "960", "160")
                .unwrap()
                .with_start_app("com.android.settings")
                .unwrap(),
        ),
        &ScrcpyLaunchOptions {
            no_control: false,
            no_audio: true,
            extra_args: "--max-size=1280".into(),
        },
    )
    .unwrap();

    let record = wait_for_record(&log);
    assert_eq!(
        args(&record),
        [
            "--serial",
            "DEVICE_TRANSPORT",
            "--new-display=1280x960/160",
            "--start-app",
            "com.android.settings",
            "--no-audio",
            "--max-size=1280",
        ]
    );
}
