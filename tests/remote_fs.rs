//! RemoteFs over the fake adb double (PRD §7/§8/§45 Test 6 essence):
//! machine-parsed listings, permission/not-found distinction, and special
//! filenames (spaces, CJK, `$`) through every operation's quoting.

mod common;

use logcatx::remote_fs::{RemoteEntryKind, RemoteFs, RemoteFsError, RemotePath};
use std::sync::{Mutex, OnceLock};

static SCRIPT_READY: OnceLock<()> = OnceLock::new();
static SCRIPT_GUARD: Mutex<()> = Mutex::new(());

fn remote_fs() -> RemoteFs {
    SCRIPT_READY.get_or_init(|| {
        let _guard = SCRIPT_GUARD
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let dir = tempfile::tempdir().expect("tempdir").keep();
        let script_path = dir.join("scenario.txt");
        std::fs::write(
            &script_path,
            concat!(
                "shell cd '/data/files' * => out:hello world.txt\\nregular file\\t10\\t1\\n",
                "中文文件.txt\\nregular file\\t30\\t3\\n | exit:0\n",
                "shell cd '/data/empty' * => out:*\\nERR\\n | exit:0\n",
                "shell cd '/data/missing' * => err:sh: cd: /data/missing: No such file or directory | exit:42\n",
                "shell cd '/data/locked' * => err:sh: cd: /data/locked: Permission denied | exit:42\n",
                "shell mkdir -p '/data/新建 目录' => exit:0\n",
                "shell mv '/data/a b.txt' '/data/a b2.txt' => exit:0\n",
                "shell rm -rf '/data/删除 我' => exit:0\n",
                "shell stat -c '%F' '/data/a$b.txt' * => out:regular file\\n | exit:0\n",
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
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].name, "hello world.txt");
    assert_eq!(entries[0].kind, RemoteEntryKind::File);
    assert_eq!(entries[0].size, 10);
    assert_eq!(entries[1].name, "中文文件.txt");
    assert_eq!(entries[1].size, 30);
}

#[test]
fn list_reports_empty_directory() {
    let fs = remote_fs();
    let entries = fs
        .list(&RemotePath::new("/data/empty").unwrap())
        .expect("empty directory lists");
    assert!(entries.is_empty());
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
