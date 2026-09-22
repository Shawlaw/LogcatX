//! Remote filesystem operations over adb shell (PRD §7-§9).
//!
//! Safety model: paths are carried by the [`RemotePath`] type (validated,
//! normalized, no string surgery) and every operand handed to the device
//! shell is single-quote escaped through [`shell_quote`] — spaces, CJK,
//! quotes, `$`, `&`, `|`, `;`, parens and trailing spaces survive verbatim.
//!
//! Directory listings never parse human-oriented `ls -l` text (PRD §7).
//! The listing protocol is machine-oriented and POSIX-portable:
//!
//! ```sh
//! cd '<dir>' 2>/dev/null || exit 42
//! for entry in *; do
//!     [ -e "$entry" ] || [ -L "$entry" ] || continue   # skip unexpanded glob
//!     if   [ -d "$entry" ]; then printf 'd\n'
//!     elif [ -L "$entry" ]; then printf 'l\n'
//!     elif [ -f "$entry" ]; then printf 'f\n'
//!     else                      printf 'o\n'
//!     fi
//!     printf '%s\n' "$entry"
//!     stat -c '%s %Y' "$entry" 2>/dev/null || printf -- '- -\n'
//! done
//! ```
//!
//! Each entry contributes three lines (kind marker, name, `size mtime`), so
//! names containing spaces, tabs-adjacent garbage or quotes never corrupt
//! the parse. An empty directory yields no output at all — the `[ -e ]`
//! guard skips the one unexpanded `*` glob iteration a shell performs (the
//! parser keeps a digit-free-stat fallback for outputs from the older
//! protocol). run-as appends the same shell prefix for debuggable app data
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteEntryKind {
    Directory,
    File,
    Symlink,
    Other(String),
}

impl RemoteEntryKind {
    /// Parse a protocol kind marker emitted by the listing script's
    /// `[ -d ]/[ -L ]/[ -f ]` tests: `d`, `l`, `f`, or anything else.
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

/// Parse the three-line-per-entry listing protocol: kind marker (`d`/`f`/
/// `l`/`o`), name, `size mtime` (`- -` when stat failed). Entries with
/// missing metadata still list with zeroed size and `None` mtime so the UI
/// can show the name (PRD §44 "missing metadata"). Tolerates the `\r\n`
/// line endings a real `adb shell` pty appends.
pub(crate) fn parse_listing(stdout: &str) -> Vec<RemoteEntry> {
    // A real adb shell emits \r\n; strip the carriage returns first so names
    // never carry a hidden trailing \r into display or path joins.
    let lines: Vec<&str> = stdout
        .lines()
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .filter(|line| !line.is_empty())
        .collect();

    // Collect complete (kind, name, meta) triplets; a truncated tail leaves
    // fewer than three lines and is ignored (protocol cut, not corruption).
    let mut triplets: Vec<(&str, &str, &str)> = Vec::new();
    let mut cursor = 0;
    while cursor + 2 < lines.len() {
        triplets.push((lines[cursor], lines[cursor + 1], lines[cursor + 2]));
        cursor += 3;
    }

    // The shell prints nothing for an empty directory except one unmatched
    // glob iteration for the literal "*". Its stat line never carries a
    // number (the entry does not exist), so treat any digit-free meta as the
    // glob leftover — some devices echo variants like "-- - -" for the
    // fallback printf. The listing script also skips non-existent entries
    // now, so this is defense for outputs from the older protocol.
    if triplets.len() == 1 {
        let (_, name, meta) = triplets[0];
        if name == "*" && !meta.chars().any(|c| c.is_ascii_digit()) {
            return Vec::new();
        }
    }

    triplets
        .into_iter()
        .map(|(kind, name, meta)| {
            let kind = RemoteEntryKind::parse_marker(kind);
            let (size, mtime) = parse_size_mtime(meta);
            RemoteEntry {
                name: name.to_owned(),
                kind,
                size,
                modified_unix_secs: mtime,
            }
        })
        .collect()
}

fn parse_size_mtime(meta: &str) -> (u64, Option<u64>) {
    let mut parts = meta.split_whitespace();
    let size = parts.next().and_then(|value| value.parse::<u64>().ok());
    let mtime = parts.next().and_then(|value| value.parse::<u64>().ok());
    (size.unwrap_or(0), mtime)
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

    /// List one directory. Distinguishes not-found / no-permission / other
    /// failures instead of a blanket "operation failed" (PRD §8).
    ///
    /// Protocol notes (learned from the 0.9.0 field trial): the device shell
    /// is POSIX, so directory detection uses `[ -d ]` tests instead of
    /// `stat -c '%F'` (whose output and escape handling vary across
    /// toybox/GNU), and size/mtime use plain space-separated `%s %Y` — no
    /// `\t` escapes that a device stat may print literally. Each entry is
    /// three lines: kind marker (`d`/`f`/`l`/`o`), name, `size mtime`
    /// (`- -` when stat fails). The parser tolerates the `\r\n` line endings
    /// a real `adb shell` pty produces.
    pub fn list(&self, path: &RemotePath) -> Result<Vec<RemoteEntry>, RemoteFsError> {
        let script = format!(
            "cd {dir} 2>/dev/null || exit 42; for entry in *; do \
             [ -e \"$entry\" ] || [ -L \"$entry\" ] || continue; \
             if [ -d \"$entry\" ]; then printf 'd\\n'; \
             elif [ -L \"$entry\" ]; then printf 'l\\n'; \
             elif [ -f \"$entry\" ]; then printf 'f\\n'; \
             else printf 'o\\n'; fi; \
             printf '%s\\n' \"$entry\"; \
             stat -c '%s %Y' \"$entry\" 2>/dev/null || printf -- '- -\\n'; done",
            dir = shell_quote(path.as_str()),
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

    /// Quick existence + kind probe using POSIX file tests (same reasoning
    /// as `list`: no dependency on stat format/escape support).
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
            return Err(Self::classify_cd_failure(&output.stderr_lossy()));
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
        let stdout = "d\nDownload\n0 1726992000\n\
                      f\nlog.zip\n1283457780 1726999200\n\
                      l\nlink\n11 1726999300\n";
        let entries = parse_listing(stdout);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].kind, RemoteEntryKind::Directory);
        assert_eq!(entries[0].name, "Download");
        assert_eq!(entries[1].kind, RemoteEntryKind::File);
        assert_eq!(entries[1].size, 1283457780);
        assert_eq!(entries[1].modified_unix_secs, Some(1726999200));
        assert_eq!(entries[2].kind, RemoteEntryKind::Symlink);
    }

    /// Real `adb shell` output carries \r\n from the device pty; names must
    /// come back clean (0.9.0 field trial carried hidden \r into names).
    #[test]
    fn parse_listing_strips_carriage_returns_from_crlf_output() {
        let stdout = "d\r\n\r\nDownload\r\n0 1726992000\r\nf\r\nhello world.txt\r\n10 1\r\n";
        let entries = parse_listing(stdout);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "Download");
        assert_eq!(entries[0].kind, RemoteEntryKind::Directory);
        assert_eq!(entries[1].name, "hello world.txt");
        assert_eq!(entries[1].size, 10);
        assert_eq!(entries[1].modified_unix_secs, Some(1));
    }

    #[test]
    fn parse_listing_survives_names_with_spaces_and_specials() {
        let stdout = "f\nhello world.txt\n10 1\n\
                      f\na'b$c.txt\n20 2\n\
                      f\n\u{4e2d}\u{6587}\u{6587}\u{4ef6}.txt\n30 3\n";
        let entries = parse_listing(stdout);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].name, "hello world.txt");
        assert_eq!(entries[1].name, "a'b$c.txt");
        assert_eq!(entries[2].name, "中文文件.txt");
    }

    #[test]
    fn parse_listing_keeps_entries_with_missing_metadata() {
        // stat failed (permission) — the name still lists with zeroed data.
        let stdout = "f\nlocked.db\n- -\nf\nvisible.txt\n5 9\n";
        let entries = parse_listing(stdout);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "locked.db");
        assert_eq!(entries[0].modified_unix_secs, None);
        assert_eq!(entries[0].size, 0);
        assert_eq!(entries[1].size, 5);
    }

    #[test]
    fn parse_listing_detects_empty_directory_glob_marker() {
        // Any digit-free stat line marks the unexpanded glob leftover; real
        // devices have been observed emitting kind "o" (the literal "*"
        // matches no -d/-L/-f test) and meta variants like "-- - -".
        assert!(parse_listing("f\n*\n- -\n").is_empty());
        assert!(parse_listing("o\n*\n- -\n").is_empty());
        assert!(parse_listing("o\n*\n-- - -\n").is_empty());
        // A literal-star file with real metadata still lists…
        let entries = parse_listing("f\n*\n7 9\n");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "*");
        assert_eq!(entries[0].size, 7);
        // …including among others.
        let entries = parse_listing("f\na.txt\n1 1\nf\n*\n2 2\n");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].name, "*");
        assert_eq!(entries[1].size, 2);
    }

    #[test]
    fn parse_listing_handles_unknown_kinds_and_truncated_tail() {
        let stdout = "o\nfifo\n0 0\no\nsocket\n0 0\n";
        let entries = parse_listing(stdout);
        assert!(matches!(entries[0].kind, RemoteEntryKind::Other(_)));
        assert!(matches!(entries[1].kind, RemoteEntryKind::Other(_)));

        // A truncated tail (fewer than three lines) is dropped, not garbled.
        let truncated = parse_listing("d\nDownload\n0 0\nf\norphan");
        assert_eq!(truncated.len(), 1);
        assert_eq!(truncated[0].name, "Download");
    }
}
