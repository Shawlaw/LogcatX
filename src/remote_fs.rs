//! Remote filesystem operations over adb shell (PRD §7-§9).
//!
//! Safety model: paths are carried by the [`RemotePath`] type (validated,
//! normalized, no string surgery) and every operand handed to the device
//! shell is single-quote escaped through [`shell_quote`] — spaces, CJK,
//! quotes, `$`, `&`, `|`, `;`, parens and trailing spaces survive verbatim.
//!
//! Directory listings never parse human-oriented `ls -l` text (PRD §7).
//! The listing protocol is machine-oriented, POSIX-portable, and loop-free:
//!
//! ```sh
//! cd '<dir>' || exit 42
//! printf '%s\n' * | tr '\n' '\000' | xargs -0 stat -c '%F|%s|%Y|%n' -- 2>/dev/null
//! printf '%s\n' 'logcatx-list-end'
//! exit 0
//! ```
//!
//! `printf '%s\n' *` expands the glob in one builtin call — the format is
//! reused per argument, so there is no per-entry shell loop. `tr` +
//! `xargs -0` feed the names to `stat` NUL-separated (spaces, quotes, `$`
//! and CJK survive verbatim; `--` guards `-`-prefixed names; `xargs`
//! re-invokes `stat` only at ARG_MAX boundaries), and one line per entry
//! comes back as `kind|size|mtime|name`. The name is everything after the
//! third `|`, so names containing pipes, spaces (leading/trailing
//! included) and quotes survive verbatim. The kind word is matched by
//! prefix: `directory`, `symbolic link`, and `regular` covering both
//! "regular file" and "regular empty file" — wording verified on Android 9
//! and 13 hardware, and prefix matching absorbs GNU's `symbolic link to
//! '<target>'` variant. `%F` is lstat-based: a symlink pointing at a
//! directory lists as a symlink (the 0.9.0 `[ -d ]`-first tests reported
//! it as a directory).
//!
//! Why no shell loops: on several OEM builds (measured: a ColorOS 13
//! phone and an Android 9 cloud phone) the adb shell's cgroup is
//! CPU-throttled to 5-15 ms per loop iteration regardless of body — 2k
//! entries meant 30-90 s of pure shell mechanics, dwarfing the per-entry
//! `stat` fork cost the 0.9.0 protocol paid. Loop-free, the same 2k-entry
//! directories measured 0.36-0.6 s end-to-end over wireless adb (the
//! batched stat itself: ~0.3 s for 2k files). `exit 0` keeps xargs's
//! aggregate failure code (123 when any stat operand failed) from masking
//! a good listing; only a failed `cd` exits non-zero. An entry whose
//! individual stat fails is simply absent from the output — acceptable
//! because the common failure modes (unreadable/unsearchable directory)
//! fail at `cd` and classify properly. The trailing
//! `logcatx-list-end` sentinel is what separates a genuinely empty
//! directory (sentinel present, no entries) from a broken pipeline: on a
//! device missing `tr`/`xargs` or whose `stat` lacks `%F` support the
//! whole pipeline dies silently (stderr is suppressed) and `exit 0`
//! would otherwise present that as a phantom empty listing — [`list`]
//! fails such output as [`RemoteFsError::Protocol`] instead. A stat line
//! always carries the format's three literal pipes, so the pipe-free
//! sentinel can never collide with an entry record. Names containing
//! newlines still cannot be represented (no line-based protocol can).
//! run-as appends the same shell prefix for debuggable app data
//! (PRD §9), presented by the UI as "app data (run-as)", never as normal
//! filesystem access.

use std::fmt;
use std::time::Duration;

use crate::adb_executor::{AdbExecutor, ExecOptions};

/// Timeout for remote filesystem commands (they are short shell round-trips).
const REMOTE_FS_TIMEOUT: Duration = Duration::from_secs(10);

/// Listings get their own budget: even on OEM builds whose adb shell is
/// cgroup-throttled, the loop-free protocol measured ~0.6 s for 2k entries
/// over wireless adb; 30 s is generous headroom for very large directories
/// and slow transports (the 0.9.0 loop-based script needed >10 s at ~2k
/// entries on the same hardware and timed out).
const LIST_TIMEOUT: Duration = Duration::from_secs(30);

/// Output capture ceiling for listings. One entry is one
/// `kind|size|mtime|name` line (~40-90 bytes), so 16 MiB admits on the
/// order of a few hundred thousand entries — beyond what the timeout
/// admits anyway. Crossing it fails with [`RemoteFsError::Truncated`]
/// rather than silently showing a partial listing (the capture layer
/// truncates at the limit and the parser cannot tell).
const LIST_STDOUT_LIMIT: usize = 16 * 1024 * 1024;

/// Bare line printed after the stat pipeline in the listing script. Its
/// presence proves the pipeline actually ran: an empty directory emits
/// just this sentinel, while a device missing `tr`/`xargs` or a `stat`
/// without `%F` support produces no output at all (stderr is suppressed
/// and `exit 0` masks the failure) — [`RemoteFs::list`] must report that
/// as [`RemoteFsError::Protocol`], not show a phantom empty directory.
/// Pipe-free, so it can never collide with a `kind|size|mtime|name`
/// record (the format emits three literal pipes per entry).
const LIST_END_SENTINEL: &str = "logcatx-list-end";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteFsError {
    /// The path cannot name a remote location at all (relative, empty, ..).
    InvalidPath(String),
    /// Directory does not exist on the device.
    NotFound,
    /// The adb user may not read/traverse the path.
    NoPermission,
    /// adb itself is missing or refuses to start.
    AdbUnavailable(String),
    /// The command exceeded its wall-clock budget.
    Timeout,
    /// The device rejected the command (offline, unauthorized, ...).
    DeviceError(String),
    /// Unexpected protocol output that cannot be attributed safely.
    Protocol(String),
    /// The listing output crossed the capture limit; the directory is too
    /// large to list in one round-trip.
    Truncated { limit_bytes: usize },
}

impl fmt::Display for RemoteFsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RemoteFsError::InvalidPath(detail) => write!(f, "invalid path: {detail}"),
            RemoteFsError::NotFound => write!(f, "not found on device"),
            RemoteFsError::NoPermission => write!(f, "permission denied"),
            RemoteFsError::AdbUnavailable(detail) => write!(f, "adb unavailable: {detail}"),
            RemoteFsError::Timeout => write!(f, "timed out"),
            RemoteFsError::DeviceError(detail) => write!(f, "device error: {detail}"),
            RemoteFsError::Protocol(detail) => write!(f, "unexpected output: {detail}"),
            RemoteFsError::Truncated { limit_bytes } => write!(
                f,
                "directory listing exceeded the {limit_bytes}-byte output limit"
            ),
        }
    }
}

/// An absolute, normalized remote path. Constructed only through validation;
/// no trailing slash (except the root `/`), no `.`/`..` components, no NUL.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RemotePath(String);

impl RemotePath {
    pub fn root() -> Self {
        Self("/".to_owned())
    }

    pub fn new(raw: &str) -> Result<Self, RemoteFsError> {
        let invalid = |detail: &str| RemoteFsError::InvalidPath(detail.to_owned());
        if raw.is_empty() {
            return Err(invalid("path is empty"));
        }
        if raw.contains('\0') {
            return Err(invalid("path contains NUL"));
        }
        if !raw.starts_with('/') {
            return Err(invalid("path must be absolute"));
        }
        let mut components = Vec::new();
        for component in raw.split('/') {
            match component {
                "" | "." => continue,
                ".." => {
                    components
                        .pop()
                        .ok_or_else(|| invalid("escapes the root"))?;
                }
                name => components.push(name.to_owned()),
            }
        }
        Ok(Self(join_components(&components)))
    }

    /// Append a single path component; the component must not contain `/`.
    pub fn join(&self, component: &str) -> Result<Self, RemoteFsError> {
        let invalid = |detail: &str| RemoteFsError::InvalidPath(detail.to_owned());
        if component.is_empty() {
            return Err(invalid("component is empty"));
        }
        if component.contains('\0') {
            return Err(invalid("component contains NUL"));
        }
        if component.contains('/') {
            return Err(invalid("component contains '/'"));
        }
        if matches!(component, "." | "..") {
            return Err(invalid("'.' and '..' are not valid components"));
        }
        let mut components = self.components().to_vec();
        components.push(component.to_owned());
        Ok(Self(join_components(&components)))
    }

    pub fn parent(&self) -> Option<Self> {
        let mut components = self.components().to_vec();
        components.pop()?;
        Some(Self(join_components(&components)))
    }

    pub fn file_name(&self) -> Option<&str> {
        self.0.rsplit('/').next().filter(|name| !name.is_empty())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn components(&self) -> Vec<String> {
        if self.0 == "/" {
            Vec::new()
        } else {
            self.0[1..].split('/').map(str::to_owned).collect()
        }
    }
}

impl fmt::Display for RemotePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn join_components(components: &[String]) -> String {
    if components.is_empty() {
        "/".to_owned()
    } else {
        format!("/{}", components.join("/"))
    }
}

/// POSIX single-quote escaping for the device shell: every operand is
/// wrapped in single quotes with embedded quotes closed-escaped. Arbitrary
/// bytes between quotes have no shell meaning, so spaces, CJK, quotes, `$`,
/// `&`, `|`, `;`, parens and trailing spaces all pass through verbatim.
pub fn shell_quote(arg: &str) -> String {
    let mut quoted = String::with_capacity(arg.len() + 2);
    quoted.push('\'');
    for ch in arg.chars() {
        if ch == '\'' {
            quoted.push_str("'\\''");
        } else {
            quoted.push(ch);
        }
    }
    quoted.push('\'');
    quoted
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteEntryKind {
    Directory,
    File,
    Symlink,
    Other(String),
}

impl RemoteEntryKind {
    /// Map a `stat -c '%F'` kind word to a kind. Prefix matching absorbs
    /// the wording spread across stat implementations: toybox prints
    /// `symbolic link` where GNU appends ` to '<target>'`, and both print
    /// `regular file` / `regular empty file` for non-empty / empty files.
    /// Wording verified on Android 9 and 13 hardware.
    fn parse_file_type(word: &str) -> Self {
        if word.starts_with("directory") {
            RemoteEntryKind::Directory
        } else if word.starts_with("symbolic link") {
            RemoteEntryKind::Symlink
        } else if word.starts_with("regular") {
            RemoteEntryKind::File
        } else {
            RemoteEntryKind::Other(word.to_owned())
        }
    }

    /// Parse a protocol kind marker emitted by the single-path `stat`
    /// probe's `[ -d ]/[ -L ]/[ -f ]` tests: `d`, `l`, `f`, or anything
    /// else.
    fn parse_marker(marker: &str) -> Self {
        match marker.trim() {
            "d" => RemoteEntryKind::Directory,
            "l" => RemoteEntryKind::Symlink,
            "f" => RemoteEntryKind::File,
            other => RemoteEntryKind::Other(other.to_owned()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteEntry {
    pub name: String,
    pub kind: RemoteEntryKind,
    pub size: u64,
    pub modified_unix_secs: Option<u64>,
}

/// Parse the single-section listing protocol: one `kind|size|mtime|name`
/// line per entry (kind is the `stat %F` word). The name is everything
/// after the third `|`, so names containing pipes, spaces (leading and
/// trailing included), quotes, `$` and CJK survive verbatim. Lines that do
/// not carry a kind word, two numeric fields and a non-empty name (stat
/// error text that escaped suppression, protocol noise, a
/// transport-truncated tail) are skipped rather than garbled into
/// entries. Tolerates the `\r\n` line endings a real `adb shell` pty
/// appends.
pub(crate) fn parse_listing(stdout: &str) -> Vec<RemoteEntry> {
    stdout
        .lines()
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .filter_map(parse_entry_line)
        .collect()
}

/// Parse one `stat -c '%F|%s|%Y|%n'` line into an entry. Returns `None`
/// for lines that are not entry records (noise, truncated tails, empty
/// lines). An unparseable mtime still lists the entry with `None` mtime.
fn parse_entry_line(line: &str) -> Option<RemoteEntry> {
    let mut fields = line.splitn(4, '|');
    let kind = RemoteEntryKind::parse_file_type(fields.next()?);
    let size = fields.next()?.parse::<u64>().ok()?;
    let mtime = fields.next().and_then(|value| value.parse::<u64>().ok());
    let name = fields.next()?;
    (!name.is_empty()).then(|| RemoteEntry {
        name: name.to_owned(),
        kind,
        size,
        modified_unix_secs: mtime,
    })
}

/// Remote filesystem handle bound to one device (optionally through run-as).
pub struct RemoteFs {
    executor: AdbExecutor,
    serial: String,
    /// When set, every shell command runs inside `run-as <package>`.
    run_as: Option<String>,
}

impl RemoteFs {
    pub fn new(adb_path: &str, serial: &str) -> Self {
        Self {
            executor: AdbExecutor::new(adb_path),
            serial: serial.to_owned(),
            run_as: None,
        }
    }

    /// Browse a debuggable app's private data through run-as (PRD §9).
    pub fn new_run_as(adb_path: &str, serial: &str, package: &str) -> Self {
        Self {
            executor: AdbExecutor::new(adb_path),
            serial: serial.to_owned(),
            run_as: Some(package.to_owned()),
        }
    }

    pub fn uses_run_as(&self) -> bool {
        self.run_as.is_some()
    }

    fn shell(&self, script: &str) -> Result<crate::adb_executor::AdbOutput, RemoteFsError> {
        self.shell_with(script, REMOTE_FS_TIMEOUT, None)
    }

    /// `shell` with per-call budget overrides — listings need a larger
    /// wall-clock budget and output ceiling than the short round-trips.
    fn shell_with(
        &self,
        script: &str,
        timeout: Duration,
        stdout_limit: Option<usize>,
    ) -> Result<crate::adb_executor::AdbOutput, RemoteFsError> {
        let mut command: Vec<String> = vec!["-s".into(), self.serial.clone(), "shell".into()];
        if let Some(package) = &self.run_as {
            // `run-as <pkg> <script>` fails on real devices: adb joins its
            // arguments with plain spaces, so the device shell parses the
            // script's first word as the binary run-as must exec (observed:
            // "run-as: exec failed for cd: Permission denied" on an Android
            // 16 device). Route through an explicit `sh -c` and single-quote
            // the script so it survives the join as one argument.
            command.push("run-as".into());
            command.push(package.clone());
            command.push("sh".into());
            command.push("-c".into());
            command.push(shell_quote(script));
        } else {
            command.push(script.to_owned());
        }
        let args: Vec<&str> = command.iter().map(String::as_str).collect();
        self.executor
            .execute_with_options(
                &args,
                ExecOptions {
                    timeout: Some(timeout),
                    stdout_limit,
                    ..Default::default()
                },
            )
            .map_err(|err| match err {
                crate::adb_executor::AdbError::Timeout { .. } => RemoteFsError::Timeout,
                crate::adb_executor::AdbError::Spawn { source, .. } => {
                    RemoteFsError::AdbUnavailable(source.to_string())
                }
                other => RemoteFsError::DeviceError(other.to_string()),
            })
    }

    fn classify_cd_failure(stderr: &str) -> RemoteFsError {
        let lowered = stderr.to_ascii_lowercase();
        if lowered.contains("permission denied") {
            RemoteFsError::NoPermission
        } else if lowered.contains("no such file") || lowered.contains("not found") {
            RemoteFsError::NotFound
        } else {
            let detail = stderr.trim();
            // Devices can fail with a bare non-zero exit and no stderr; an
            // empty error message would render as a blank banner.
            if detail.is_empty() {
                RemoteFsError::DeviceError("device command failed without output".to_owned())
            } else {
                RemoteFsError::DeviceError(detail.to_owned())
            }
        }
    }

    /// Classify a failed device command from whichever stream carried the
    /// diagnostic. Modern shell-v2 adb separates stderr, but pty-style
    /// transports merge it into stdout — and a device-side `2>/dev/null`
    /// suppresses it entirely, which would collapse every failure into a
    /// generic DeviceError on real hardware. Scanning stdout when stderr is
    /// empty keeps the NotFound / NoPermission distinction alive on both
    /// transports (PRD §8).
    fn classify_failure(output: &crate::adb_executor::AdbOutput) -> RemoteFsError {
        let stderr = output.stderr_lossy();
        if stderr.trim().is_empty() {
            Self::classify_cd_failure(&output.stdout_lossy())
        } else {
            Self::classify_cd_failure(&stderr)
        }
    }

    /// List one directory. Distinguishes not-found / no-permission / other
    /// failures instead of a blanket "operation failed" (PRD §8).
    ///
    /// Protocol notes: one loop-free round-trip. `printf '%s\n' *` emits
    /// every name in a single builtin call (the format is reused per
    /// argument — no per-entry shell iteration, which OEM cgroup throttling
    /// makes cost 5-15 ms each), then `tr | xargs -0 stat -c
    /// '%F|%s|%Y|%n'` batches all metadata in ~ARG_MAX-sized stat
    /// invocations. Kind words are prefix-matched (see the module docs).
    /// Listings run with a 30 s budget and a 16 MiB capture ceiling;
    /// crossing the ceiling reports [`RemoteFsError::Truncated`] instead
    /// of a silent partial listing. The output must end with the
    /// `logcatx-list-end` sentinel — its absence means the stat pipeline
    /// never ran (see the module docs) and yields a Protocol error.
    pub fn list(&self, path: &RemotePath) -> Result<Vec<RemoteEntry>, RemoteFsError> {
        // No `2>/dev/null` on the cd: the diagnostic line is what
        // `classify_failure` reads to tell NotFound from NoPermission, and
        // suppressing it device-side erased that distinction on real
        // hardware. A failed cd produces no listing output, so the error
        // text can never pollute the parse of a successful listing.
        //
        // `exit 0` at the end keeps xargs's aggregate failure code (123 when
        // any stat invocation failed) from masking a good listing — after a
        // successful cd every later step is best-effort; a per-entry stat
        // failure just omits that entry.
        let script = format!(
            "cd {dir} || exit 42; \
             printf '%s\\n' * | tr '\\n' '\\000' \
             | xargs -0 stat -c '%F|%s|%Y|%n' -- 2>/dev/null; \
             printf '%s\\n' '{LIST_END_SENTINEL}'; \
             exit 0",
            dir = shell_quote(path.as_str()),
        );
        let output = self.shell_with(&script, LIST_TIMEOUT, Some(LIST_STDOUT_LIMIT))?;
        if !output.success() {
            return Err(Self::classify_failure(&output));
        }
        if output.stdout_truncated {
            return Err(RemoteFsError::Truncated {
                limit_bytes: LIST_STDOUT_LIMIT,
            });
        }
        let stdout = output.stdout_lossy();
        // The sentinel separates a genuinely empty directory (sentinel, no
        // entries) from a pipeline that never ran — a device without
        // tr/xargs, or a stat that rejects the %F format, prints nothing at
        // all and exit 0 masks it. That must be an error, not a phantom
        // empty listing.
        let completed = stdout
            .lines()
            .map(|line| line.strip_suffix('\r').unwrap_or(line))
            .any(|line| line == LIST_END_SENTINEL);
        if !completed {
            return Err(RemoteFsError::Protocol(format!(
                "listing of {} ended without the {LIST_END_SENTINEL} marker \
                 (tr/xargs/stat unavailable or %F unsupported?)",
                path.as_str()
            )));
        }
        Ok(parse_listing(&stdout))
    }

    pub fn mkdir(&self, path: &RemotePath) -> Result<(), RemoteFsError> {
        let script = format!("mkdir -p {}", shell_quote(path.as_str()));
        let output = self.shell(&script)?;
        if output.success() {
            Ok(())
        } else {
            Err(Self::classify_failure(&output))
        }
    }

    /// Rename or move within the device filesystem (`mv`).
    pub fn rename(&self, from: &RemotePath, to: &RemotePath) -> Result<(), RemoteFsError> {
        let script = format!(
            "mv {} {}",
            shell_quote(from.as_str()),
            shell_quote(to.as_str())
        );
        let output = self.shell(&script)?;
        if output.success() {
            Ok(())
        } else {
            Err(Self::classify_failure(&output))
        }
    }

    /// Delete a file or directory tree. The caller must confirm destructive
    /// intent in the UI (PRD §6.6); this layer performs no prompting.
    pub fn delete(&self, path: &RemotePath) -> Result<(), RemoteFsError> {
        let script = format!("rm -rf {}", shell_quote(path.as_str()));
        let output = self.shell(&script)?;
        if output.success() {
            Ok(())
        } else {
            Err(Self::classify_failure(&output))
        }
    }

    /// Quick existence + kind probe using POSIX file tests (a single path
    /// has no loop to throttle, so the builtin tests stay cheapest here
    /// and keep the probe independent of stat format support).
    pub fn stat(&self, path: &RemotePath) -> Result<RemoteEntryKind, RemoteFsError> {
        let quoted = shell_quote(path.as_str());
        let script = format!(
            "if [ -d {path} ]; then printf 'd\\n'; \
             elif [ -L {path} ]; then printf 'l\\n'; \
             elif [ -f {path} ]; then printf 'f\\n'; \
             else printf 'o\\n'; fi",
            path = quoted,
        );
        let output = self.shell(&script)?;
        if !output.success() {
            return Err(Self::classify_failure(&output));
        }
        Ok(RemoteEntryKind::parse_marker(output.stdout_lossy().trim()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_path_accepts_unusual_but_legal_names() {
        for raw in [
            "/",
            "/sdcard",
            "/sdcard/Download/hello world.txt",
            "/sdcard/\u{4e2d}\u{6587}\u{6587}\u{4ef6}.txt",
            "/sdcard/a'b.txt",
            "/sdcard/a$b.txt",
            "/sdcard/a\"b.txt",
            "/sdcard/a&b|c;d(e).txt",
            "/sdcard/trailing space ",
        ] {
            assert_eq!(RemotePath::new(raw).unwrap().as_str(), raw, "raw: {raw}");
        }
    }

    #[test]
    fn remote_path_normalizes_dot_segments_and_slashes() {
        assert_eq!(
            RemotePath::new("/sdcard/./Download//x/").unwrap().as_str(),
            "/sdcard/Download/x"
        );
        assert_eq!(RemotePath::new("//").unwrap().as_str(), "/");
    }

    #[test]
    fn remote_path_rejects_traversal_and_relative() {
        for raw in ["", "sdcard/x", "/../etc", "/sdcard/../..", "/a/\0/b"] {
            assert!(RemotePath::new(raw).is_err(), "accepted {raw:?}");
        }
        assert!(RemotePath::root().join("..").is_err());
        assert!(RemotePath::root().join("a/b").is_err());
    }

    #[test]
    fn remote_path_parent_join_file_name() {
        let path = RemotePath::new("/sdcard/DCIM/Camera").unwrap();
        assert_eq!(path.parent().unwrap().as_str(), "/sdcard/DCIM");
        assert_eq!(path.parent().unwrap().parent().unwrap().as_str(), "/sdcard");
        assert_eq!(
            RemotePath::new("/sdcard")
                .unwrap()
                .parent()
                .unwrap()
                .as_str(),
            "/"
        );
        assert!(RemotePath::root().parent().is_none());
        assert_eq!(path.file_name(), Some("Camera"));
        assert_eq!(RemotePath::root().file_name(), None);
        assert_eq!(
            path.join("photo (1).jpg").unwrap().as_str(),
            "/sdcard/DCIM/Camera/photo (1).jpg"
        );
    }

    #[test]
    fn shell_quote_neutralizes_metacharacters() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("hello world.txt"), "'hello world.txt'");
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
        assert_eq!(shell_quote("a$b; rm -rf /"), "'a$b; rm -rf /'");
        assert_eq!(shell_quote("\u{4e2d}\u{6587}.txt"), "'中文.txt'");
        assert_eq!(shell_quote("trailing "), "'trailing '");
    }

    #[test]
    fn parse_listing_reads_files_directories_and_symlinks() {
        let stdout = "directory|0|1726992000|Download\n\
                      regular file|1283457780|1726999200|log.zip\n\
                      symbolic link|11|1726999300|link\n";
        let entries = parse_listing(stdout);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].kind, RemoteEntryKind::Directory);
        assert_eq!(entries[0].name, "Download");
        assert_eq!(entries[1].kind, RemoteEntryKind::File);
        assert_eq!(entries[1].size, 1283457780);
        assert_eq!(entries[1].modified_unix_secs, Some(1726999200));
        assert_eq!(entries[2].kind, RemoteEntryKind::Symlink);
        assert_eq!(entries[2].name, "link");
    }

    /// Both stat wording variants must map to File: "regular file" and
    /// GNU's "regular empty file" for zero-size files (verified on
    /// Android 9/13 hardware).
    #[test]
    fn parse_listing_maps_regular_wording_variants_to_file() {
        let stdout = "regular file|10|1|full.bin\n\
                      regular empty file|0|2|empty.bin\n";
        let entries = parse_listing(stdout);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].kind, RemoteEntryKind::File);
        assert_eq!(entries[0].size, 10);
        assert_eq!(entries[1].kind, RemoteEntryKind::File);
        assert_eq!(entries[1].size, 0);
    }

    /// GNU stat appends the target to symlink kinds; toybox does not —
    /// prefix matching must accept both.
    #[test]
    fn parse_listing_maps_symbolic_link_wording_variants() {
        let stdout = "symbolic link|6|1|toybox-link\n\
                      symbolic link to 'subdir'|6|2|gnu-link\n";
        let entries = parse_listing(stdout);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].kind, RemoteEntryKind::Symlink);
        assert_eq!(entries[1].kind, RemoteEntryKind::Symlink);
    }

    /// Real `adb shell` output carries \r\n from the device pty; names must
    /// come back clean (0.9.0 field trial carried hidden \r into names).
    #[test]
    fn parse_listing_strips_carriage_returns_from_crlf_output() {
        let stdout = "directory|0|1726992000|Download\r\n\r\nregular file|5|1|x.txt\r\n";
        let entries = parse_listing(stdout);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "Download");
        assert_eq!(entries[0].kind, RemoteEntryKind::Directory);
        assert_eq!(entries[1].name, "x.txt");
        assert_eq!(entries[1].size, 5);
    }

    #[test]
    fn parse_listing_survives_names_with_spaces_and_specials() {
        let stdout = "regular file|10|1|hello world.txt\n\
                      regular file|20|2|a'b$c.txt\n\
                      regular file|30|3|中文文件.txt\n";
        let entries = parse_listing(stdout);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].name, "hello world.txt");
        assert_eq!(entries[0].size, 10);
        assert_eq!(entries[1].name, "a'b$c.txt");
        assert_eq!(entries[1].size, 20);
        assert_eq!(entries[2].name, "中文文件.txt");
        assert_eq!(entries[2].size, 30);
    }

    /// The name is everything after the third pipe, so names containing
    /// pipes themselves round-trip verbatim.
    #[test]
    fn parse_listing_survives_names_containing_pipes() {
        let stdout = "regular file|5|9|weird|name.txt\n";
        let entries = parse_listing(stdout);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "weird|name.txt");
        assert_eq!(entries[0].size, 5);
    }

    #[test]
    fn parse_listing_preserves_leading_and_trailing_spaces_in_names() {
        let stdout = "regular empty file|5|9| pad\nregular empty file|6|10|trail \n";
        let entries = parse_listing(stdout);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, " pad");
        assert_eq!(entries[0].size, 5);
        assert_eq!(entries[1].name, "trail ");
        assert_eq!(entries[1].size, 6);
    }

    /// A literal file named `*` lists like any other entry; in an empty
    /// directory the unexpanded glob dies inside stat (suppressed), so no
    /// output at all means empty.
    #[test]
    fn parse_listing_lists_literal_star_file() {
        let entries = parse_listing("regular empty file|7|9|*\ndirectory|0|0|Download\n");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "*");
        assert_eq!(entries[0].size, 7);
        assert_eq!(entries[1].name, "Download");
    }

    #[test]
    fn parse_listing_reports_empty_directory() {
        assert!(parse_listing("").is_empty());
        assert!(parse_listing("\r\n\r\n").is_empty());
    }

    /// The protocol's end sentinel is pipe-free, so it never parses as an
    /// entry record and simply drops out of the listing — including the
    /// sentinel-only output of a genuinely empty directory.
    #[test]
    fn parse_listing_ignores_the_end_sentinel() {
        let entries =
            parse_listing(&format!("regular empty file|7|9|a.txt\n{LIST_END_SENTINEL}\n"));
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "a.txt");
        assert!(parse_listing(&format!("{LIST_END_SENTINEL}\r\n")).is_empty());
    }

    /// Lines without the four-field shape (stat error text that escaped
    /// device-side suppression, protocol noise, truncated tails) are
    /// skipped rather than garbled into phantom entries. An unparseable
    /// mtime still lists the entry with `None` mtime.
    #[test]
    fn parse_listing_skips_unparseable_lines() {
        let stdout = "regular file|1|1|a.txt\n\
                      stat: cannot read 'x': Permission denied\n\
                      truncated tail without fields\n\
                      regular file|2|notanumber|b.txt\n\
                      regular file|3|3|c.txt\n";
        let entries = parse_listing(stdout);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].name, "a.txt");
        assert_eq!(entries[0].size, 1);
        assert_eq!(entries[1].name, "b.txt");
        assert_eq!(entries[1].size, 2);
        assert_eq!(entries[1].modified_unix_secs, None);
        assert_eq!(entries[2].name, "c.txt");
        assert_eq!(entries[2].size, 3);
    }

    #[test]
    fn parse_listing_handles_unknown_kinds() {
        let stdout = "fifo|0|0|pipe0\nlocal socket|0|0|sock1\n";
        let entries = parse_listing(stdout);
        assert!(matches!(entries[0].kind, RemoteEntryKind::Other(_)));
        assert!(matches!(entries[1].kind, RemoteEntryKind::Other(_)));
        assert_eq!(entries[0].name, "pipe0");
    }
}
