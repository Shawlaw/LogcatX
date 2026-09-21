//! Unified owner of every adb child process (PRD §14-§16, §21).
//!
//! All short adb commands go through [`AdbExecutor`], which enforces:
//!
//! - a timeout on every execution (no adb call may hang forever),
//! - cooperative cancellation via [`CancelToken`],
//! - kill + reap of the child on timeout/cancel (no zombie processes),
//! - bounded stdout/stderr capture (a misbehaving device cannot grow memory
//!   without limit; streaming commands are exempt by design).
//!
//! Output capture uses anonymous temp files instead of pipes so a flooding
//! child can never block on a full pipe and no reader threads need to be
//! reaped after a deadline kill. This generalizes the pattern previously
//! maintained inside the wireless module.

use std::{
    fmt,
    fs::File,
    io::{self, Read, Seek, SeekFrom, Write},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use crate::managed_child::ManagedChild;

/// Default wall-clock budget for short adb commands (PRD §15).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);
/// Default stdout capture cap; callers doing `dumpsys` raise this.
pub const DEFAULT_STDOUT_LIMIT: usize = 1024 * 1024;
/// Default stderr capture cap.
pub const DEFAULT_STDERR_LIMIT: usize = 256 * 1024;

/// Cooperative cancellation shared between UI and a running execution.
#[derive(Clone, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

impl fmt::Debug for CancelToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CancelToken")
            .field("cancelled", &self.cancelled())
            .finish()
    }
}

/// Errors distinguished for UI mapping (PRD §39 taxonomy; grows over 0.9.0).
#[derive(Debug)]
pub enum AdbError {
    Cancelled {
        command: String,
    },
    Timeout {
        command: String,
        timeout: Duration,
    },
    /// The adb binary could not be spawned (missing, not executable, ...).
    Spawn {
        command: String,
        source: io::Error,
    },
    /// Waiting on or reading from the child failed.
    Wait {
        command: String,
        source: io::Error,
    },
}

impl fmt::Display for AdbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AdbError::Cancelled { .. } => write!(f, "cancelled"),
            AdbError::Timeout { timeout, .. } => write!(f, "timed out after {timeout:?}"),
            AdbError::Spawn { source, .. } => write!(f, "failed to start adb: {source}"),
            AdbError::Wait { source, .. } => write!(f, "failed to wait for adb: {source}"),
        }
    }
}

/// Result of a completed (non-streaming) adb command.
#[derive(Debug)]
pub struct AdbOutput {
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// More than the capture limit was produced; bytes are truncated.
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub duration: Duration,
}

impl AdbOutput {
    pub fn success(&self) -> bool {
        self.exit_code == Some(0)
    }

    pub fn stdout_lossy(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    pub fn stderr_lossy(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
}

/// Per-call overrides for [`AdbExecutor::execute_with_options`].
#[derive(Default)]
pub struct ExecOptions {
    pub timeout: Option<Duration>,
    pub cancel: Option<CancelToken>,
    pub stdout_limit: Option<usize>,
    pub stderr_limit: Option<usize>,
    /// Line written to the child's stdin (used by `adb pair` for the code).
    pub stdin_line: Option<String>,
    /// Extra environment variables for the child (used by the test harness).
    pub env: Vec<(String, String)>,
}

#[derive(Clone)]
pub struct AdbExecutor {
    adb_path: String,
}

impl AdbExecutor {
    pub fn new(adb_path: impl Into<String>) -> Self {
        Self {
            adb_path: adb_path.into(),
        }
    }

    pub fn adb_path(&self) -> &str {
        &self.adb_path
    }

    /// Base `adb` command with platform window-hiding applied.
    pub fn command(&self) -> Command {
        let mut command = Command::new(&self.adb_path);
        hide_window(&mut command);
        command
    }

    pub fn execute(&self, args: &[&str]) -> Result<AdbOutput, AdbError> {
        self.execute_with_options(args, ExecOptions::default())
    }

    pub fn execute_with_timeout(
        &self,
        args: &[&str],
        timeout: Duration,
    ) -> Result<AdbOutput, AdbError> {
        self.execute_with_options(
            args,
            ExecOptions {
                timeout: Some(timeout),
                ..Default::default()
            },
        )
    }

    pub fn execute_with_options(
        &self,
        args: &[&str],
        options: ExecOptions,
    ) -> Result<AdbOutput, AdbError> {
        let timeout = options.timeout.unwrap_or(DEFAULT_TIMEOUT);
        let stdout_limit = options.stdout_limit.unwrap_or(DEFAULT_STDOUT_LIMIT);
        let stderr_limit = options.stderr_limit.unwrap_or(DEFAULT_STDERR_LIMIT);
        if options.cancel.as_ref().is_some_and(CancelToken::cancelled) {
            return Err(AdbError::Cancelled {
                command: self.describe(args),
            });
        }

        let command_desc = self.describe(args);

        // Temp-file backed capture: a flooding child writes to a file and can
        // never block, so the deadline loop below stays in control.
        let mut stdout_file = tempfile_file(&command_desc)?;
        let mut stderr_file = tempfile_file(&command_desc)?;
        let mut command = self.command();
        command
            .args(args)
            .stdout(Stdio::from(stdout_file.try_clone().map_err(|source| {
                AdbError::Spawn {
                    command: command_desc.clone(),
                    source,
                }
            })?))
            .stderr(Stdio::from(stderr_file.try_clone().map_err(|source| {
                AdbError::Spawn {
                    command: command_desc.clone(),
                    source,
                }
            })?))
            .stdin(if options.stdin_line.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            });

        for (key, value) in &options.env {
            command.env(key, value);
        }

        let started = Instant::now();
        let mut child = command.spawn().map_err(|source| AdbError::Spawn {
            command: command_desc.clone(),
            source,
        })?;

        // Feed optional stdin (pairing code) before wrapping in the job
        // object; a failed write aborts the child immediately.
        if let Some(input) = options.stdin_line
            && let Some(mut stdin) = child.stdin.take()
            && let Err(source) = stdin.write_all(format!("{input}\n").as_bytes())
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err(AdbError::Wait {
                command: self.describe(args),
                source,
            });
        }

        // ManagedChild attaches a kill-on-drop Windows job object so an
        // abandoned child cannot outlive a panicked executor thread.
        let mut child = ManagedChild::new(child);

        let status = loop {
            if options.cancel.as_ref().is_some_and(CancelToken::cancelled)
                || started.elapsed() >= timeout
            {
                let _ = child.kill();
                let _ = child.wait();
                return Err(
                    if options.cancel.as_ref().is_some_and(CancelToken::cancelled) {
                        AdbError::Cancelled {
                            command: self.describe(args),
                        }
                    } else {
                        AdbError::Timeout {
                            command: self.describe(args),
                            timeout,
                        }
                    },
                );
            }
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => thread::sleep(POLL_INTERVAL),
                Err(source) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(AdbError::Wait {
                        command: self.describe(args),
                        source,
                    });
                }
            }
        };

        let stdout = capture(&mut stdout_file, stdout_limit, &command_desc)?;
        let stderr = capture(&mut stderr_file, stderr_limit, &command_desc)?;
        Ok(AdbOutput {
            exit_code: status.code(),
            stdout: stdout.bytes,
            stderr: stderr.bytes,
            stdout_truncated: stdout.truncated,
            stderr_truncated: stderr.truncated,
            duration: started.elapsed(),
        })
    }

    /// Spawn a long-running streaming command (logcat). No timeout applies —
    /// streaming sessions are supervised by their owner (cancel/kill on stop).
    /// No capture limit either (PRD §16 exempts streaming commands).
    pub fn spawn_streaming(
        &self,
        args: &[&str],
        stdout: Stdio,
        stderr: Stdio,
    ) -> Result<ManagedChild, AdbError> {
        let command_desc = self.describe(args);
        let mut command = self.command();
        command
            .args(args)
            .stdout(stdout)
            .stderr(stderr)
            .stdin(Stdio::null());
        let child = command.spawn().map_err(|source| AdbError::Spawn {
            command: command_desc,
            source,
        })?;
        Ok(ManagedChild::new(child))
    }

    fn describe(&self, args: &[&str]) -> String {
        format!("{} {}", self.adb_path, args.join(" "))
    }
}

const POLL_INTERVAL: Duration = Duration::from_millis(25);

fn tempfile_file(command: &str) -> Result<File, AdbError> {
    tempfile::tempfile().map_err(|source| AdbError::Wait {
        command: command.to_owned(),
        source,
    })
}

struct Capture {
    bytes: Vec<u8>,
    truncated: bool,
}

fn capture(file: &mut File, limit: usize, command: &str) -> Result<Capture, AdbError> {
    let io_error = |source| AdbError::Wait {
        command: command.to_owned(),
        source,
    };
    let total = file
        .metadata()
        .map(|meta| meta.len() as usize)
        .map_err(io_error)?;
    file.seek(SeekFrom::Start(0)).map_err(io_error)?;
    let mut bytes = Vec::new();
    file.take(limit as u64)
        .read_to_end(&mut bytes)
        .map_err(io_error)?;
    Ok(Capture {
        bytes,
        truncated: total > limit,
    })
}

#[cfg(target_os = "windows")]
fn hide_window(command: &mut Command) {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(target_os = "windows"))]
fn hide_window(_command: &mut Command) {}
