//! Remote filesystem operations over adb shell (PRD §7-§9).
//!
//! Safety model: paths are carried by the [`RemotePath`] type (validated,
//! normalized, no string surgery) and every operand handed to the device
//! shell is single-quote escaped through [`shell_quote`] — spaces, CJK,
//! quotes, `$`, `&`, `|`, `;`, parens and trailing spaces survive verbatim.
//!
//! Directory listings never parse human-oriented `ls -l` text (PRD §7).
//! The listing protocol is machine-oriented, POSIX-portable, and fork-light.
//! It prints two sections separated by a control-byte marker line:
//!
//! ```sh
//! cd '<dir>' || exit 42
//! for entry in *; do                                  # kind+name, builtins only
//!     [ -e "$entry" ] || [ -L "$entry" ] || continue  # skip unexpanded glob
//!     if   [ -d "$entry" ]; then printf 'd\n'
//!     elif [ -L "$entry" ]; then printf 'l\n'
//!     elif [ -f "$entry" ]; then printf 'f\n'
//!     else                      printf 'o\n'
//!     fi
//!     printf '%s\n' "$entry"
//! done
//! printf '%s\n' '<TAB>--META--<TAB>'                   # section separator
//! for entry in *; do                                  # names only
//!     [ -e "$entry" ] || [ -L "$entry" ] || continue
//!     printf '%s\n' "$entry"
//! done | tr '\n' '\000' | xargs -0 stat -c '%s %Y %n' -- 2>/dev/null
//! exit 0
//! ```
//!
//! The first section is pure shell builtins (zero process spawns) and pairs
//! a kind marker with each name. The second section batches every entry
//! through one `tr | xargs -0 stat` pipeline — `xargs` re-invokes `stat`
//! only when it would exceed ARG_MAX, so a directory costs one fork per
//! few hundred entries instead of the per-entry `stat` fork the 0.9.0
//! script paid (that fork cost ~3-10 ms per entry on-device and blew the
//! 10 s budget around ~2k entries). Metadata lines are merged by name in
//! Rust; an entry without a stat line (unreadable, or `tr`/`xargs` absent
//! on an exotic shell) degrades to zero size / unknown mtime while keeping
//! its name and kind. The trailing `exit 0` keeps xargs's per-file failure
//! code (123) from masking an otherwise successful listing; only a failed
//! `cd` exits non-zero. The separator rides as a `printf '%s\n'` argument,
//! not as escape syntax in the format string — `\ddd` octal support varies
//! across device printfs, and a literally-printed `\001` would break the
//! parse on every directory. A name consisting of the literal separator
//! bytes (tab-flanked `--META--`), or one containing a newline, would
//! containing a newline, would corrupt the parse — neither is producible
//! through the app's path joining and neither parsed under the 0.9.0
//! three-line protocol either. run-as appends the same shell prefix for
//! debuggable app data (PRD §9), presented by the UI as "app data
//! (run-as)", never as normal filesystem access.

use std::collections::HashMap;
use std::fmt;
use std::time::Duration;

use crate::adb_executor::{AdbExecutor, ExecOptions};

/// Timeout for remote filesystem commands (they are short shell round-trips).
const REMOTE_FS_TIMEOUT: Duration = Duration::from_secs(10);

/// Listings legitimately take seconds on large directories (the 0.9.0
/// per-entry fork cost alone exceeded 10 s around ~2k entries), so they get
/// their own budget instead of the short-round-trip default.
const LIST_TIMEOUT: Duration = Duration::from_secs(30);

/// Output capture ceiling for listings. One entry costs two kind/name lines
/// plus one `size mtime name` line (~tens of bytes), so 16 MiB admits on
/// the order of a few hundred thousand entries — beyond what the timeout
/// admits anyway. Crossing it fails with [`RemoteFsError::Truncated`]
/// rather than silently showing a partial listing (the capture layer
/// truncates at the limit and the parser cannot tell).
const LIST_STDOUT_LIMIT: usize = 16 * 1024 * 1024;

/// Line separating the kind/name section from the stat metadata section.
/// Tab-flanked so it cannot be a `stat` output line (those start with a
/// digit) and could only collide with a file literally named
/// `\t--META--\t` — not producible through the app's path joining. Emitted
/// as a `printf '%s\n'` argument (raw bytes), never as printf escape
/// syntax, because `\ddd` octal handling varies across device printfs.
const META_SEPARATOR: &str = "\t--META--\t";

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

/// Parse the two-section listing protocol: kind/name pairs, the separator
/// line, then one `size mtime name` stat line per entry. The name is
/// everything after the second space of a stat line, so names containing
/// spaces (including leading/trailing ones) survive verbatim. Entries
/// without a stat line still list with zeroed size and `None` mtime so the
/// UI can show the name (PRD §44 "missing metadata"); stat lines naming
/// entries absent from the first section (created between the two passes)
/// are ignored. A truncated pair tail is dropped, output without the
/// separator yields no entries, and the `\r\n` line endings a real
/// `adb shell` pty appends are tolerated.
pub(crate) fn parse_listing(stdout: &str) -> Vec<RemoteEntry> {
    // A real adb shell emits \r\n; strip the carriage returns first so names
    // never carry a hidden trailing \r into display or path joins.
    let lines: Vec<&str> = stdout
        .lines()
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .filter(|line| !line.is_empty())
        .collect();

    let Some(separator) = lines.iter().position(|line| *line == META_SEPARATOR) else {
        return Vec::new();
    };

    let mut metadata: HashMap<&str, (u64, Option<u64>)> = HashMap::new();
    for line in &lines[separator + 1..] {
        if let Some((size, mtime, name)) = parse_stat_line(line) {
            metadata.insert(name, (size, mtime));
        }
    }

    lines[..separator]
        .chunks_exact(2)
        .map(|pair| {
            let (size, mtime) = metadata.get(pair[1]).copied().unwrap_or((0, None));
            RemoteEntry {
                name: pair[1].to_owned(),
                kind: RemoteEntryKind::parse_marker(pair[0]),
                size,
                modified_unix_secs: mtime,
            }
        })
        .collect()
}

/// Split one `stat -c '%s %Y %n'` line into size, mtime and name. Lines
/// that do not start with two parseable numeric fields (stat error text
/// that escaped suppression, protocol noise) are rejected.
fn parse_stat_line(line: &str) -> Option<(u64, Option<u64>, &str)> {
    let mut fields = line.splitn(3, ' ');
    let size = fields.next()?.parse::<u64>().ok()?;
    let mtime = fields.next().and_then(|value| value.parse::<u64>().ok());
    let name = fields.next()?;
    (!name.is_empty()).then_some((size, mtime, name))
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
    /// Protocol notes: two sections in one round-trip. Kind/name pairs come
    /// from builtin POSIX file tests (not `stat -c '%F'`, whose output and
    /// escape handling vary across toybox/GNU); size/mtime come from one
    /// batched `xargs -0 stat -c '%s %Y %n'` pass merged by name. The batch
    /// replaces the 0.9.0 per-entry `stat` fork, which cost ~3-10 ms per
    /// entry on-device and timed out on large directories (field report:
    /// ~2k entries exceeded 10 s). Listings run with a 30 s budget and a
    /// 16 MiB capture ceiling; crossing the ceiling reports
    /// [`RemoteFsError::Truncated`] instead of a silent partial listing.
    pub fn list(&self, path: &RemotePath) -> Result<Vec<RemoteEntry>, RemoteFsError> {
        // No `2>/dev/null` on the cd: the diagnostic line is what
        // `classify_failure` reads to tell NotFound from NoPermission, and
        // suppressing it device-side erased that distinction on real
        // hardware. A failed cd produces no listing output, so the error
        // text can never pollute the parse of a successful listing.
        //
        // `exit 0` at the end keeps xargs's aggregate failure code (123 when
        // any stat invocation failed) from masking a good listing — after a
        // successful cd every later step is best-effort and degrades to
        // missing metadata, never to a failed command.
        let script = format!(
            "cd {dir} || exit 42; \
             for entry in *; do \
             [ -e \"$entry\" ] || [ -L \"$entry\" ] || continue; \
             if [ -d \"$entry\" ]; then printf 'd\\n'; \
             elif [ -L \"$entry\" ]; then printf 'l\\n'; \
             elif [ -f \"$entry\" ]; then printf 'f\\n'; \
             else printf 'o\\n'; fi; \
             printf '%s\\n' \"$entry\"; done; \
             printf '%s\\n' '\t--META--\t'; \
             for entry in *; do \
             [ -e \"$entry\" ] || [ -L \"$entry\" ] || continue; \
             printf '%s\\n' \"$entry\"; done \
             | tr '\\n' '\\000' | xargs -0 stat -c '%s %Y %n' -- 2>/dev/null; \
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
        if stdout.trim().is_empty() {
            // Defensive: a successful run always prints at least the
            // separator line, so empty output means transport weirdness —
            // report an empty directory rather than a protocol panic.
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
            return Err(Self::classify_failure(&output));
        }
        Ok(RemoteEntryKind::parse_marker(output.stdout_lossy().trim()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The separator as it appears on the wire (after \r stripping).
    const SEP: &str = "\t--META--\t";

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
        let stdout = format!(
            "d\nDownload\nf\nlog.zip\nl\nlink\n{SEP}\n\
             0 1726992000 Download\n1283457780 1726999200 log.zip\n11 1726999300 link\n"
        );
        let entries = parse_listing(&stdout);
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
        let stdout = format!("d\r\n\r\nDownload\r\n{SEP}\r\n0 1726992000 Download\r\n");
        let entries = parse_listing(&stdout);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "Download");
        assert_eq!(entries[0].kind, RemoteEntryKind::Directory);
        assert_eq!(entries[0].size, 0);
        assert_eq!(entries[0].modified_unix_secs, Some(1726992000));
    }

    #[test]
    fn parse_listing_survives_names_with_spaces_and_specials() {
        let stdout = format!(
            "f\nhello world.txt\nf\na'b$c.txt\nf\n\u{4e2d}\u{6587}\u{6587}\u{4ef6}.txt\n{SEP}\n\
             10 1 hello world.txt\n20 2 a'b$c.txt\n30 3 中文文件.txt\n"
        );
        let entries = parse_listing(&stdout);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].name, "hello world.txt");
        assert_eq!(entries[0].size, 10);
        assert_eq!(entries[1].name, "a'b$c.txt");
        assert_eq!(entries[1].size, 20);
        assert_eq!(entries[2].name, "中文文件.txt");
        assert_eq!(entries[2].size, 30);
    }

    #[test]
    fn parse_listing_keeps_entries_with_missing_metadata() {
        // stat failed (permission) — the name still lists with zeroed data.
        let stdout = format!("f\nlocked.db\nf\nvisible.txt\n{SEP}\n5 9 visible.txt\n");
        let entries = parse_listing(&stdout);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "locked.db");
        assert_eq!(entries[0].modified_unix_secs, None);
        assert_eq!(entries[0].size, 0);
        assert_eq!(entries[1].size, 5);
    }

    /// A stat section with no lines at all is the degraded output of a
    /// device where `tr`/`xargs`/`stat` is unavailable or every stat call
    /// failed: names and kinds must still list.
    #[test]
    fn parse_listing_tolerates_an_empty_stat_section() {
        let stdout = format!("f\na.txt\nd\nsub\n{SEP}\n");
        let entries = parse_listing(&stdout);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "a.txt");
        assert_eq!(entries[0].size, 0);
        assert_eq!(entries[1].kind, RemoteEntryKind::Directory);
    }

    #[test]
    fn parse_listing_reports_empty_directory() {
        // Empty directory: the separator is the only output (the [ -e ]
        // guard skips the unexpanded glob iteration).
        assert!(parse_listing(&format!("{SEP}\n")).is_empty());
        // Whitespace-only output around it changes nothing.
        assert!(parse_listing(&format!("\r\n{SEP}\r\n\r\n")).is_empty());
    }

    /// A literal file named `*` lists like any other entry; the 0.9.0
    /// glob-leftover heuristics are unnecessary because the [ -e ] guard
    /// already skips the unexpanded glob.
    #[test]
    fn parse_listing_lists_literal_star_file() {
        let stdout = format!("f\n*\nd\nDownload\n{SEP}\n7 9 *\n0 0 Download\n");
        let entries = parse_listing(&stdout);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, "*");
        assert_eq!(entries[0].size, 7);
        assert_eq!(entries[1].name, "Download");
    }

    #[test]
    fn parse_listing_preserves_leading_and_trailing_spaces_in_names() {
        let stdout = format!("f\n pad\nf\ntrail \n{SEP}\n5 9  pad\n6 10 trail \n");
        let entries = parse_listing(&stdout);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, " pad");
        assert_eq!(entries[0].size, 5);
        assert_eq!(entries[1].name, "trail ");
        assert_eq!(entries[1].size, 6);
    }

    #[test]
    fn parse_listing_ignores_unknown_stat_names_and_drops_truncated_tail() {
        // ghost.txt appeared between the two passes (stat section only) —
        // ignored. The orphan kind marker after the pair is a truncated
        // tail — dropped, not garbled into a phantom entry.
        let stdout = format!("f\na.txt\nf\n{SEP}\n1 1 a.txt\n2 2 ghost.txt\n");
        let entries = parse_listing(&stdout);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "a.txt");
        assert_eq!(entries[0].size, 1);
    }

    #[test]
    fn parse_listing_skips_unparseable_stat_lines() {
        // stat error text that escaped the device-side suppression, or a
        // missing mtime field, must not corrupt neighboring entries.
        let stdout = format!(
            "f\na.txt\nf\nb.txt\n{SEP}\n1 1 a.txt\nstat: cannot read 'x'\n2 2 b.txt\n"
        );
        let entries = parse_listing(&stdout);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].size, 1);
        assert_eq!(entries[1].size, 2);
    }

    #[test]
    fn parse_listing_handles_unknown_kinds() {
        let stdout = format!("o\nfifo\no\nsocket\n{SEP}\n0 0 fifo\n0 0 socket\n");
        let entries = parse_listing(&stdout);
        assert!(matches!(entries[0].kind, RemoteEntryKind::Other(_)));
        assert!(matches!(entries[1].kind, RemoteEntryKind::Other(_)));
    }

    /// Output from anything but this protocol (no separator line) yields no
    /// entries rather than garbage.
    #[test]
    fn parse_listing_without_separator_yields_nothing() {
        assert!(parse_listing("f\nx.txt\n10 1 x.txt\n").is_empty());
        assert!(parse_listing("").is_empty());
    }
}
