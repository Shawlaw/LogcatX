//! Unified transfer engine for adb push/pull (PRD §10-§13).
//!
//! One queue owns every file transfer. Per-device concurrency is capped
//! (default 2) so wireless ADB is not starved by parallel pushes. Each
//! running transfer:
//!
//! - streams adb's stderr progress markers into shared task state
//!   (percentage, bytes, total, speed; `None` total = indeterminate),
//! - supports cancellation at any time (token → child kill → reap; no
//!   zombie processes, other devices unaffected),
//! - is supervised by activity rather than a wall-clock timeout: silence
//!   for `stall_timeout` kills the transfer as stalled (PRD §15),
//! - never loads file contents into memory; only a bounded stderr tail is
//!   kept for error reporting (PRD §42).

use std::{
    collections::HashMap,
    fmt,
    io::Read,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant},
};

use crate::{
    adb_executor::{AdbExecutor, CancelToken},
    managed_child::ManagedChild,
};

pub type TransferId = u64;

/// Default per-device parallel transfers (PRD §11: 1-2 per device).
pub const DEFAULT_PER_DEVICE_CONCURRENCY: usize = 2;
/// Default inactivity budget before a transfer is treated as stalled.
pub const DEFAULT_STALL_TIMEOUT: Duration = Duration::from_secs(60);
/// How much child output is kept for error messages.
const TAIL_LIMIT: usize = 4 * 1024;
/// Snapshot poll cadence of the scheduler loop.
const SCHEDULER_TICK: Duration = Duration::from_millis(50);
/// Worker poll cadence for progress/cancel/exit checks.
const WORKER_TICK: Duration = Duration::from_millis(100);

#[derive(Clone, Debug, PartialEq)]
pub enum TransferOperation {
    Push {
        source: PathBuf,
        destination: String,
    },
    Pull {
        source: String,
        destination: PathBuf,
    },
}

impl TransferOperation {
    pub fn describe(&self) -> String {
        match self {
            TransferOperation::Push {
                source,
                destination,
            } => format!("push {} -> {destination}", source.display()),
            TransferOperation::Pull {
                source,
                destination,
            } => format!("pull {source} -> {}", destination.display()),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransferState {
    Queued,
    Preparing,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl TransferState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TransferState::Completed | TransferState::Failed | TransferState::Cancelled
        )
    }

    pub fn is_active(self) -> bool {
        !self.is_terminal()
    }
}

#[derive(Clone, Debug)]
pub struct TransferTask {
    pub id: TransferId,
    pub device: String,
    pub operation: TransferOperation,
    pub state: TransferState,
    pub bytes_transferred: u64,
    /// `None` = indeterminate (PRD §12).
    pub bytes_total: Option<u64>,
    pub speed_bps: Option<u64>,
    pub error: Option<String>,
}

impl TransferTask {
    pub fn progress_fraction(&self) -> Option<f64> {
        self.bytes_total
            .filter(|total| *total > 0)
            .map(|total| (self.bytes_transferred as f64 / total as f64).clamp(0.0, 1.0))
    }
}

#[derive(Debug)]
pub enum TransferError {
    Cancelled,
    Stalled,
    Spawn(String),
    AdbExit(String),
}

impl fmt::Display for TransferError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransferError::Cancelled => write!(f, "cancelled"),
            TransferError::Stalled => write!(f, "transfer stalled"),
            TransferError::Spawn(source) => write!(f, "failed to start adb: {source}"),
            TransferError::AdbExit(detail) => write!(f, "adb failed: {detail}"),
        }
    }
}

enum ManagerCommand {
    Enqueue {
        id: TransferId,
        device: String,
        operation: TransferOperation,
    },
    Cancel(TransferId),
    CancelAll,
    Retry(TransferId),
    RetryFailed,
    ClearFinished,
    Shutdown,
}

/// Handle owned by the UI thread; the scheduler thread owns the queue.
#[derive(Clone)]
pub struct TransferManager {
    cmd_tx: Sender<ManagerCommand>,
    snapshot: Arc<Mutex<Vec<TransferTask>>>,
}

impl TransferManager {
    /// Start the manager with default concurrency/stall settings.
    pub fn new(adb_path: impl Into<String>) -> Self {
        Self::with_options(adb_path, TransferOptions::default())
    }

    pub fn with_options(adb_path: impl Into<String>, options: TransferOptions) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let snapshot = Arc::new(Mutex::new(Vec::new()));
        let scheduler = Scheduler {
            executor: AdbExecutor::new(adb_path),
            options,
            tasks: Vec::new(),
            cancels: HashMap::new(),
            snapshot: snapshot.clone(),
        };
        thread::Builder::new()
            .name("transfer-scheduler".into())
            .spawn(move || scheduler.run(cmd_rx))
            .expect("spawn transfer scheduler");
        Self { cmd_tx, snapshot }
    }

    pub fn enqueue_push(
        &self,
        device: impl Into<String>,
        source: PathBuf,
        destination: String,
    ) -> TransferId {
        self.enqueue_device(
            device,
            TransferOperation::Push {
                source,
                destination,
            },
        )
    }

    pub fn enqueue_pull(
        &self,
        device: impl Into<String>,
        source: String,
        destination: PathBuf,
    ) -> TransferId {
        self.enqueue_device(
            device,
            TransferOperation::Pull {
                source,
                destination,
            },
        )
    }

    fn enqueue_device(
        &self,
        device: impl Into<String>,
        operation: TransferOperation,
    ) -> TransferId {
        // The id is minted here and carried through the command so the
        // caller's handle matches the scheduler's record exactly.
        let id = next_transfer_id();
        let _ = self.cmd_tx.send(ManagerCommand::Enqueue {
            id,
            device: device.into(),
            operation,
        });
        id
    }

    /// Cancel a queued or running transfer.
    pub fn cancel(&self, id: TransferId) {
        let _ = self.cmd_tx.send(ManagerCommand::Cancel(id));
    }

    pub fn cancel_all(&self) {
        let _ = self.cmd_tx.send(ManagerCommand::CancelAll);
    }

    /// Re-queue a failed transfer (PRD §11 Failed retry).
    pub fn retry(&self, id: TransferId) {
        let _ = self.cmd_tx.send(ManagerCommand::Retry(id));
    }

    pub fn retry_failed(&self) {
        let _ = self.cmd_tx.send(ManagerCommand::RetryFailed);
    }

    pub fn clear_finished(&self) {
        let _ = self.cmd_tx.send(ManagerCommand::ClearFinished);
    }

    /// Point-in-time task list for rendering.
    pub fn snapshot(&self) -> Vec<TransferTask> {
        self.snapshot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub fn has_active(&self) -> bool {
        self.snapshot().iter().any(|task| task.state.is_active())
    }

    /// Stop the scheduler, cancelling every active transfer. The manager
    /// must not be used afterwards.
    pub fn shutdown(&self) {
        let _ = self.cmd_tx.send(ManagerCommand::Shutdown);
    }
}

static NEXT_TRANSFER_ID: AtomicU64 = AtomicU64::new(0);

fn next_transfer_id() -> TransferId {
    NEXT_TRANSFER_ID.fetch_add(1, Ordering::SeqCst) + 1
}

#[derive(Clone)]
pub struct TransferOptions {
    pub per_device_concurrency: usize,
    pub stall_timeout: Duration,
}

impl Default for TransferOptions {
    fn default() -> Self {
        Self {
            per_device_concurrency: DEFAULT_PER_DEVICE_CONCURRENCY,
            stall_timeout: DEFAULT_STALL_TIMEOUT,
        }
    }
}

struct Scheduler {
    executor: AdbExecutor,
    options: TransferOptions,
    tasks: Vec<TransferTask>,
    cancels: HashMap<TransferId, CancelToken>,
    snapshot: Arc<Mutex<Vec<TransferTask>>>,
}

impl Scheduler {
    fn run(mut self, cmd_rx: Receiver<ManagerCommand>) {
        loop {
            // Block only when nothing is running; otherwise poll.
            let idle = !self.tasks.iter().any(|task| task.state.is_active());
            let command = if idle {
                cmd_rx.recv().ok()
            } else {
                cmd_rx.try_recv().ok()
            };
            // Worker state must be merged BEFORE commands are applied:
            // a ClearFinished/Cancel arriving in the same tick as a worker's
            // terminal write must observe the fresh state, not a stale copy.
            self.harvest_finished();
            self.merge_worker_state();
            let exit = match command {
                Some(command) => self.apply(command),
                None if idle => true, // manager dropped
                None => false,
            };
            if exit {
                break;
            }

            self.start_eligible();
            self.publish();
            if !idle {
                thread::sleep(SCHEDULER_TICK);
            }
        }
        self.cancel_all_active();
        self.publish();
    }

    /// Workers mutate the shared snapshot for tasks they run; merge their
    /// view back so scheduler-side state (and `publish`) never stomps a
    /// Running/terminal state back to the stale Preparing record.
    fn merge_worker_state(&mut self) {
        let snapshot: Vec<TransferTask> = {
            let tasks = self
                .snapshot
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            tasks.clone()
        };
        for task in &mut self.tasks {
            if matches!(
                task.state,
                TransferState::Preparing | TransferState::Running
            ) && let Some(record) = snapshot.iter().find(|candidate| candidate.id == task.id)
                && record.state != TransferState::Queued
            {
                *task = record.clone();
            }
        }
    }

    /// Returns true when the scheduler should exit.
    fn apply(&mut self, command: ManagerCommand) -> bool {
        match command {
            ManagerCommand::Shutdown => return true,
            ManagerCommand::Enqueue {
                id,
                device,
                operation,
            } => {
                let task = TransferTask {
                    id,
                    device,
                    operation,
                    state: TransferState::Queued,
                    bytes_transferred: 0,
                    bytes_total: None,
                    speed_bps: None,
                    error: None,
                };
                log::info!(
                    "transfer #{} queued on {}: {}",
                    task.id,
                    task.device,
                    task.operation.describe()
                );
                self.tasks.push(task);
            }
            ManagerCommand::Cancel(id) => self.cancel_task(id),
            ManagerCommand::CancelAll => {
                let ids: Vec<_> = self
                    .tasks
                    .iter()
                    .filter(|task| task.state.is_active())
                    .map(|task| task.id)
                    .collect();
                for id in ids {
                    self.cancel_task(id);
                }
            }
            ManagerCommand::Retry(id) => {
                if let Some(task) = self.tasks.iter_mut().find(|task| task.id == id)
                    && task.state == TransferState::Failed
                {
                    Self::reset_for_retry(task);
                }
            }
            ManagerCommand::RetryFailed => {
                for task in &mut self.tasks {
                    if task.state == TransferState::Failed {
                        Self::reset_for_retry(task);
                    }
                }
            }
            ManagerCommand::ClearFinished => {
                self.tasks.retain(|task| !task.state.is_terminal());
                self.cancels.retain(|_, _| true);
            }
        }
        false
    }

    fn reset_for_retry(task: &mut TransferTask) {
        task.state = TransferState::Queued;
        task.error = None;
        task.bytes_transferred = 0;
        task.speed_bps = None;
    }

    fn cancel_task(&mut self, id: TransferId) {
        let Some(task) = self.tasks.iter_mut().find(|task| task.id == id) else {
            return;
        };
        if !task.state.is_active() {
            return;
        }
        if task.state == TransferState::Queued {
            task.state = TransferState::Cancelled;
        } else if let Some(token) = self.cancels.get(&id) {
            token.cancel();
        }
    }

    fn cancel_all_active(&mut self) {
        for token in self.cancels.values() {
            token.cancel();
        }
        for task in &mut self.tasks {
            if task.state == TransferState::Queued {
                task.state = TransferState::Cancelled;
            }
        }
    }

    fn harvest_finished(&mut self) {
        let terminal: Vec<_> = self
            .tasks
            .iter()
            .filter(|task| task.state.is_terminal())
            .map(|task| task.id)
            .collect();
        for id in terminal {
            self.cancels.remove(&id);
        }
    }

    fn start_eligible(&mut self) {
        let mut running_per_device: HashMap<String, usize> = HashMap::new();
        for task in &self.tasks {
            if matches!(
                task.state,
                TransferState::Preparing | TransferState::Running
            ) {
                *running_per_device.entry(task.device.clone()).or_default() += 1;
            }
        }

        let mut to_start = Vec::new();
        for task in &self.tasks {
            if task.state != TransferState::Queued {
                continue;
            }
            let running = running_per_device.entry(task.device.clone()).or_default();
            if *running < self.options.per_device_concurrency.max(1) {
                *running += 1;
                to_start.push((task.id, task.device.clone(), task.operation.clone()));
            }
        }

        for (id, device, operation) in to_start {
            if let Some(task) = self.tasks.iter_mut().find(|task| task.id == id) {
                task.state = TransferState::Preparing;
            }
            self.publish_task(id);

            let cancel = CancelToken::new();
            self.cancels.insert(id, cancel.clone());
            let executor = self.executor.clone();
            let snapshot = self.snapshot.clone();
            let stall_timeout = self.options.stall_timeout;
            thread::Builder::new()
                .name(format!("transfer-{id}"))
                .spawn(move || {
                    run_transfer(
                        id,
                        &device,
                        &operation,
                        executor,
                        cancel,
                        snapshot,
                        stall_timeout,
                    )
                })
                .expect("spawn transfer worker");
        }
    }

    /// Copy one task record into the shared snapshot (keeps queue order).
    fn publish_task(&self, id: TransferId) {
        let record = self
            .tasks
            .iter()
            .find(|task| task.id == id)
            .cloned()
            .expect("task present in scheduler copy");
        let mut tasks = self
            .snapshot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match tasks.iter_mut().find(|task| task.id == id) {
            Some(existing) => *existing = record,
            None => tasks.push(record),
        }
    }

    fn publish(&self) {
        let mut tasks = self
            .snapshot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *tasks = self.tasks.clone();
    }
}

struct TransferWorker<'a> {
    id: TransferId,
    device: &'a str,
    operation: &'a TransferOperation,
    snapshot: Arc<Mutex<Vec<TransferTask>>>,
}

impl TransferWorker<'_> {
    fn update(&self, mutate: impl FnOnce(&mut TransferTask)) {
        let mut tasks = self
            .snapshot
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(task) = tasks.iter_mut().find(|task| task.id == self.id) {
            mutate(task);
        }
    }

    fn finish(&self, state: TransferState, error: Option<String>) {
        log::info!(
            "transfer #{} {} on {} ended: {:?} {}",
            self.id,
            self.operation.describe(),
            self.device,
            state,
            error.as_deref().unwrap_or("")
        );
        self.update(|task| {
            task.state = state;
            task.error = error;
        });
    }
}

fn run_transfer(
    id: TransferId,
    device: &str,
    operation: &TransferOperation,
    executor: AdbExecutor,
    cancel: CancelToken,
    snapshot: Arc<Mutex<Vec<TransferTask>>>,
    stall_timeout: Duration,
) {
    let worker = TransferWorker {
        id,
        device,
        operation,
        snapshot,
    };
    log::info!(
        "transfer #{id} {} on {device} starting",
        operation.describe()
    );

    if cancel.cancelled() {
        worker.finish(TransferState::Cancelled, None);
        return;
    }

    // Push knows its total up front from the local file (PRD §12).
    if let TransferOperation::Push { source, .. } = operation
        && let Ok(meta) = std::fs::metadata(source)
    {
        worker.update(|task| task.bytes_total = Some(meta.len()));
    }

    let mut command = executor.command();
    command.arg("-s").arg(device);
    match operation {
        TransferOperation::Push {
            source,
            destination,
        } => {
            command.arg("push").arg(source).arg(destination);
        }
        TransferOperation::Pull {
            source,
            destination,
        } => {
            command.arg("pull").arg(source).arg(destination);
        }
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            worker.finish(
                TransferState::Failed,
                Some(TransferError::Spawn(err.to_string()).to_string()),
            );
            return;
        }
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    // Kill-on-drop job object: even a panicked worker cannot orphan adb.
    let mut child = ManagedChild::new(child);

    let tail = Arc::new(Mutex::new(String::new()));
    let written = Arc::new(AtomicUsize::new(0));
    spawn_pipe_reader(stdout, tail.clone(), written.clone());
    spawn_pipe_reader(stderr, tail.clone(), written.clone());

    worker.update(|task| task.state = TransferState::Running);
    let mut last_activity = Instant::now();
    let mut last_bytes = 0u64;
    let mut last_speed_sample = Instant::now();

    loop {
        if cancel.cancelled() {
            kill_and_reap(&mut child);
            worker.finish(TransferState::Cancelled, None);
            return;
        }

        let written_now = written.load(Ordering::SeqCst);
        let (changed, progress) = if written_now > 0 {
            let tail_text = tail
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone();
            (true, parse_progress(&tail_text))
        } else {
            (false, None)
        };
        if changed {
            last_activity = Instant::now();
        }
        if let Some(progress) = progress {
            let now = Instant::now();
            let elapsed = now
                .duration_since(last_speed_sample)
                .as_secs_f64()
                .max(0.001);
            let fallback_speed =
                (progress.bytes.saturating_sub(last_bytes) as f64 / elapsed) as u64;
            worker.update(|task| {
                task.state = TransferState::Running;
                // Percentage-only markers carry no byte counts; keep a total
                // we already know (push pre-computes it from the local file).
                if progress.total > 0 {
                    task.bytes_transferred = progress.bytes;
                    task.bytes_total = Some(progress.total);
                }
                task.speed_bps = Some(progress.speed_bps.unwrap_or(fallback_speed));
            });
            last_bytes = progress.bytes;
            last_speed_sample = now;
        }

        match child.try_wait() {
            Ok(Some(status)) => {
                // Give the pipe readers a beat to flush trailing output.
                thread::sleep(Duration::from_millis(50));
                let tail_text = tail
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone();
                if status.success() {
                    worker.finish(TransferState::Completed, None);
                } else {
                    let detail = tail_text.trim();
                    worker.finish(
                        TransferState::Failed,
                        Some(
                            TransferError::AdbExit(if detail.is_empty() {
                                format!("exit status {status}")
                            } else {
                                detail.to_owned()
                            })
                            .to_string(),
                        ),
                    );
                }
                return;
            }
            Ok(None) => {}
            Err(err) => {
                kill_and_reap(&mut child);
                worker.finish(
                    TransferState::Failed,
                    Some(TransferError::AdbExit(err.to_string()).to_string()),
                );
                return;
            }
        }

        if last_activity.elapsed() > stall_timeout {
            kill_and_reap(&mut child);
            worker.finish(
                TransferState::Failed,
                Some(TransferError::Stalled.to_string()),
            );
            return;
        }

        thread::sleep(WORKER_TICK);
    }
}

fn kill_and_reap(child: &mut ManagedChild) {
    let _ = child.kill();
    let _ = child.wait();
}

fn spawn_pipe_reader(
    stream: Option<impl Read + Send + 'static>,
    tail: Arc<Mutex<String>>,
    written: Arc<AtomicUsize>,
) {
    let Some(mut stream) = stream else {
        return;
    };
    thread::spawn(move || {
        let mut buffer = [0u8; 4096];
        loop {
            match stream.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    written.fetch_add(n, Ordering::SeqCst);
                    let mut tail = tail.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                    tail.push_str(&String::from_utf8_lossy(&buffer[..n]));
                    if tail.len() > TAIL_LIMIT {
                        let cut = tail.len() - TAIL_LIMIT;
                        let boundary = tail
                            .char_indices()
                            .map(|(i, _)| i)
                            .find(|i| *i >= cut)
                            .unwrap_or(tail.len());
                        let _ = tail.drain(..boundary);
                    }
                }
            }
        }
    });
}

#[derive(Debug, PartialEq)]
pub(crate) struct ProgressSample {
    pub bytes: u64,
    pub total: u64,
    pub speed_bps: Option<u64>,
}

/// Parse the latest adb transfer progress marker from a stderr tail.
///
/// Recognized shapes (from real adb output):
/// - `[ 42%] 12.34 MB/s (12345678/98765432)` — modern, `\r`-separated
/// - `[ 42%] /remote/path` — legacy percentage-only (no byte counts)
pub(crate) fn parse_progress(tail: &str) -> Option<ProgressSample> {
    let percent = last_percent_marker(tail)?;
    if let Some((bytes, total)) = last_bytes_marker(tail) {
        let marker_pos = tail.rfind('(').unwrap_or(0);
        let speed_bps = parse_speed_bps_before(tail, marker_pos);
        Some(ProgressSample {
            bytes,
            total,
            speed_bps,
        })
    } else {
        // Percentage-only markers carry no byte counts; report zeros so the
        // UI can show a percentage-of-unknown instead of a fake total.
        let _ = percent;
        Some(ProgressSample {
            bytes: 0,
            total: 0,
            speed_bps: None,
        })
    }
}

fn last_bytes_marker(tail: &str) -> Option<(u64, u64)> {
    let close = tail.rfind(')')?;
    let open = tail[..close].rfind('(')?;
    let inner = &tail[open + 1..close];
    let (a, b) = inner.split_once('/')?;
    let bytes: u64 = a.trim().parse().ok()?;
    let total: u64 = b.trim().parse().ok()?;
    Some((bytes, total))
}

fn last_percent_marker(tail: &str) -> Option<u8> {
    let close = tail.rfind(']')?;
    let open = tail[..close].rfind('[')?;
    let inner = tail[open + 1..close].trim();
    let percent = inner.strip_suffix('%')?;
    percent.trim().parse().ok()
}

fn parse_speed_bps_before(tail: &str, marker: usize) -> Option<u64> {
    let before = &tail[..marker];
    let idx = before.trim_end().rfind('B')?;
    // Unit token ends at the last alphabetic run before "/s" (e.g. "12.34 MB").
    let head = &before[..idx + 1];
    let unit: String = head
        .chars()
        .rev()
        .take_while(|ch| ch.is_alphabetic())
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    let number_part = &head[..head.len() - unit.len()];
    let number: f64 = number_part
        .trim()
        .rsplit(|ch: char| !(ch.is_ascii_digit() || ch == '.'))
        .next()?
        .trim()
        .parse()
        .ok()?;
    let multiplier = match unit.as_str() {
        "B" => 1.0,
        "KB" => 1_000.0,
        "MB" => 1_000_000.0,
        "GB" => 1_000_000_000.0,
        _ => return None,
    };
    Some((number * multiplier) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_progress_reads_modern_adb_markers() {
        let sample = "[ 25%] 12.34 MB/s (1638400/6553600)\r[ 50%] 24.68 MB/s (3276800/6553600)\r";
        let progress = parse_progress(sample).expect("progress parsed");
        assert_eq!(progress.bytes, 3276800);
        assert_eq!(progress.total, 6553600);
        // The latest marker wins consistently: its bytes/total and speed.
        assert_eq!(progress.speed_bps, Some(24_680_000));
    }

    #[test]
    fn parse_progress_reads_percentage_only_markers() {
        let sample = "[ 75%] /sdcard/Download/big.zip";
        let progress = parse_progress(sample).expect("progress parsed");
        assert_eq!(progress.bytes, 0);
        assert_eq!(progress.total, 0);
        assert_eq!(progress.speed_bps, None);
    }

    #[test]
    fn parse_progress_reads_kb_and_gb_speeds() {
        let kb = parse_progress("[  1%] 512 KB/s (1024/1048576)").unwrap();
        assert_eq!(kb.speed_bps, Some(512_000));
        let gb = parse_progress("[ 90%] 1.5 GB/s (9/10)").unwrap();
        assert_eq!(gb.speed_bps, Some(1_500_000_000));
    }

    #[test]
    fn parse_progress_ignores_non_progress_text() {
        assert!(parse_progress("adb: error: device offline").is_none());
        assert!(parse_progress("1 file pushed").is_none());
    }

    #[test]
    fn progress_fraction_handles_unknown_total() {
        let mut task = TransferTask {
            id: 1,
            device: "d".into(),
            operation: TransferOperation::Push {
                source: PathBuf::from("a"),
                destination: "b".into(),
            },
            state: TransferState::Running,
            bytes_transferred: 10,
            bytes_total: None,
            speed_bps: None,
            error: None,
        };
        assert_eq!(task.progress_fraction(), None);
        task.bytes_total = Some(0);
        assert_eq!(task.progress_fraction(), None);
        task.bytes_total = Some(20);
        assert_eq!(task.progress_fraction(), Some(0.5));
    }
}
