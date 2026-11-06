//! Screen recording orchestration (UI-free).
//!
//! Two modes share the same task/stop surface:
//!
//! - [`RecordingMode::Quick`] runs `adb shell screenrecord` on the device,
//!   stops it with `SIGINT` (or its 180 s limit), then pulls the MP4 back.
//! - [`RecordingMode::Long`] runs scrcpy headless (`--no-playback`) and writes
//!   directly to the local path. scrcpy's SDL window is hidden; posting
//!   `WM_CLOSE` is the graceful stop that finalizes the MP4.

use crate::{adb_executor::AdbExecutor, fs_utils, scrcpy, win};
use chrono::Local;
use std::{
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant},
};
use tempfile::NamedTempFile;

/// Android's `screenrecord --time-limit` is documented as 180 s maximum.
pub const MAX_QUICK_SECONDS: u32 = 180;
const STOP_TIMEOUT: Duration = Duration::from_secs(10);
const STOP_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const PULL_TIMEOUT: Duration = Duration::from_secs(120);
const PULL_ATTEMPTS: u32 = 3;
const QUICK_GRACE: Duration = Duration::from_secs(30);
const LONG_MAX: Duration = Duration::from_secs(4 * 60 * 60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordingMode {
    Quick,
    Long,
}

#[derive(Clone, Debug)]
pub struct RecordingOutcome {
    pub mode: RecordingMode,
    pub local_path: PathBuf,
    pub duration: Duration,
    pub bytes: u64,
    /// The process had to be force-killed; the MP4 may be incomplete.
    pub forced_stop: bool,
}

#[derive(Debug)]
pub enum RecordingEvent {
    /// The adb/scrcpy child was spawned and recording is in progress.
    Started,
    Finished(Result<RecordingOutcome, String>),
}

/// Handle for one in-flight recording. Dropping it does not stop the worker;
/// call [`RecordingTask::request_stop`] or [`RecordingTask::join`] explicitly.
pub struct RecordingTask {
    pub mode: RecordingMode,
    pub local_path: PathBuf,
    stop: Arc<AtomicBool>,
    events: Receiver<RecordingEvent>,
    join: Option<thread::JoinHandle<()>>,
}

impl RecordingTask {
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    pub fn stop_requested(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    pub fn try_recv(&self) -> Option<RecordingEvent> {
        self.events.try_recv().ok()
    }

    /// Wait for the worker. Used on app shutdown after `request_stop`.
    pub fn join(&mut self) {
        if let Some(handle) = self.join.take() {
            let _ = handle.join();
        }
    }
}

pub fn start_quick(
    adb_path: &str,
    serial: &str,
    local_path: PathBuf,
    bitrate_mbps: u32,
    time_limit_secs: u32,
) -> Result<RecordingTask, String> {
    if time_limit_secs == 0 || time_limit_secs > MAX_QUICK_SECONDS {
        return Err(format!(
            "Quick recordings are limited to 1-{MAX_QUICK_SECONDS} seconds."
        ));
    }
    if bitrate_mbps == 0 || bitrate_mbps > 50 {
        return Err("Recording bitrate must be between 1 and 50 Mbps.".to_owned());
    }
    ensure_parent(&local_path)?;

    let device_path = quick_device_path();
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, events) = mpsc::channel();
    let adb_path = adb_path.to_owned();
    let serial = serial.to_owned();
    let worker_stop = Arc::clone(&stop);
    let worker_path = local_path.clone();
    let handle = thread::spawn(move || {
        let result = run_quick(
            &adb_path,
            &serial,
            &device_path,
            &worker_path,
            bitrate_mbps,
            time_limit_secs,
            &worker_stop,
            &tx,
        );
        let _ = tx.send(RecordingEvent::Finished(result));
    });

    Ok(RecordingTask {
        mode: RecordingMode::Quick,
        local_path,
        stop,
        events,
        join: Some(handle),
    })
}

pub fn start_long(
    scrcpy_path: &str,
    adb_path: &str,
    serial: &str,
    local_path: PathBuf,
    bitrate_mbps: u32,
) -> Result<RecordingTask, String> {
    if bitrate_mbps == 0 || bitrate_mbps > 50 {
        return Err("Recording bitrate must be between 1 and 50 Mbps.".to_owned());
    }
    let version = scrcpy::validate_scrcpy_path(scrcpy_path)?;
    if !version.supports_recording() {
        return Err(format!(
            "Long recording requires scrcpy 2.0 or later (found {version})."
        ));
    }
    ensure_parent(&local_path)?;

    let stop = Arc::new(AtomicBool::new(false));
    let (tx, events) = mpsc::channel();
    let scrcpy_path = scrcpy_path.to_owned();
    let adb_path = adb_path.to_owned();
    let serial = serial.to_owned();
    let worker_stop = Arc::clone(&stop);
    let worker_path = local_path.clone();
    let handle = thread::spawn(move || {
        let result = run_long(
            &scrcpy_path,
            &adb_path,
            &serial,
            &worker_path,
            bitrate_mbps,
            &worker_stop,
            &tx,
        );
        let _ = tx.send(RecordingEvent::Finished(result));
    });

    Ok(RecordingTask {
        mode: RecordingMode::Long,
        local_path,
        stop,
        events,
        join: Some(handle),
    })
}

/// Default file name under the configured recording folder.
pub fn default_recording_path(base_dir: &Path, device_label: &str) -> PathBuf {
    let mut stem = fs_utils::sanitize_serial(device_label);
    if stem.trim().is_empty() {
        stem = "device".to_owned();
    }
    let stamp = Local::now().format("%Y%m%d-%H%M%S");
    let mut candidate = base_dir.join(format!("{stem}-{stamp}.mp4"));
    let mut suffix = 1u32;
    while candidate.exists() {
        candidate = base_dir.join(format!("{stem}-{stamp}-{suffix}.mp4"));
        suffix += 1;
    }
    candidate
}

fn ensure_parent(path: &Path) -> Result<(), String> {
    let Some(parent) = path.parent() else {
        return Err(format!(
            "The recording path {} has no parent directory.",
            path.display()
        ));
    };
    fs::create_dir_all(parent).map_err(|err| {
        format!(
            "Failed to create the recording directory {}: {err}",
            parent.display()
        )
    })
}

fn quick_device_path() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    format!(
        "/data/local/tmp/logcatx-rec-{}-{nanos}.mp4",
        std::process::id()
    )
}

fn quick_command_args(
    serial: &str,
    device_path: &str,
    bitrate_mbps: u32,
    time_limit_secs: u32,
) -> Vec<String> {
    vec![
        "-s".to_owned(),
        serial.to_owned(),
        "shell".to_owned(),
        "screenrecord".to_owned(),
        "--bit-rate".to_owned(),
        (bitrate_mbps.saturating_mul(1_000_000)).to_string(),
        "--time-limit".to_owned(),
        time_limit_secs.to_string(),
        device_path.to_owned(),
    ]
}

#[allow(clippy::too_many_arguments)]
fn run_quick(
    adb_path: &str,
    serial: &str,
    device_path: &str,
    local_path: &Path,
    bitrate_mbps: u32,
    time_limit_secs: u32,
    stop: &AtomicBool,
    events: &Sender<RecordingEvent>,
) -> Result<RecordingOutcome, String> {
    let executor = AdbExecutor::new(adb_path);
    let stderr_log = NamedTempFile::new()
        .map_err(|err| format!("Failed to create a screenrecord log file: {err}"))?;
    let stderr = stderr_log
        .as_file()
        .try_clone()
        .map_err(|err| format!("Failed to prepare the screenrecord log file: {err}"))?;

    let args = quick_command_args(serial, device_path, bitrate_mbps, time_limit_secs);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut child = executor
        .spawn_streaming(&args, Stdio::null(), Stdio::from(stderr))
        .map_err(|err| format!("Failed to start screenrecord on {serial}: {err}"))?;

    let started = Instant::now();
    let _ = events.send(RecordingEvent::Started);
    let deadline = started + Duration::from_secs(u64::from(time_limit_secs)) + QUICK_GRACE;
    let mut stop_sent = false;
    let mut forced_stop = false;
    let exit_status: Option<std::process::ExitStatus>;

    loop {
        if !stop_sent && stop.load(Ordering::SeqCst) {
            stop_sent = true;
            if let Err(err) = stop_screenrecord(&executor, serial) {
                log::warn!("Failed to signal screenrecord on {serial}: {err}");
            }
        }

        match child.try_wait() {
            Ok(Some(status)) => {
                exit_status = Some(status);
                break;
            }
            Ok(None) => {}
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "Failed while waiting for screenrecord on {serial}: {err}"
                ));
            }
        }

        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            forced_stop = true;
            exit_status = None;
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }

    let duration = started.elapsed();
    let stderr_text = fs::read_to_string(stderr_log.path()).unwrap_or_default();
    // screenrecord finalizes the MP4 just before it exits; give the device a
    // moment before pulling, then retry if the file is still being flushed.
    thread::sleep(Duration::from_millis(300));
    let bytes = pull_recording(&executor, serial, device_path, local_path).map_err(|err| {
        format!(
            "Screen recording finished but the file could not be copied ({err}). \
             It was kept on the device at {device_path}.{}",
            if stderr_text.trim().is_empty() {
                String::new()
            } else {
                format!(" screenrecord output: {}", stderr_text.trim())
            }
        )
    })?;

    if !forced_stop && !stop_sent && !exit_status.is_some_and(|status| status.success()) {
        // A nonzero exit can still leave a fully pulled MP4 (some devices
        // report the stop signal as an error). Keep the file and surface the
        // details in the app log instead of discarding the user's recording.
        log::warn!(
            "screenrecord on {serial} exited with an error after producing a file: {}",
            stderr_text.trim()
        );
    }

    Ok(RecordingOutcome {
        mode: RecordingMode::Quick,
        local_path: local_path.to_path_buf(),
        duration,
        bytes,
        forced_stop,
    })
}

fn stop_screenrecord(executor: &AdbExecutor, serial: &str) -> Result<(), String> {
    let mut last_error = String::new();
    for args in [
        ["shell", "pkill", "-INT", "screenrecord"],
        ["shell", "killall", "-INT", "screenrecord"],
    ] {
        let mut full = vec!["-s", serial];
        full.extend_from_slice(&args);
        match executor.execute_with_timeout(&full, STOP_COMMAND_TIMEOUT) {
            Ok(output) if output.success() => return Ok(()),
            Ok(output) => {
                last_error = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            }
            Err(err) => last_error = err.to_string(),
        }
    }

    let script = "kill -2 $(pidof screenrecord) 2>/dev/null";
    let quoted = crate::remote_fs::shell_quote(script);
    let output = executor
        .execute_with_timeout(
            &["-s", serial, "shell", "sh", "-c", &quoted],
            STOP_COMMAND_TIMEOUT,
        )
        .map_err(|err| format!("Failed to stop screenrecord on {serial}: {err}"))?;
    if output.success() {
        Ok(())
    } else {
        Err(format!(
            "Failed to stop screenrecord on {serial}: {}",
            String::from_utf8_lossy(&output.stderr)
                .trim()
                .if_empty(last_error.as_str())
        ))
    }
}

fn pull_recording(
    executor: &AdbExecutor,
    serial: &str,
    device_path: &str,
    local_path: &Path,
) -> Result<u64, String> {
    let local = local_path.to_string_lossy().into_owned();
    let mut last_error = String::new();
    for attempt in 0..PULL_ATTEMPTS {
        match executor
            .execute_with_timeout(&["-s", serial, "pull", device_path, &local], PULL_TIMEOUT)
        {
            Ok(output) if output.success() => match fs::metadata(local_path) {
                Ok(metadata) if metadata.len() > 0 => {
                    let bytes = metadata.len();
                    let _ = executor.execute_with_timeout(
                        &[
                            "-s",
                            serial,
                            "shell",
                            "rm",
                            "-f",
                            &crate::remote_fs::shell_quote(device_path),
                        ],
                        STOP_COMMAND_TIMEOUT,
                    );
                    return Ok(bytes);
                }
                Ok(_) => {
                    last_error = "the pulled file is empty".to_owned();
                }
                Err(err) => {
                    last_error = err.to_string();
                }
            },
            Ok(output) => {
                last_error = String::from_utf8_lossy(&output.stderr).trim().to_owned();
            }
            Err(err) => last_error = err.to_string(),
        }
        if attempt + 1 < PULL_ATTEMPTS {
            thread::sleep(Duration::from_millis(400));
        }
    }
    Err(last_error.if_empty("unknown adb pull error").to_owned())
}

fn run_long(
    scrcpy_path: &str,
    adb_path: &str,
    serial: &str,
    local_path: &Path,
    bitrate_mbps: u32,
    stop: &AtomicBool,
    events: &Sender<RecordingEvent>,
) -> Result<RecordingOutcome, String> {
    let mut process =
        scrcpy::spawn_recording(scrcpy_path, adb_path, serial, local_path, bitrate_mbps)?;
    let started = Instant::now();
    let _ = events.send(RecordingEvent::Started);

    let hide_until = started + Duration::from_secs(3);
    let mut close_requested_at: Option<Instant> = None;
    let mut last_close_attempt = Instant::now()
        .checked_sub(Duration::from_secs(1))
        .unwrap_or_else(Instant::now);
    let mut forced_stop = false;

    loop {
        if Instant::now() < hide_until {
            win::hide_windows_for_pid(process.pid);
        }
        let should_stop = stop.load(Ordering::SeqCst) || started.elapsed() >= LONG_MAX;
        if should_stop {
            if close_requested_at.is_none() {
                close_requested_at = Some(Instant::now());
            }
            if last_close_attempt.elapsed() >= Duration::from_millis(500) {
                last_close_attempt = Instant::now();
                win::close_windows_for_pid(process.pid);
            }
            if close_requested_at.is_some_and(|at| at.elapsed() >= STOP_TIMEOUT) {
                let _ = process.child.kill();
                forced_stop = true;
            }
        }

        match process.child.try_wait() {
            Ok(Some(status)) => {
                let duration = started.elapsed();
                let log = fs::read_to_string(process.log.path()).unwrap_or_default();
                let bytes = fs::metadata(local_path).map(|meta| meta.len()).unwrap_or(0);
                if bytes == 0 {
                    return Err(format!(
                        "scrcpy recording on {serial} produced no output: {}",
                        log.trim().if_empty("the output file is empty")
                    ));
                }
                if forced_stop {
                    return Ok(RecordingOutcome {
                        mode: RecordingMode::Long,
                        local_path: local_path.to_path_buf(),
                        duration,
                        bytes,
                        forced_stop: true,
                    });
                }
                if !status.success() {
                    return Err(format!(
                        "scrcpy recording on {serial} exited with an error; the file was kept at {}: {}",
                        local_path.display(),
                        log.trim().if_empty("unknown scrcpy error")
                    ));
                }
                return Ok(RecordingOutcome {
                    mode: RecordingMode::Long,
                    local_path: local_path.to_path_buf(),
                    duration,
                    bytes,
                    forced_stop: false,
                });
            }
            Ok(None) => {}
            Err(err) => {
                let _ = process.child.kill();
                let _ = process.child.wait();
                return Err(format!(
                    "Failed while waiting for the scrcpy recording on {serial}: {err}"
                ));
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
}

trait EmptyStringExt {
    fn if_empty<'a>(&'a self, fallback: &'a str) -> &'a str;
}

impl EmptyStringExt for str {
    fn if_empty<'a>(&'a self, fallback: &'a str) -> &'a str {
        if self.trim().is_empty() {
            fallback
        } else {
            self
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_QUICK_SECONDS, default_recording_path, quick_command_args};

    #[test]
    fn quick_command_carries_limit_bitrate_and_device_path() {
        let args = quick_command_args("emulator-5554", "/data/local/tmp/x.mp4", 8, 180);
        assert_eq!(
            args,
            vec![
                "-s",
                "emulator-5554",
                "shell",
                "screenrecord",
                "--bit-rate",
                "8000000",
                "--time-limit",
                "180",
                "/data/local/tmp/x.mp4",
            ]
        );
        assert_eq!(MAX_QUICK_SECONDS, 180);
    }

    #[test]
    fn default_path_sanitizes_the_device_label_and_avoids_collisions() {
        let dir = tempfile::tempdir().expect("temp dir");
        let first = default_recording_path(dir.path(), "Pixel 8 / USB");
        let name = first.file_name().unwrap().to_string_lossy();
        assert!(name.starts_with("Pixel_8"), "{name}");
        assert!(!name.contains('/') && !name.contains('\\'), "{name}");
        assert!(name.ends_with(".mp4"), "{name}");
        std::fs::write(&first, b"existing").expect("seed existing file");
        let second = default_recording_path(dir.path(), "Pixel 8 / USB");
        assert_ne!(first, second);
        assert!(
            second
                .file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with("-1.mp4")
        );
    }
}
