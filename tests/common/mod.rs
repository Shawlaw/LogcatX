//! Shared harness for driving the `fake_adb` binary (PRD §43).
//!
//! Usage in an integration test:
//!
//! ```ignore
//! mod common;
//! let output = common::run("devices => out:List of devices attached\\n...", &["devices"]);
//! ```

use std::{
    path::PathBuf,
    process::{Child, Command, Output},
};

pub fn exe() -> &'static str {
    env!("CARGO_BIN_EXE_fake_adb")
}

/// Write a scenario script into a fresh temp dir and return its path.
/// Keep the returned `TempDir` alive while the fake adb runs.
pub struct Scenario {
    pub script: PathBuf,
    _dir: tempfile::TempDir,
}

pub fn scenario(contents: &str) -> Scenario {
    let dir = tempfile::tempdir().expect("create scenario temp dir");
    let script = dir.path().join("scenario.txt");
    std::fs::write(&script, contents).expect("write scenario script");
    Scenario { script, _dir: dir }
}

pub fn command(script: &Scenario, args: &[&str]) -> Command {
    let mut command = Command::new(exe());
    command.env("FAKE_ADB_SCRIPT", &script.script).args(args);
    command
}

/// Run the fake adb to completion and collect its output.
pub fn run(contents: &str, args: &[&str]) -> Output {
    let scenario = scenario(contents);
    command(&scenario, args).output().expect("spawn fake_adb")
}

/// Spawn the fake adb without waiting (hang/stream/cancel scenarios).
pub fn spawn(contents: &str, args: &[&str]) -> (Scenario, Child) {
    let scenario = scenario(contents);
    let child = command(&scenario, args).spawn().expect("spawn fake_adb");
    (scenario, child)
}
