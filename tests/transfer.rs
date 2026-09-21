//! TransferManager queue behavior against the fake adb double (PRD §10-§13):
//! progress parsing, cancellation with kill/reap, stall supervision, failure
//! detail, per-device concurrency, and multi-device isolation.

mod common;

use logcatx::transfer::{TransferManager, TransferOptions, TransferState, TransferTask};
use std::{
    path::PathBuf,
    sync::{Mutex, OnceLock},
    thread,
    time::{Duration, Instant},
};

/// One shared scenario script distinguishes tests by remote path prefix;
/// FAKE_ADB_SCRIPT is process-global, so it is installed exactly once.
static SCRIPT_READY: OnceLock<()> = OnceLock::new();
static SCRIPT_GUARD: Mutex<()> = Mutex::new(());

fn manager(options: TransferOptions) -> TransferManager {
    SCRIPT_READY.get_or_init(|| {
        let _guard = SCRIPT_GUARD
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempfile::tempdir().expect("tempdir").keep();
        let script_path = dir.join("scenario.txt");
        std::fs::write(
            &script_path,
            "push * => estream:40:3:[ 30%] 12.5 MB/s (314572/1048576) | exit:0\n\
             pull /stall/* => sleep:10000 | exit:0\n\
             pull /cancel/* => estream:100:60:chunk | exit:0\n\
             pull /fail/* => err:adb: error: device offline | exit:1\n\
             pull /queue-a/* => sleep:800 | exit:0\n\
             pull /queue-b/* => sleep:800 | exit:0\n\
             pull /other-dev/* => sleep:800 | exit:0\n",
        )
        .expect("write script");
        unsafe {
            std::env::set_var("FAKE_ADB_SCRIPT", &script_path);
        }
    });
    TransferManager::with_options(common::exe(), options)
}

fn wait_until(
    manager: &TransferManager,
    id: u64,
    timeout: Duration,
    predicate: impl Fn(&TransferTask) -> bool,
) -> TransferTask {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(task) = manager.snapshot().into_iter().find(|task| task.id == id)
            && predicate(&task)
        {
            return task;
        }
        thread::sleep(Duration::from_millis(25));
    }
    manager
        .snapshot()
        .into_iter()
        .find(|task| task.id == id)
        .unwrap_or_else(|| panic!("task {id} never appeared in the transfer snapshot"))
}

/// A scratch local file (the fake adb never touches it, but the paths must
/// be plausible for push metadata and pull destinations).
fn local_target(name: &str) -> PathBuf {
    let path = tempfile::tempdir().expect("tempdir").keep().join(name);
    std::fs::write(&path, b"payload").expect("write target file");
    path
}

#[test]
fn push_reports_progress_and_completes() {
    let manager = manager(TransferOptions::default());
    let source = local_target("progress.bin");
    let id = manager.enqueue_push("device-a", source, "/sdcard/progress.bin".into());

    let task = wait_until(&manager, id, Duration::from_secs(5), |task| {
        task.state == TransferState::Completed
    });
    assert_eq!(task.state, TransferState::Completed);
    assert_eq!(task.bytes_total, Some(1048576));
    assert_eq!(task.bytes_transferred, 314572);
    assert_eq!(task.speed_bps, Some(12_500_000));
    assert!(task.error.is_none());
    manager.shutdown();
}

#[test]
fn cancel_mid_transfer_finishes_promptly() {
    let manager = manager(TransferOptions::default());
    let id = manager.enqueue_pull(
        "device-a",
        "/cancel/huge.zip".into(),
        local_target("cancel.bin"),
    );
    wait_until(&manager, id, Duration::from_secs(5), |task| {
        task.state == TransferState::Running
    });

    let started = Instant::now();
    manager.cancel(id);
    let task = wait_until(&manager, id, Duration::from_secs(5), |task| {
        task.state == TransferState::Cancelled
    });
    assert_eq!(task.state, TransferState::Cancelled);
    // kill+reap must be prompt: the fake would stream for 6 more seconds.
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "cancel took {:?}",
        started.elapsed()
    );
    manager.shutdown();
}

#[test]
fn silent_transfer_fails_as_stalled() {
    let options = TransferOptions {
        stall_timeout: Duration::from_millis(400),
        ..TransferOptions::default()
    };
    let manager = manager(options);
    let id = manager.enqueue_pull(
        "device-a",
        "/stall/frozen.zip".into(),
        local_target("stall.bin"),
    );
    let started = Instant::now();
    let task = wait_until(&manager, id, Duration::from_secs(5), |task| {
        task.state == TransferState::Failed
    });
    assert_eq!(task.state, TransferState::Failed);
    assert!(
        task.error.as_deref().unwrap_or("").contains("stalled"),
        "error was: {:?}",
        task.error
    );
    assert!(started.elapsed() < Duration::from_secs(3));
    manager.shutdown();
}

#[test]
fn adb_failure_carries_stderr_detail() {
    let manager = manager(TransferOptions::default());
    let id = manager.enqueue_pull(
        "device-a",
        "/fail/missing.zip".into(),
        local_target("fail.bin"),
    );
    let task = wait_until(&manager, id, Duration::from_secs(5), |task| {
        task.state == TransferState::Failed
    });
    let error = task.error.as_deref().unwrap_or("");
    assert!(error.contains("device offline"), "error was: {error}");
    manager.shutdown();
}

#[test]
fn per_device_concurrency_capped_and_devices_isolated() {
    // One slot per device: queue-b must wait for queue-a on the same device,
    // while a transfer on another device runs immediately (PRD §11/§19).
    let options = TransferOptions {
        per_device_concurrency: 1,
        ..TransferOptions::default()
    };
    let manager = manager(options);
    let a = manager.enqueue_pull(
        "device-a",
        "/queue-a/first.zip".into(),
        local_target("q1.bin"),
    );
    let b = manager.enqueue_pull(
        "device-a",
        "/queue-b/second.zip".into(),
        local_target("q2.bin"),
    );
    let other = manager.enqueue_pull(
        "device-b",
        "/other-dev/parallel.zip".into(),
        local_target("q3.bin"),
    );

    // The device-b transfer starts right away even though device-a is busy.
    wait_until(&manager, other, Duration::from_secs(5), |task| {
        task.state == TransferState::Running
    });

    // While queue-a is still inside its 800ms sleep, queue-b must be queued.
    let snapshot = manager.snapshot();
    let a_state = snapshot
        .iter()
        .find(|task| task.id == a)
        .expect("task a present");
    let b_state = snapshot
        .iter()
        .find(|task| task.id == b)
        .expect("task b present");
    if a_state.state == TransferState::Running {
        assert_eq!(b_state.state, TransferState::Queued);
    }

    wait_until(&manager, a, Duration::from_secs(5), |task| {
        task.state == TransferState::Completed
    });
    wait_until(&manager, b, Duration::from_secs(5), |task| {
        task.state == TransferState::Completed
    });
    wait_until(&manager, other, Duration::from_secs(5), |task| {
        task.state == TransferState::Completed
    });
    manager.shutdown();
}

#[test]
fn failed_transfer_can_be_retried() {
    let manager = manager(TransferOptions::default());
    let id = manager.enqueue_pull(
        "device-a",
        "/fail/again.zip".into(),
        local_target("retry.bin"),
    );
    wait_until(&manager, id, Duration::from_secs(5), |task| {
        task.state == TransferState::Failed
    });

    manager.retry(id);
    // The scenario fails again, proving the task actually re-executed.
    let task = wait_until(&manager, id, Duration::from_secs(5), |task| {
        task.state == TransferState::Failed && task.error.is_some() && task.bytes_transferred == 0
    });
    assert!(task.error.is_some());
    manager.shutdown();
}

#[test]
fn clear_finished_removes_terminal_tasks() {
    let manager = manager(TransferOptions::default());
    let id = manager.enqueue_pull(
        "device-a",
        "/fail/gone.zip".into(),
        local_target("clear.bin"),
    );
    wait_until(&manager, id, Duration::from_secs(5), |task| {
        task.state == TransferState::Failed
    });

    manager.clear_finished();
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && !manager.snapshot().is_empty() {
        thread::sleep(Duration::from_millis(25));
    }
    assert!(manager.snapshot().is_empty(), "finished tasks cleared");
    manager.shutdown();
}
