//! RemoteFs over the fake adb double (PRD §7/§8/§45 Test 6 essence):
//! machine-parsed listings, permission/not-found distinction, and special
//! filenames (spaces, CJK, `$`) through every operation's quoting.

mod common;

use logcatx::remote_fs::{RemoteEntryKind, RemoteFs, RemoteFsError, RemotePath};
use std::sync::{Mutex, OnceLock};

static SCRIPT_READY: OnceLock<()> = OnceLock::new();
static SCRIPT_GUARD: Mutex<()> = Mutex::new(());

/// Listing payload for a 3000-entry directory: one
/// `kind|size|mtime|name` line per entry, as `stat -c '%F|%s|%Y|%n'`
/// emits. Regression target for the 0.9.0 timeout — the protocol must
/// carry thousands of entries in one round-trip and parse them all.
fn big_listing_payload() -> String {
    let mut payload = String::new();
    for i in 0..3000 {
        payload.push_str(&format!(
            "regular empty file\\|{}\\|1700000000\\|file_{i:04}.txt\\n",
            i * 7
        ));
    }
    payload
}

fn remote_fs() -> RemoteFs {
    SCRIPT_READY.get_or_init(|| {
        let _guard = SCRIPT_GUARD
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempfile::tempdir().expect("tempdir").keep();
        let script_path = dir.join("scenario.txt");
        std::fs::write(
            &script_path,
            format!(
                "shell cd '/data/files' * => out:regular file\\|10\\|1\\|hello world.txt\\nregular empty file\\|30\\|3\\|中文文件.txt\\ndirectory\\|0\\|0\\|子目录\\n | exit:0\n\
                 shell cd '/data/empty' * => exit:0\n\
                 shell cd '/data/missing' * => err:sh: cd: /data/missing: No such file or directory | exit:42\n\
                 shell cd '/data/ptymissing' * => out:sh: cd: /data/ptymissing: No such file or directory | exit:42\n\
                 shell cd '/data/ptylocked' * => out:sh: cd: /data/ptylocked: Permission denied | exit:42\n\
                 shell run-as com.example sh -c * => out:directory\\|0\\|1\\|cache\\ndirectory\\|0\\|2\\|files\\n | exit:0\n\
                 shell cd '/data/locked' * => err:sh: cd: /data/locked: Permission denied | exit:42\n\
                 shell mkdir -p '/data/新建 目录' => exit:0\n\
                 shell mv '/data/a b.txt' '/data/a b2.txt' => exit:0\n\
                 shell rm -rf '/data/删除 我' => exit:0\n\
                 shell if [ -d '/data/a$b.txt' ]* => out:f\\n | exit:0\n\
                 shell cd '/data/big' * => out:{big} | exit:0\n",
                big = big_listing_payload(),
            ),
        )
        .expect("write script");
        unsafe {
            std::env::set_var("FAKE_ADB_SCRIPT", &script_path);
        }
    });
    RemoteFs::new(common::exe(), "device-a")
}

#[test]
fn list_parses_special_names_from_machine_protocol() {
    let fs = remote_fs();
    let entries = fs
        .list(&RemotePath::new("/data/files").unwrap())
        .expect("listing succeeds");
    assert_eq!(entries.len(), 3);
    assert_eq!(entries[0].name, "hello world.txt");
    assert_eq!(entries[0].kind, RemoteEntryKind::File);
    assert_eq!(entries[0].size, 10);
    assert_eq!(entries[1].name, "中文文件.txt");
    assert_eq!(entries[1].size, 30);
    assert_eq!(entries[2].name, "子目录");
    assert_eq!(entries[2].kind, RemoteEntryKind::Directory);
}

#[test]
fn list_reports_empty_directory() {
    let fs = remote_fs();
    let entries = fs
        .list(&RemotePath::new("/data/empty").unwrap())
        .expect("empty directory lists");
    assert!(entries.is_empty());
}

/// 0.9.0 regression: large listings timed out (per-entry stat forks, plus
/// OEM-throttled shell loops at 5-15 ms per iteration) or, when they
/// finished past the 1 MiB capture cap, silently dropped the truncated
/// tail. The loop-free batched protocol must return every entry with its
/// metadata.
#[test]
fn list_parses_large_directory_listing() {
    let fs = remote_fs();
    let entries = fs
        .list(&RemotePath::new("/data/big").unwrap())
        .expect("3000-entry listing succeeds");
    assert_eq!(entries.len(), 3000);
    assert_eq!(entries[0].name, "file_0000.txt");
    assert_eq!(entries[0].kind, RemoteEntryKind::File);
    assert_eq!(entries[0].size, 0);
    assert_eq!(entries[42].size, 42 * 7);
    assert_eq!(entries[42].modified_unix_secs, Some(1700000000));
    assert_eq!(entries[2999].name, "file_2999.txt");
    assert_eq!(entries[2999].size, 2999 * 7);
}

#[test]
fn list_distinguishes_not_found_from_permission() {
    let fs = remote_fs();
    let missing = fs
        .list(&RemotePath::new("/data/missing").unwrap())
        .unwrap_err();
    assert!(matches!(missing, RemoteFsError::NotFound), "{missing:?}");
    let locked = fs
        .list(&RemotePath::new("/data/locked").unwrap())
        .unwrap_err();
    assert!(matches!(locked, RemoteFsError::NoPermission), "{locked:?}");
}

/// Regression (found on real hardware 2026-09-27): pty-style transports and
/// devices that suppress stderr used to collapse every listing failure into
/// a generic DeviceError. When the diagnostic rides on stdout instead, the
/// NotFound / NoPermission classification must survive.
#[test]
fn list_classifies_from_stdout_when_stderr_is_empty() {
    let fs = remote_fs();
    let missing = fs
        .list(&RemotePath::new("/data/ptymissing").unwrap())
        .unwrap_err();
    assert!(matches!(missing, RemoteFsError::NotFound), "{missing:?}");
    let locked = fs
        .list(&RemotePath::new("/data/ptylocked").unwrap())
        .unwrap_err();
    assert!(matches!(locked, RemoteFsError::NoPermission), "{locked:?}");
}

/// Regression (found on a real Android 16 device): `run-as <pkg> <script>`
/// makes run-as exec the script's first word as a binary ("exec failed for
/// cd"). The RemoteFs run-as path must route through `sh -c` with the
/// script single-quoted, and the fake scenario pins that argv shape.
#[test]
fn run_as_listing_routes_through_sh_c() {
    // This test constructs the run-as RemoteFs directly, so the shared
    // scenario setup (process-wide FAKE_ADB_SCRIPT) must fire first.
    drop(remote_fs());
    let fs = RemoteFs::new_run_as(common::exe(), "device-a", "com.example");
    let entries = fs
        .list(&RemotePath::new("/data/data/com.example").unwrap())
        .expect("run-as listing executes");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].name, "cache");
    assert_eq!(entries[0].kind, RemoteEntryKind::Directory);
    assert_eq!(entries[1].name, "files");
}

#[test]
fn mkdir_rename_delete_accept_special_paths() {
    let fs = remote_fs();
    fs.mkdir(&RemotePath::new("/data/新建 目录").unwrap())
        .expect("mkdir with CJK and space");
    fs.rename(
        &RemotePath::new("/data/a b.txt").unwrap(),
        &RemotePath::new("/data/a b2.txt").unwrap(),
    )
    .expect("rename with space");
    fs.delete(&RemotePath::new("/data/删除 我").unwrap())
        .expect("delete with CJK and space");
}

#[test]
fn stat_probe_reports_kind_for_dollar_names() {
    let fs = remote_fs();
    let kind = fs
        .stat(&RemotePath::new("/data/a$b.txt").unwrap())
        .expect("stat succeeds");
    assert_eq!(kind, RemoteEntryKind::File);
}

#[test]
fn invalid_paths_never_reach_the_device() {
    // Path validation happens before any adb call: the constructor cannot
    // even express a relative path.
    let error = RemotePath::new("relative/path").unwrap_err();
    assert!(matches!(error, RemoteFsError::InvalidPath(_)));
}
