//! Remote filesystem operations over adb shell (PRD §7-§9).
//!
//! Safety model: paths are carried by the [`RemotePath`] type (validated,
//! normalized, no string surgery) and every operand handed to the device
//! shell is single-quote escaped through [`shell_quote`] — spaces, CJK,
//! quotes, `$`, `&`, `|`, `;`, parens and trailing spaces survive verbatim.
//!
//! Directory listings never parse human-oriented `ls -l` text (PRD §7).
//! The listing protocol is machine-oriented:
//!
//! ```sh
//! cd '<dir>' 2>/dev/null || exit 42
//! for entry in *; do
//!     printf '%s\n' "$entry"
//!     stat -c '%F\t%s\t%Y' "$entry" 2>/dev/null || printf 'ERR\n'
//! done
//! ```
//!
//! Each entry contributes a name line and a `kind\tsize\tmtime` line, so
//! names containing spaces, tabs-adjacent garbage or quotes never corrupt
//! the parse. run-as appends the same shell prefix for debuggable app data
//! (PRD §9), presented by the UI as "app data (run-as)", never as normal
//! filesystem access.

use std::fmt;

use crate::adb_executor::{AdbExecutor, ExecOptions};

/// Timeout for remote filesystem commands (they are short shell round-trips).
const REMOTE_FS_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

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

/// Escape for embedding into a double-quoted `stat -c` format string.
fn quote_stat_format(format: &str) -> String {
    format.replace('\\', "\\\\").replace('\'', "'\\''")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteEntryKind {
    Directory,
    File,
    Symlink,
    Other(String),
}

impl RemoteEntryKind {
    fn parse(stat_kind: &str) -> Self {
        let lowered = stat_kind.trim().to_ascii_lowercase();
        if lowered.contains("directory") {
            RemoteEntryKind::Directory
        } else if lowered.contains("symbolic link") {
            RemoteEntryKind::Symlink
        } else if lowered.contains("regular file") || lowered.contains("regular empty file") {
            RemoteEntryKind::File
        } else {
            RemoteEntryKind::Other(stat_kind.trim().to_owned())
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

/// Parse the two-line-per-entry listing protocol; also returns entries whose
/// stat failed (missing metadata) with zeroed size and `None` mtime so the
/// UI can still show the name (PRD §44 "missing metadata").
pub(crate) fn parse_listing(stdout: &str) -> Vec<RemoteEntry> {
    let mut entries = Vec::new();
    let mut lines = stdout.lines().peekable();
    // The shell prints nothing for an empty directory except a single
    // unmatched glob line ("*") followed by our ERR marker; drop that pair.
    let mut raw_pairs: Vec<(String, Option<String>)> = Vec::new();
    while let Some(name_line) = lines.next() {
        if name_line.is_empty() {
            continue;
        }
        let meta_line = lines.next();
        raw_pairs.push((name_line.to_owned(), meta_line.map(str::to_owned)));
    }
    if raw_pairs.len() == 1 {
        let (name, meta) = &raw_pairs[0];
        if name == "*" && meta.as_deref().is_some_and(|m| m.trim() == "ERR") {
            return Vec::new();
        }
    }
    for (name, meta) in raw_pairs {
        if name == "*" && meta.as_deref().is_some_and(|m| m.trim() == "ERR") {
            // A lone literal-star file would be indistinguishable from the
            // empty-glob marker; keep only when more entries follow.
            if entries.is_empty() {
                continue;
            }
        }
        let (kind, size, mtime) = match meta.as_deref() {
            Some(meta) if meta.trim() != "ERR" => match parse_stat_line(meta.trim()) {
                Some(parsed) => parsed,
                None => (RemoteEntryKind::Other(String::new()), 0, None),
            },
            _ => (RemoteEntryKind::Other(String::new()), 0, None),
        };
        entries.push(RemoteEntry {
            name,
            kind,
            size,
            modified_unix_secs: mtime,
        });
    }
    entries
}

fn parse_stat_line(line: &str) -> Option<(RemoteEntryKind, u64, Option<u64>)> {
    let mut parts = line.split('\t');
    let kind = parts.next()?.to_owned();
    let size: u64 = parts.next()?.trim().parse().ok()?;
    let mtime = parts
        .next()
        .and_then(|value| value.trim().parse::<u64>().ok());
    Some((RemoteEntryKind::parse(&kind), size, mtime))
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
        let mut command: Vec<String> = vec!["-s".into(), self.serial.clone(), "shell".into()];
        if let Some(package) = &self.run_as {
            command.push("run-as".into());
            command.push(package.clone());
        }
        command.push(script.to_owned());
        let args: Vec<&str> = command.iter().map(String::as_str).collect();
        self.executor
            .execute_with_options(
                &args,
                ExecOptions {
                    timeout: Some(REMOTE_FS_TIMEOUT),
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
            RemoteFsError::DeviceError(stderr.trim().to_owned())
        }
    }

    /// List one directory. Distinguishes not-found / no-permission / other
    /// failures instead of a blanket "operation failed" (PRD §8).
    pub fn list(&self, path: &RemotePath) -> Result<Vec<RemoteEntry>, RemoteFsError> {
        let script = format!(
            "cd {dir} 2>/dev/null || exit 42; for entry in *; do printf '%s\\n' \"$entry\"; \
             stat -c {fmt} \"$entry\" 2>/dev/null || printf 'ERR\\n'; done",
            dir = shell_quote(path.as_str()),
            fmt = shell_quote(&quote_stat_format("%F\\t%s\\t%Y")),
        );
        let output = self.shell(&script)?;
        if !output.success() {
            return Err(Self::classify_cd_failure(&output.stderr_lossy()));
        }
        let stdout = output.stdout_lossy();
        if stdout.trim().is_empty() {
            // Exit 0 with no output at all happens on toybox when cd
            // succeeded but the glob produced nothing printable.
            return Ok(Vec::new());
        }
        Ok(parse_listing(&stdout))
    }

    pub fn mkdir(&self, path: &RemotePath) -> Result<(), RemoteFsError> {
        let script = format!("mkdir -p {}", shell_quote(path.as_str()));
        let output = self.shell(&script)?;
        if output.success() {
            Ok(())
        } else {
            Err(Self::classify_cd_failure(&output.stderr_lossy()))
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
            Err(Self::classify_cd_failure(&output.stderr_lossy()))
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
            Err(Self::classify_cd_failure(&output.stderr_lossy()))
        }
    }

    /// Quick existence + kind probe.
    pub fn stat(&self, path: &RemotePath) -> Result<RemoteEntryKind, RemoteFsError> {
        let script = format!(
            "stat -c {fmt} {path} 2>/dev/null || exit 42",
            fmt = shell_quote(&quote_stat_format("%F")),
            path = shell_quote(path.as_str()),
        );
        let output = self.shell(&script)?;
        if !output.success() {
            return Err(Self::classify_cd_failure(&output.stderr_lossy()));
        }
        Ok(RemoteEntryKind::parse(&output.stdout_lossy()))
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
        let stdout = "Download\ndirectory\t0\t1726992000\n\
                      log.zip\nregular file\t1283457780\t1726999200\n\
                      link\nsymbolic link\t11\t1726999300\n";
        let entries = parse_listing(stdout);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].kind, RemoteEntryKind::Directory);
        assert_eq!(entries[1].kind, RemoteEntryKind::File);
        assert_eq!(entries[1].size, 1283457780);
        assert_eq!(entries[1].modified_unix_secs, Some(1726999200));
        assert_eq!(entries[2].kind, RemoteEntryKind::Symlink);
    }

    #[test]
    fn parse_listing_survives_names_with_spaces_and_specials() {
        let stdout = "hello world.txt\nregular file\t10\t1\n\
                      a'b$c.txt\nregular file\t20\t2\n\
                      \u{4e2d}\u{6587}\u{6587}\u{4ef6}.txt\nregular file\t30\t3\n";
        let entries = parse_listing(stdout);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].name, "hello world.txt");
        assert_eq!(entries[1].name, "a'b$c.txt");
        assert_eq!(entries[2].name, "中文文件.txt");
    }

    #[test]
    fn parse_listing_keeps_entries_with_missing_metadata() {
        // stat failed (permission) — the name still lists with zeroed data.
        let stdout = "locked.db\nERR\nvisible.txt\nregular file\t5\t9\n";
        let entries = parse_listing(stdout);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "locked.db");
        assert_eq!(entries[0].modified_unix_secs, None);
        assert_eq!(entries[0].size, 0);
        assert_eq!(entries[1].size, 5);
    }

    #[test]
    fn parse_listing_detects_empty_directory_glob_marker() {
        assert!(parse_listing("*\nERR\n").is_empty());
        // A literal-star file among others still lists.
        let entries = parse_listing("a.txt\nregular file\t1\t1\n*\nERR\n");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].name, "*");
    }

    #[test]
    fn parse_listing_handles_unknown_kinds() {
        let stdout = "fifo\nfifo file\t0\t0\nsocket\nsocket\t0\t0\n";
        let entries = parse_listing(stdout);
        assert!(matches!(entries[0].kind, RemoteEntryKind::Other(_)));
        assert!(matches!(entries[1].kind, RemoteEntryKind::Other(_)));
    }
}
