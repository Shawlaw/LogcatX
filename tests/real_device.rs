//! Optional real-device smoke against a physically attached Android device
//! (a subset of PRD §46 that needs no GUI). Every test is `#[ignore]`d so
//! CI and ordinary runs skip them; enable by setting `LOGCATX_REAL_ADB` to
//! a real adb binary and running:
//!
//! ```text
//! LOGCATX_REAL_ADB="$(where adb)" cargo test --test real_device -- --ignored --test-threads=1
//! ```
//!
//! The first attached device in `device` state is used (override with
//! `LOGCATX_REAL_SERIAL`). Mutation tests confine themselves to a unique
//! `/sdcard/LogcatXSmoke_<pid>_<ts>` directory and remove it afterwards.
//!
//! `live_update_channel_*` needs no device: it fetches and verifies the
//! live signed manifest, and additionally requires the build to carry the
//! update public key (`LOGCATX_UPDATE_PUBLIC_KEY` at compile time).

use logcatx::adb::list_devices;
use logcatx::remote_fs::{RemoteEntryKind, RemoteFs, RemoteFsError, RemotePath};
use logcatx::transfer::{TransferManager, TransferState};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn adb_path() -> String {
    std::env::var("LOGCATX_REAL_ADB")
        .expect("set LOGCATX_REAL_ADB=<real adb path> to run real-device smoke tests")
}

fn ready_devices() -> Vec<logcatx::models::DeviceInfo> {
    let outcome = list_devices(&adb_path(), Default::default(), false)
        .expect("adb devices succeeds against the real adb");
    outcome
        .devices
        .into_iter()
        .filter(|d| d.state == "device")
        .collect()
}

fn target_serial() -> String {
    let wanted = std::env::var("LOGCATX_REAL_SERIAL").ok();
    let devices = ready_devices();
    let device = devices
        .iter()
        .find(|d| wanted.as_ref().is_none_or(|w| w == &d.serial))
        // USB transports (serial without ':') are the primary smoke target
        // once one is attached; network endpoints fall back.
        .or_else(|| devices.iter().find(|d| !d.serial.contains(':')))
        .or_else(|| devices.first())
        .expect("at least one device in 'device' state is attached");
    device.serial.clone()
}

fn smoke_root() -> RemotePath {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    RemotePath::new(&format!("/sdcard/LogcatXSmoke_{}_{ts}", std::process::id()))
        .expect("smoke root path is valid")
}

/// Poll the transfer snapshot until the task reaches a terminal state.
fn wait_terminal(
    manager: &TransferManager,
    id: logcatx::transfer::TransferId,
) -> logcatx::transfer::TransferTask {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(task) = manager.snapshot().into_iter().find(|t| t.id == id)
            && task.state.is_terminal()
        {
            return task;
        }
        assert!(
            Instant::now() < deadline,
            "transfer did not finish in 60s (id {id})"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Regression (fixed for 0.9.1): the adb client behind `list_devices` may be
/// the one that (re)starts the shared adb daemon, and a kill-on-close job
/// around that client used to kill the daemon the moment the command
/// finished — wireless connections vanished and "connect succeeded but the
/// device never appeared". The raw probe client below runs outside any job,
/// so its stderr stays clean exactly as long as the daemon survives the
/// executor's child reaping. Needs no attached device, only the real adb.
/// A concurrently running pre-0.9.1 LogcatX can muddy a round by racing its
/// own daemon respawn, so up to three rounds are attempted.
#[test]
#[ignore = "real adb: set LOGCATX_REAL_ADB; restarts the shared adb daemon"]
fn daemon_spawned_by_discovery_survives_command_exit() {
    let adb = adb_path();
    for round in 1..=3 {
        let kill = std::process::Command::new(&adb)
            .args(["kill-server"])
            .output()
            .expect("run adb kill-server");
        assert!(
            kill.status.success(),
            "kill-server failed: {}",
            String::from_utf8_lossy(&kill.stderr)
        );

        list_devices(&adb, Default::default(), false).expect("adb devices succeeds");

        let probe = std::process::Command::new(&adb)
            .args(["devices"])
            .output()
            .expect("run raw adb devices probe");
        let stderr = String::from_utf8_lossy(&probe.stderr);
        if !stderr.contains("daemon not running") {
            return;
        }
        eprintln!("round {round}: daemon was down again, retrying");
    }
    panic!("adb daemon did not survive list_devices (killed by the executor)");
}

#[test]
#[ignore = "real device: set LOGCATX_REAL_ADB"]
fn discovery_resolves_identity_and_metadata() {
    let outcome = list_devices(&adb_path(), Default::default(), true)
        .expect("discovery succeeds against the real adb");
    let device = outcome
        .devices
        .iter()
        .find(|d| d.state == "device")
        .expect("attached device in 'device' state");
    println!(
        "discovered {} state={} identity={} manufacturer={:?} model={:?} android={:?}",
        device.serial,
        device.state,
        device.identity_key,
        device.manufacturer,
        device.model,
        device.android_version
    );
    assert!(!device.identity_key.is_empty(), "identity key resolved");
    assert!(
        device.manufacturer.is_some() && device.model.is_some(),
        "metadata (manufacturer/model) resolved via getprop"
    );
}

#[test]
#[ignore = "real device: set LOGCATX_REAL_ADB"]
fn listing_protocol_parses_real_device_output() {
    let serial = target_serial();
    let fs = RemoteFs::new(&adb_path(), &serial);
    let entries = fs
        .list(&RemotePath::new("/sdcard").unwrap())
        .expect("listing /sdcard succeeds on the real device");
    println!("/sdcard: {} entries", entries.len());
    assert!(
        !entries.is_empty(),
        "/sdcard is never empty on a real device"
    );
    for entry in &entries {
        assert!(
            !entry.name.trim().is_empty(),
            "no blank/glob-leftover names"
        );
        if let RemoteEntryKind::Other(marker) = &entry.kind {
            // `o` markers are legal protocol output; log for visibility.
            println!("nonstandard entry kind {marker:?}: {entry:?}");
        }
    }
    let dirs = entries
        .iter()
        .filter(|e| e.kind == RemoteEntryKind::Directory)
        .count();
    println!(
        "parsed kinds: {} directories among {} entries",
        dirs,
        entries.len()
    );
    assert!(dirs > 0, "real /sdcard contains directories");

    let missing = fs
        .list(&RemotePath::new("/sdcard/__logcatx_missing_no_such__").unwrap())
        .expect_err("missing path must fail");
    assert!(
        matches!(missing, RemoteFsError::NotFound),
        "exit-42 cd failure classifies as NotFound, got {missing:?}"
    );
}

#[test]
#[ignore = "real device: set LOGCATX_REAL_ADB"]
fn empty_directory_lists_empty_without_glob_leftovers() {
    let serial = target_serial();
    let fs = RemoteFs::new(&adb_path(), &serial);
    let root = smoke_root();
    fs.mkdir(&root).expect("mkdir smoke root");
    let entries = fs.list(&root).expect("freshly created dir lists");
    assert!(
        entries.is_empty(),
        "empty dir must not surface a literal '*' entry: {entries:?}"
    );
    fs.delete(&root).expect("cleanup smoke root");
}

#[test]
#[ignore = "real device: set LOGCATX_REAL_ADB"]
fn file_operations_roundtrip_with_special_names() {
    let serial = target_serial();
    let adb = adb_path();
    let fs = RemoteFs::new(&adb, &serial);
    let root = smoke_root();
    fs.mkdir(&root).expect("mkdir smoke root");
    let sub = root.join("子 目录").expect("subdir name with CJK+space");
    fs.mkdir(&sub).expect("mkdir CJK+space subdir");

    // Push a local file with a hostile name through the transfer queue.
    let payload: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
    let local_dir = tempfile::tempdir().expect("tempdir");
    let local_file = local_dir.path().join("名称 空格$.txt");
    std::fs::write(&local_file, &payload).expect("write local payload");

    let manager = TransferManager::new(&adb);
    let remote_target = format!("{}/名称 空格$.txt", root.as_str());
    let push_id = manager.enqueue_push(serial.clone(), local_file.clone(), remote_target.clone());
    let task = wait_terminal(&manager, push_id);
    assert_eq!(
        task.state,
        TransferState::Completed,
        "push completed: {:?}",
        task.error
    );

    // Listing sees the pushed file with the right size.
    let entries = fs.list(&root).expect("list root after push");
    let pushed = entries
        .iter()
        .find(|e| e.name == "名称 空格$.txt")
        .expect("pushed file visible in listing");
    assert_eq!(pushed.kind, RemoteEntryKind::File);
    assert_eq!(pushed.size, payload.len() as u64);

    // Rename, then move into the CJK subdir.
    let renamed = format!("{}/renamed 文件.txt", root.as_str());
    fs.rename(
        &RemotePath::new(&remote_target).unwrap(),
        &RemotePath::new(&renamed).unwrap(),
    )
    .expect("rename succeeds");
    let moved = format!("{}/renamed 文件.txt", sub.as_str());
    fs.rename(
        &RemotePath::new(&renamed).unwrap(),
        &RemotePath::new(&moved).unwrap(),
    )
    .expect("move into subdir succeeds");
    let sub_entries = fs.list(&sub).expect("list subdir after move");
    assert!(
        sub_entries.iter().any(|e| e.name == "renamed 文件.txt"),
        "moved file present in subdir"
    );

    // Pull it back through the transfer queue and byte-compare.
    let back_dir = tempfile::tempdir().expect("tempdir");
    let back_file = back_dir.path().join("pulled.txt");
    let pull_id = manager.enqueue_pull(serial.clone(), moved.clone(), back_file.clone());
    let task = wait_terminal(&manager, pull_id);
    assert_eq!(
        task.state,
        TransferState::Completed,
        "pull completed: {:?}",
        task.error
    );
    let pulled = std::fs::read(&back_file).expect("pulled file exists locally");
    assert_eq!(pulled, payload, "pulled bytes match pushed payload");
    manager.shutdown();

    // Delete the whole tree and verify it is gone from the parent listing.
    fs.delete(&root).expect("delete smoke tree");
    let parent = fs
        .list(&RemotePath::new("/sdcard").unwrap())
        .expect("list /sdcard");
    assert!(
        !parent
            .iter()
            .any(|e| Some(e.name.as_str()) == root.file_name()),
        "smoke tree removed from /sdcard"
    );
}

#[test]
#[ignore = "real device: set LOGCATX_REAL_ADB"]
fn logcat_capture_streams_real_output_then_stops() {
    let serial = target_serial();
    let out_dir = tempfile::tempdir().expect("tempdir");
    let log = out_dir.path().join("logcat.txt");
    let mut child =
        logcatx::adb::spawn_logcat(&adb_path(), &serial, &log, &[]).expect("spawn real logcat");
    std::thread::sleep(Duration::from_secs(5));
    child.kill().expect("kill logcat");
    let status = child.wait().expect("reap logcat");
    println!("logcat exited: {status}");
    let captured = std::fs::read(&log).expect("log file readable");
    println!("captured {} bytes", captured.len());
    assert!(!captured.is_empty(), "logcat wrote output to the file");
    let text = String::from_utf8_lossy(&captured);
    assert!(
        text.lines().count() > 5,
        "log file contains multiple real log lines"
    );
}

#[test]
#[ignore = "real device: set LOGCATX_REAL_ADB"]
fn run_as_capability_probe() {
    let serial = target_serial();
    // Informational: reports whether any installed package accepts run-as
    // (debuggable). No assertion on the outcome — device capability varies.
    let script = "for p in $(pm list packages -3 | cut -d: -f2 | head -30); do \
                  run-as \"$p\" ls >/dev/null 2>&1 && echo \"RUN_AS_OK $p\" && exit 0; done; \
                  echo RUN_AS_NONE";
    let output = logcatx::adb_executor::AdbExecutor::new(adb_path())
        .execute_with_timeout(&["-s", &serial, "shell", script], Duration::from_secs(30))
        .expect("run-as probe executes");
    println!(
        "run-as probe: {:?}",
        String::from_utf8_lossy(&output.stdout).trim()
    );
    assert!(output.success(), "probe script itself must succeed");
}

/// PRD §46 "run-as 支持" case: browse a debuggable app's own data directory
/// through the same RemoteFs run-as prefix the Files page uses. Soft-skips
/// when no attached device has a debuggable package installed.
#[test]
#[ignore = "real device: set LOGCATX_REAL_ADB; needs a debuggable package"]
fn run_as_browsing_lists_app_data_on_real_device() {
    let adb = adb_path();
    let probe = "for p in $(pm list packages -3 | cut -d: -f2 | head -30); do \
                 run-as \"$p\" ls >/dev/null 2>&1 && echo \"$p\" && exit 0; done";
    let mut found: Option<(String, String)> = None;
    for device in ready_devices() {
        let output = logcatx::adb_executor::AdbExecutor::new(&adb)
            .execute_with_timeout(
                &["-s", &device.serial, "shell", probe],
                Duration::from_secs(30),
            )
            .expect("run-as probe executes");
        if let Some(package) = String::from_utf8_lossy(&output.stdout)
            .trim()
            .lines()
            .next()
            && !package.is_empty()
        {
            found = Some((device.serial.clone(), package.to_owned()));
            break;
        }
    }
    let Some((serial, package)) = found else {
        eprintln!("skipping: no attached device exposes a debuggable package");
        return;
    };
    println!("run-as browse via {serial} / {package}");

    let fs = RemoteFs::new_run_as(&adb, &serial, &package);
    let own_dir = RemotePath::new(&format!("/data/data/{package}")).unwrap();
    let entries = fs
        .list(&own_dir)
        .unwrap_or_else(|err| panic!("run-as listing of {package} data dir: {err:?}"));
    println!(
        "run-as {} -> {} entries: {:?}",
        own_dir.as_str(),
        entries.len(),
        entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>()
    );
    assert!(
        !entries.is_empty(),
        "a debuggable app's data dir lists its files/cache/lib entries"
    );
    assert!(
        entries.iter().all(|e| e.kind == RemoteEntryKind::Directory),
        "top-level app data entries are directories"
    );

    // Informational: the Files page also navigates to /data/data itself;
    // traversability varies by device (0711 allows cd but not always glob).
    match fs.list(&RemotePath::new("/data/data").unwrap()) {
        Ok(parent) => println!("/data/data under run-as: {} entries", parent.len()),
        Err(err) => println!("/data/data under run-as not listable: {err:?}"),
    }
}

#[test]
#[ignore = "needs LOGCATX_UPDATE_PUBLIC_KEY at compile time + network"]
fn live_update_channel_fetches_and_verifies_signed_manifest() {
    use desktop_updater::{CheckResult, UpdateConfig, check};
    use logcatx::config::UpdateProxyConfig;
    assert!(
        logcatx::updater::updates_configured(),
        "build lacks LOGCATX_UPDATE_PUBLIC_KEY; rebuild with it set"
    );

    // The live channel is real CDN traffic; occasional 5xx/timeout blips are
    // transport noise, not product failure (the app retries with backoff,
    // PRD §31). Give the check a few attempts before declaring failure.
    fn check_with_retry(config: &UpdateConfig) -> desktop_updater::CheckResult {
        let mut last_err = None;
        for attempt in 0..3 {
            match check(config) {
                Ok(result) => return result,
                Err(error) => {
                    eprintln!("live check attempt {} failed: {error}", attempt + 1);
                    last_err = Some(error);
                    std::thread::sleep(Duration::from_secs(2));
                }
            }
        }
        panic!(
            "live signed check failed after retries: {:?}",
            last_err.map(|e| e.to_string())
        );
    }

    // From an older version the live channel must surface an update (the
    // essence of PRD §45 Test 10 without a released 0.9.0 yet).
    let config = logcatx::updater::update_config("0.0.1", &UpdateProxyConfig::default())
        .expect("proxy config valid")
        .expect("updates configured");
    let result = check_with_retry(&config);
    if let CheckResult::UpdateAvailable(candidate) = &result {
        println!(
            "live channel offers {} (notes {:?})",
            candidate.version(),
            candidate.notes_url()
        );
    }
    // From the current version the same manifest must verify clean.
    let config_now =
        logcatx::updater::update_config(env!("CARGO_PKG_VERSION"), &UpdateProxyConfig::default())
            .expect("proxy config valid")
            .expect("updates configured");
    let now = check_with_retry(&config_now);
    assert_eq!(now, CheckResult::UpToDate);
    assert_ne!(result, CheckResult::UpToDate, "0.0.1 must see an update");
}

/// Soft-skip unless at least two devices are ready; USB + the cloud phone
/// together exercise the PRD §46 multi-device matrix.
fn require_two_devices() -> Vec<logcatx::models::DeviceInfo> {
    let devices = ready_devices();
    if devices.len() < 2 {
        eprintln!(
            "skipping multi-device assertions: {} device(s) in 'device' state",
            devices.len()
        );
    }
    devices
}

#[test]
#[ignore = "real device: set LOGCATX_REAL_ADB"]
fn multi_device_discovery_keeps_identities_distinct() {
    let devices = require_two_devices();
    if devices.len() < 2 {
        return;
    }
    let mut serials: Vec<_> = devices.iter().map(|d| d.serial.as_str()).collect();
    serials.sort_unstable();
    serials.dedup();
    assert_eq!(
        serials.len(),
        devices.len(),
        "every ready device has a distinct serial"
    );
    for device in &devices {
        assert!(
            !device.identity_key.is_empty(),
            "{} has a resolved identity",
            device.serial
        );
        println!(
            "{} -> identity {} ({:?} {:?})",
            device.serial, device.identity_key, device.manufacturer, device.model
        );
    }

    // Group entries by identity: distinct physical hardware must never share
    // an identity. The one legal overlap is the SAME hardware attached over
    // two transports (USB + wireless debugging) — the app folds those into
    // one device row upstream, which requires exactly this shared identity.
    let mut groups: std::collections::BTreeMap<&str, Vec<&str>> = Default::default();
    for device in &devices {
        groups
            .entry(device.identity_key.as_str())
            .or_default()
            .push(&device.serial);
    }
    for (identity, serials) in &groups {
        if serials.len() == 1 {
            continue;
        }
        let usb: Vec<_> = serials.iter().filter(|s| !s.contains(':')).collect();
        let network: Vec<_> = serials.iter().filter(|s| s.contains(':')).collect();
        assert_eq!(
            usb.len() + network.len(),
            serials.len(),
            "transports classify as USB or network"
        );
        assert!(
            !usb.is_empty() && !network.is_empty(),
            "shared identity {identity} spans USB {usb:?} and network {network:?} — \
             anything else would be two devices colliding on one identity"
        );
    }
}

/// The USB+wireless merge path (PRD: one row per physical device, USB
/// preferred) keys off `identity_key`: both transports of the same hardware
/// must resolve the same identity via getprop metadata. The grouping and
/// USB-preference selection themselves are unit-tested in app.rs.
#[test]
#[ignore = "real device: set LOGCATX_REAL_ADB; needs one device on USB AND wireless"]
fn same_device_usb_and_wireless_transports_share_identity() {
    let devices = ready_devices();
    let mut groups: std::collections::BTreeMap<&str, Vec<&logcatx::models::DeviceInfo>> =
        Default::default();
    for device in &devices {
        groups
            .entry(device.identity_key.as_str())
            .or_default()
            .push(device);
    }
    let dual = groups.values().find(|entries| {
        entries.len() > 1
            && entries.iter().any(|d| !d.serial.contains(':'))
            && entries.iter().any(|d| d.serial.contains(':'))
    });
    let Some(entries) = dual else {
        eprintln!(
            "skipping: no device is attached over both USB and wireless ({})",
            devices.len()
        );
        return;
    };
    assert_eq!(
        entries.len(),
        2,
        "exactly two transports of the same hardware"
    );
    let metadata: std::collections::BTreeSet<(Option<&String>, Option<&String>)> = entries
        .iter()
        .map(|d| (d.manufacturer.as_ref(), d.model.as_ref()))
        .collect();
    assert_eq!(
        metadata.len(),
        1,
        "both transports report identical manufacturer/model: {:?}",
        entries.iter().map(|d| &d.serial).collect::<Vec<_>>()
    );
    for entry in entries {
        println!(
            "dual-transport {}: identity {} ({:?} {:?})",
            entry.serial, entry.identity_key, entry.manufacturer, entry.model
        );
    }
}

#[test]
#[ignore = "real device: set LOGCATX_REAL_ADB"]
fn multi_device_logcat_streams_are_isolated() {
    let devices = require_two_devices();
    if devices.len() < 2 {
        return;
    }
    let adb = adb_path();
    let mut children = Vec::new();
    for device in &devices {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("logcat.txt").to_path_buf();
        let child =
            logcatx::adb::spawn_logcat(&adb, &device.serial, &log, &[]).expect("spawn logcat");
        children.push((child, dir.keep(), log));
    }
    std::thread::sleep(Duration::from_secs(6));
    for (child, _dir, _) in &mut children {
        child.kill().expect("kill logcat");
        child.wait().expect("reap logcat");
    }
    for (index, (_child, _dir, log)) in children.iter().enumerate() {
        let captured = std::fs::read(log).expect("log file readable");
        let text = String::from_utf8_lossy(&captured);
        println!(
            "device {}: {} bytes, {} lines",
            devices[index].serial,
            captured.len(),
            text.lines().count()
        );
        assert!(
            text.lines().count() > 5,
            "{} captured its own logcat stream",
            devices[index].serial
        );
    }
}

#[test]
#[ignore = "real device: set LOGCATX_REAL_ADB"]
fn multi_device_parallel_pushes_stay_unmixed() {
    let devices = require_two_devices();
    if devices.len() < 2 {
        return;
    }
    let adb = adb_path();
    let manager = TransferManager::new(&adb);
    let root = smoke_root();

    // Per-device smoke dir + a payload unique to that device. The tempdir
    // guards are kept alive in `planned` — pushes run asynchronously, so a
    // guard dropped at loop end would delete the source file mid-transfer.
    let mut planned = Vec::new();
    for (index, device) in devices.iter().enumerate() {
        let fs = RemoteFs::new(&adb, &device.serial);
        let dir = root.join(&format!("dev{index}")).expect("per-device dir");
        fs.mkdir(&dir).expect("mkdir per-device smoke dir");
        let local = tempfile::tempdir().expect("tempdir");
        let file = local.path().join("payload.txt").to_path_buf();
        let payload = format!(
            "device-specific payload for {} (index {index}, run {})",
            device.serial,
            std::process::id()
        );
        std::fs::write(&file, payload.as_bytes()).expect("write payload");
        let remote = format!("{}/payload.txt", dir.as_str());
        let id = manager.enqueue_push(device.serial.clone(), file, remote.clone());
        planned.push((
            device.serial.clone(),
            dir,
            remote,
            payload.into_bytes(),
            id,
            local,
        ));
    }

    // All transfers were queued up front; the per-device scheduler runs the
    // two devices concurrently. Each must end up with exactly its own bytes.
    for (serial, dir, remote, payload, id, _local) in &planned {
        let task = wait_terminal(&manager, *id);
        assert_eq!(
            task.state,
            TransferState::Completed,
            "push to {serial} completed: {:?}",
            task.error
        );
        let fs = RemoteFs::new(&adb, serial);
        let entries = fs.list(dir).expect("list per-device dir");
        assert_eq!(entries.len(), 1, "exactly one file on {serial}");
        assert_eq!(entries[0].size, payload.len() as u64);

        let back = tempfile::tempdir().expect("tempdir");
        let back_file = back.path().join("pulled.txt");
        let pull_id = manager.enqueue_pull(serial.clone(), remote.clone(), back_file.clone());
        let task = wait_terminal(&manager, pull_id);
        assert_eq!(task.state, TransferState::Completed);
        let pulled = std::fs::read(&back_file).expect("pulled file readable");
        assert_eq!(
            &pulled, payload,
            "{serial} received exactly its own payload — no cross-device mixing"
        );
    }
    manager.shutdown();

    for (serial, dir, _, _, _, _) in &planned {
        RemoteFs::new(&adb, serial)
            .delete(dir)
            .expect("cleanup per-device smoke dir");
    }
}
