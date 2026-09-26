//! End-to-end exercise of the shipped update helper binary (PRD §45
//! Test 11 essence, headless): whitelist replacement, ack-success cleanup,
//! bootstrap-failure rollback, and undeclared/missing-file rejection.
//!
//! Runs the real `logcatx-updater` bin target against temp install
//! directories. Batch files stand in for the restarted application —
//! `ack.bat` writes the acknowledgement env path (what LogcatX.exe does on
//! its first rendered frame, PRD §32), `fail.bat` exits immediately to
//! simulate a build that dies before acknowledging (the rollback scenario).

#![cfg(windows)]

use serde_json::json;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// Must stay in sync with desktop-update.toml and RELEASE_REPLACE_FILES.
const REPLACE_FILES: [&str; 9] = [
    "LogcatX.exe",
    "LogcatX.Updater.exe",
    "README.md",
    "README.en.md",
    "CHANGELOG.md",
    "CHANGELOG.en.md",
    "LICENSE",
    "config.example.json",
    "icons/icon_128.png",
];

fn write_file(path: &Path, contents: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create parent dir");
    }
    std::fs::write(path, contents).expect("write file");
}

fn read(path: &Path) -> String {
    String::from_utf8_lossy(&std::fs::read(path).expect("read file")).into_owned()
}

/// Build a release-layout install dir with ORIGINAL contents per file.
fn install_dir(root: &Path) -> PathBuf {
    let dir = root.join("install");
    for relative in REPLACE_FILES {
        write_file(
            &dir.join(relative),
            format!("ORIGINAL {relative}").as_bytes(),
        );
    }
    dir
}

/// Build an update package whose files carry NEW contents; README.md gets
/// the demo marker when requested.
fn package(root: &Path, readme_marker: bool) -> PathBuf {
    let path = root.join("update.zip");
    let file = std::fs::File::create(&path).expect("create package");
    let mut writer = zip::ZipWriter::new(file);
    let options =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for relative in REPLACE_FILES {
        let contents = if readme_marker && relative == "README.md" {
            format!("NEW {relative}\nDEMO marker\n").into_bytes()
        } else {
            format!("NEW {relative}").into_bytes()
        };
        writer
            .start_file(relative, options)
            .expect("zip entry start");
        writer.write_all(&contents).expect("zip entry write");
    }
    writer.finish().expect("zip finish");
    path
}

fn batch(root: &Path, name: &str, body: &str) -> PathBuf {
    let path = root.join(name);
    write_file(&path, body.as_bytes());
    path
}

/// A process that has already exited, satisfying the helper's
/// wait-for-parent gate without a live parent.
fn dead_parent_pid() -> u32 {
    let mut child = Command::new("cmd")
        .args(["/c", "exit"])
        .spawn()
        .expect("spawn");
    let pid = child.id();
    child.wait().expect("reap placeholder parent");
    pid
}

fn plan_json(
    root: &Path,
    install: &Path,
    package_path: &Path,
    restart: &Path,
    replace_files: &[&str],
) -> PathBuf {
    let ack = root.join("ack");
    let journal = root.join("journal.json");
    let plan = root.join("plan.json");
    let value = json!({
        "schemaVersion": 1,
        "id": "helper-smoke",
        "parentPid": dead_parent_pid(),
        "packagePath": package_path,
        "installDir": install,
        "restartExecutable": restart,
        "layout": {
            "replaceFiles": replace_files,
            "preserveFiles": [],
        },
        "ackPath": ack,
        "stagingDir": root.join("staging"),
        "backupDir": root.join("backup"),
        "journalPath": journal,
    });
    write_file(
        &plan,
        serde_json::to_string(&value)
            .expect("serialize plan")
            .as_bytes(),
    );
    plan
}

/// Copy the helper to a run location, exactly like the app does before
/// launching it (the archive replaces LogcatX.Updater.exe in place).
fn run_helper(plan: &Path, root: &Path) -> (Option<i32>, String) {
    let run = root.join("logcatx-updater-run.exe");
    std::fs::copy(
        std::env::var("CARGO_BIN_EXE_logcatx-updater").expect("helper bin built"),
        &run,
    )
    .expect("copy helper");
    let output = Command::new(&run)
        .arg("--desktop-updater-apply-plan")
        .arg(plan)
        .output()
        .expect("run helper");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn helper_applies_update_and_cleans_up_after_ack() {
    let root = tempfile::tempdir().expect("tempdir");
    let root = root.path();
    let install = install_dir(root);
    let package = package(root, true);
    let ack = batch(
        root,
        "ack.bat",
        "@echo ok> \"%DESKTOP_UPDATER_ACK_PATH%\"\r\n",
    );
    let plan = plan_json(root, &install, &package, &ack, &REPLACE_FILES);

    let (code, stderr) = run_helper(&plan, root);
    assert_eq!(code, Some(0), "helper succeeds after ack: {stderr}");

    // Every whitelist file carries the new content; README has the marker.
    for relative in REPLACE_FILES {
        let text = read(&install.join(relative));
        assert!(
            text.starts_with(&format!("NEW {relative}")),
            "{relative}: {text:?}"
        );
    }
    assert!(read(&install.join("README.md")).contains("DEMO marker"));

    // Success cleanup removes all update working state.
    for dir in ["staging", "backup"] {
        assert!(!root.join(dir).exists(), "{dir} cleaned");
    }
    for file in ["plan.json", "journal.json", "ack", "update.zip"] {
        assert!(!root.join(file).exists(), "{file} cleaned");
    }
}

#[test]
fn helper_rolls_back_when_restart_never_acknowledges() {
    let root = tempfile::tempdir().expect("tempdir");
    let root = root.path();
    let install = install_dir(root);
    let package = package(root, true);
    let fail = batch(root, "fail.bat", "@exit /b 1\r\n");
    let plan = plan_json(root, &install, &package, &fail, &REPLACE_FILES);

    let (code, stderr) = run_helper(&plan, root);
    assert_ne!(code, Some(0), "helper fails when bootstrap never acks");
    assert!(
        stderr.contains("did not acknowledge"),
        "failure names the bootstrap ack: {stderr}"
    );

    // Rollback restored every whitelist file to its original content.
    for relative in REPLACE_FILES {
        let text = read(&install.join(relative));
        assert!(
            text.starts_with(&format!("ORIGINAL {relative}")),
            "{relative}: {text:?}"
        );
    }
    assert!(!read(&install.join("README.md")).contains("DEMO marker"));
}

#[test]
fn helper_rejects_package_missing_declared_file() {
    let root = tempfile::tempdir().expect("tempdir");
    let root = root.path();
    let install = install_dir(root);
    let package = package(root, true);
    // Drop LICENSE from the archive by rebuilding a package without it.
    let short = root.join("short.zip");
    let file = std::fs::File::create(&short).expect("create package");
    let mut writer = zip::ZipWriter::new(file);
    let options =
        zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
    for relative in REPLACE_FILES.iter().filter(|r| **r != "LICENSE") {
        writer
            .start_file(*relative, options)
            .expect("zip entry start");
        writer
            .write_all(format!("NEW {relative}").as_bytes())
            .expect("zip entry write");
    }
    writer.finish().expect("zip finish");
    drop(package);

    let ack = batch(
        root,
        "ack.bat",
        "@echo ok> \"%DESKTOP_UPDATER_ACK_PATH%\"\r\n",
    );
    let plan = plan_json(root, &install, &short, &ack, &REPLACE_FILES);

    let (code, stderr) = run_helper(&plan, root);
    assert_ne!(code, Some(0), "layout mismatch must fail");
    assert!(
        stderr.contains("missing declared file"),
        "failure names the missing whitelist entry: {stderr}"
    );
    // Nothing was replaced.
    for relative in REPLACE_FILES {
        assert!(read(&install.join(relative)).starts_with(&format!("ORIGINAL {relative}")));
    }
}

#[test]
#[ignore = "slow: exercises the helper's 60s acknowledgement timeout"]
fn helper_ack_timeout_eventually_rolls_back() {
    // A restart that neither exits nor acks exercises the timeout branch.
    // The stand-in self-terminates shortly after the window so nothing
    // lingers; the rollback re-spawn is by design and equally short-lived.
    let root = tempfile::tempdir().expect("tempdir");
    let root = root.path();
    let install = install_dir(root);
    let package = package(root, true);
    let hang = batch(root, "hang.bat", "@ping -n 70 127.0.0.1 > nul\r\n");
    let plan = plan_json(root, &install, &package, &hang, &REPLACE_FILES);

    let started = Instant::now();
    let (code, stderr) = run_helper(&plan, root);
    assert_ne!(code, Some(0), "helper fails on ack timeout");
    assert!(
        stderr.contains("did not acknowledge"),
        "failure names the bootstrap ack: {stderr}"
    );
    assert!(
        started.elapsed() >= Duration::from_secs(60),
        "helper honored its acknowledgement window before rolling back"
    );
    for relative in REPLACE_FILES {
        assert!(read(&install.join(relative)).starts_with(&format!("ORIGINAL {relative}")));
    }
}
