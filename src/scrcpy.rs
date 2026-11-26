use crate::managed_child::ManagedChild;
use serde::{Deserialize, Serialize};
use std::{
    cmp::Ordering,
    io::Read,
    path::Path,
    process::{Command, Output, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};
use tempfile::NamedTempFile;

pub const MAX_EXTRA_ARGS: usize = 64;
pub const MAX_EXTRA_ARGS_BYTES: usize = 16 * 1024;
pub const MAX_EXTRA_ARG_BYTES: usize = 4 * 1024;
pub const DEFAULT_NEW_DISPLAY_WIDTH: &str = "720";
pub const DEFAULT_NEW_DISPLAY_HEIGHT: &str = "1600";
pub const DEFAULT_NEW_DISPLAY_DPI: &str = "320";

/// Per-device options shared by Mirror and New Display. Each non-empty line in
/// `extra_args` is passed to scrcpy as one independent argv item.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScrcpyLaunchOptions {
    pub no_control: bool,
    pub no_audio: bool,
    pub extra_args: String,
}

impl ScrcpyLaunchOptions {
    pub fn is_empty(&self) -> bool {
        !self.no_control && !self.no_audio && self.extra_args.trim().is_empty()
    }
}

/// Persisted New Display draft. Custom dimensions remain present even when
/// `use_device_defaults` is true, so switching modes never erases them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NewDisplaySettings {
    pub use_device_defaults: bool,
    pub width: String,
    pub height: String,
    pub dpi: String,
    pub start_app: String,
}

impl Default for NewDisplaySettings {
    fn default() -> Self {
        Self {
            use_device_defaults: true,
            width: DEFAULT_NEW_DISPLAY_WIDTH.to_owned(),
            height: DEFAULT_NEW_DISPLAY_HEIGHT.to_owned(),
            dpi: DEFAULT_NEW_DISPLAY_DPI.to_owned(),
            start_app: String::new(),
        }
    }
}

impl NewDisplaySettings {
    pub fn normalized(mut self) -> Self {
        self.width = self.width.trim().to_owned();
        self.height = self.height.trim().to_owned();
        self.dpi = self.dpi.trim().to_owned();
        self.start_app = self.start_app.trim().to_owned();
        if self.width.is_empty() {
            self.width = DEFAULT_NEW_DISPLAY_WIDTH.to_owned();
        }
        if self.height.is_empty() {
            self.height = DEFAULT_NEW_DISPLAY_HEIGHT.to_owned();
        }
        self
    }

    pub fn is_default(&self) -> bool {
        self == &Self::default()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScrcpyArgsError {
    ContainsNul,
    InputTooLong { max_bytes: usize },
    TooManyArguments { max: usize },
    ArgumentTooLong { line: usize, max_bytes: usize },
    DeviceSelector { line: usize },
}

impl std::fmt::Display for ScrcpyArgsError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ContainsNul => {
                write!(formatter, "scrcpy arguments cannot contain NUL characters")
            }
            Self::InputTooLong { max_bytes } => write!(
                formatter,
                "scrcpy extra arguments exceed the {max_bytes}-byte limit"
            ),
            Self::TooManyArguments { max } => {
                write!(formatter, "scrcpy accepts at most {max} extra arguments")
            }
            Self::ArgumentTooLong { line, max_bytes } => write!(
                formatter,
                "scrcpy argument on line {line} exceeds the {max_bytes}-byte limit"
            ),
            Self::DeviceSelector { line } => write!(
                formatter,
                "scrcpy argument on line {line} cannot override the selected device (--serial/-s)"
            ),
        }
    }
}

impl std::error::Error for ScrcpyArgsError {}

/// Parse one argv item per line without shell tokenization or execution.
/// Blank lines are ignored, CRLF is accepted, and quotes/backslashes are kept
/// as literal argument content rather than interpreted by a shell.
pub fn parse_extra_args(input: &str) -> Result<Vec<String>, ScrcpyArgsError> {
    if input.contains('\0') {
        return Err(ScrcpyArgsError::ContainsNul);
    }
    if input.len() > MAX_EXTRA_ARGS_BYTES {
        return Err(ScrcpyArgsError::InputTooLong {
            max_bytes: MAX_EXTRA_ARGS_BYTES,
        });
    }

    let mut args = Vec::new();
    for (index, raw_line) in input.lines().enumerate() {
        let argument = raw_line.trim();
        if argument.is_empty() {
            continue;
        }
        let line = index + 1;
        if argument.len() > MAX_EXTRA_ARG_BYTES {
            return Err(ScrcpyArgsError::ArgumentTooLong {
                line,
                max_bytes: MAX_EXTRA_ARG_BYTES,
            });
        }
        if is_device_selector(argument) {
            return Err(ScrcpyArgsError::DeviceSelector { line });
        }
        if args.len() == MAX_EXTRA_ARGS {
            return Err(ScrcpyArgsError::TooManyArguments {
                max: MAX_EXTRA_ARGS,
            });
        }
        args.push(argument.to_owned());
    }
    Ok(args)
}

fn is_long_option_prefix(argument: &str, option: &str) -> bool {
    argument
        .strip_prefix("--")
        .map(|value| value.split_once('=').map_or(value, |(name, _)| name))
        .is_some_and(|name| !name.is_empty() && option.starts_with(name))
}

fn is_device_selector(argument: &str) -> bool {
    if argument.starts_with("--") {
        // getopt_long accepts unique long-option prefixes, so checking only
        // `--serial` misses forms such as `--seri=OTHER`. Reject every prefix
        // (including ambiguous short prefixes) to remain fail-closed.
        return is_long_option_prefix(argument, "serial");
    }

    let Some(short_options) = argument.strip_prefix('-') else {
        return false;
    };
    // getopt also accepts clustered short flags: `-nsOTHER` means `-n` plus
    // `-sOTHER`. Stop at the first other value-taking option because the rest
    // of that same argv is its value, not more options (scrcpy 3.3.4 cli.c).
    for option in short_options.chars() {
        if option == 's' {
            return true;
        }
        if matches!(option, 'b' | 'm' | 'p' | 'r' | 'V') {
            break;
        }
    }
    false
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScrcpyVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl ScrcpyVersion {
    pub fn supports_new_display(self) -> bool {
        self >= Self {
            major: 3,
            minor: 0,
            patch: 0,
        }
    }

    /// `--no-playback` and `--record` are available from scrcpy 2.x.
    pub fn supports_recording(self) -> bool {
        self >= Self {
            major: 2,
            minor: 0,
            patch: 0,
        }
    }
}

impl Ord for ScrcpyVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch).cmp(&(other.major, other.minor, other.patch))
    }
}

impl PartialOrd for ScrcpyVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl std::fmt::Display for ScrcpyVersion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NewDisplayOptions {
    width: Option<u32>,
    height: Option<u32>,
    dpi: Option<u32>,
    start_app: Option<String>,
}

impl NewDisplayOptions {
    pub fn custom(width: &str, height: &str, dpi: &str) -> Result<Self, String> {
        let width = parse_positive_dimension(width, "width")?;
        let height = parse_positive_dimension(height, "height")?;
        let dpi = parse_positive_dimension(dpi, "DPI")?;

        if width.is_none() || height.is_none() {
            return Err("Both virtual display width and height are required.".to_owned());
        }

        Ok(Self {
            width,
            height,
            dpi,
            start_app: None,
        })
    }

    pub fn with_start_app(mut self, start_app: &str) -> Result<Self, String> {
        let start_app = start_app.trim();
        if start_app.is_empty() {
            return Ok(self);
        }
        if start_app.chars().any(char::is_whitespace) {
            return Err("Start app must be a single Android package name.".to_owned());
        }
        self.start_app = Some(start_app.to_owned());
        Ok(self)
    }

    fn argument(&self) -> String {
        let Some(width) = self.width else {
            return "--new-display".to_owned();
        };
        let height = self
            .height
            .expect("new display width and height are paired");
        let mut argument = format!("--new-display={width}x{height}");
        if let Some(dpi) = self.dpi {
            argument.push('/');
            argument.push_str(&dpi.to_string());
        }
        argument
    }
}

pub enum LaunchMode {
    Mirror,
    NewDisplay(NewDisplayOptions),
}

pub fn validate_scrcpy_path(scrcpy_path: &str) -> Result<ScrcpyVersion, String> {
    let trimmed = scrcpy_path.trim();
    if trimmed.is_empty() {
        return Err("scrcpy executable path cannot be empty".to_owned());
    }

    let output = scrcpy_command(trimmed)
        .arg("--version")
        .output()
        .map_err(|err| format!("Failed to execute `{trimmed} --version`: {err}"))?;
    if !output.status.success() {
        return Err(format!(
            "scrcpy validation failed for `{trimmed}`: {}",
            combined_output(&output).if_empty("unknown error")
        ));
    }

    parse_scrcpy_version(&combined_output(&output)).ok_or_else(|| {
        format!(
            "Could not determine the scrcpy version from `{trimmed} --version`: {}",
            combined_output(&output).if_empty("no output")
        )
    })
}

pub fn launch(
    scrcpy_path: &str,
    adb_path: &str,
    serial: &str,
    mode: LaunchMode,
    options: &ScrcpyLaunchOptions,
) -> Result<(), String> {
    let mut command = build_command(scrcpy_path, adb_path, serial, mode, options)
        .map_err(|error| error.to_string())?;
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|err| format!("Failed to start scrcpy for {serial}: {err}"))?;

    // Drain both pipes for the whole child lifetime: a quiet early failure may
    // return its exact diagnostics, while a long-running mirror can never
    // block on a full pipe. Each reader retains only a bounded prefix.
    let (sender, receiver) = mpsc::channel();
    spawn_output_reader(
        child.stdout.take().expect("piped scrcpy stdout"),
        CaptureStream::Stdout,
        sender.clone(),
    );
    spawn_output_reader(
        child.stderr.take().expect("piped scrcpy stderr"),
        CaptureStream::Stderr,
        sender,
    );

    // Argument errors normally make scrcpy exit immediately. Reap such an
    // early exit instead of reporting a successful mirror; a still-running
    // child keeps the existing detached fire-and-forget behavior.
    let deadline = Instant::now() + Duration::from_millis(250);
    loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|err| format!("Failed to check scrcpy startup for {serial}: {err}"))?
        {
            let output = collect_captured_output(&receiver);
            let mut error = format!("scrcpy for {serial} exited during startup with {status}");
            if !output.is_empty() {
                error.push('\n');
                error.push_str(&output);
            }
            return Err(error);
        }
        if Instant::now() >= deadline {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

const MAX_CAPTURED_STREAM_BYTES: usize = 32 * 1024;

#[derive(Clone, Copy)]
enum CaptureStream {
    Stdout,
    Stderr,
}

fn spawn_output_reader<R>(
    mut reader: R,
    stream: CaptureStream,
    sender: mpsc::Sender<(CaptureStream, String)>,
) where
    R: Read + Send + 'static,
{
    std::thread::spawn(move || {
        let mut captured = Vec::new();
        let mut truncated = false;
        let mut buffer = [0_u8; 8192];
        while let Ok(count) = reader.read(&mut buffer) {
            if count == 0 {
                break;
            }
            let remaining = MAX_CAPTURED_STREAM_BYTES.saturating_sub(captured.len());
            let retained = remaining.min(count);
            captured.extend_from_slice(&buffer[..retained]);
            truncated |= retained < count;
        }
        let mut text = String::from_utf8_lossy(&captured).into_owned();
        if truncated {
            text.push_str("\n[output truncated]");
        }
        let _ = sender.send((stream, text));
    });
}

fn collect_captured_output(receiver: &mpsc::Receiver<(CaptureStream, String)>) -> String {
    let deadline = Instant::now() + Duration::from_millis(100);
    let mut stdout = String::new();
    let mut stderr = String::new();
    for _ in 0..2 {
        let Ok((stream, text)) =
            receiver.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        else {
            break;
        };
        match stream {
            CaptureStream::Stdout => stdout = text,
            CaptureStream::Stderr => stderr = text,
        }
    }
    combined_text_output(&stdout, &stderr)
}

/// A headless scrcpy process recording the device screen to a local MP4.
///
/// scrcpy still creates its SDL window in `--no-playback` mode; closing that
/// window is what finalizes the MP4 cleanly. The caller hides it right after
/// launch and posts `WM_CLOSE` on stop.
pub struct RecordingProcess {
    pub child: ManagedChild,
    pub pid: u32,
    /// Captured stdout/stderr; read after the child exits for diagnostics.
    pub log: NamedTempFile,
}

pub fn spawn_recording(
    scrcpy_path: &str,
    adb_path: &str,
    serial: &str,
    output_path: &Path,
    bitrate_mbps: u32,
) -> Result<RecordingProcess, String> {
    let log =
        NamedTempFile::new().map_err(|err| format!("Failed to create a scrcpy log file: {err}"))?;
    let stdout = log
        .as_file()
        .try_clone()
        .map_err(|err| format!("Failed to prepare the scrcpy log file: {err}"))?;
    let stderr = log
        .as_file()
        .try_clone()
        .map_err(|err| format!("Failed to prepare the scrcpy log file: {err}"))?;

    let mut command =
        build_recording_command(scrcpy_path, adb_path, serial, output_path, bitrate_mbps);
    command
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    let child = command
        .spawn()
        .map_err(|err| format!("Failed to start scrcpy recording for {serial}: {err}"))?;
    let pid = child.id();

    Ok(RecordingProcess {
        child: ManagedChild::new(child),
        pid,
        log,
    })
}

fn build_recording_command(
    scrcpy_path: &str,
    adb_path: &str,
    serial: &str,
    output_path: &Path,
    bitrate_mbps: u32,
) -> Command {
    let mut command = scrcpy_command(scrcpy_path);
    command
        .args(["--serial", serial])
        .env("ADB", adb_path)
        .args(["--no-playback", "--no-audio"])
        .arg(format!("--record={}", output_path.display()))
        .args(["--record-format=mp4"])
        .arg(format!("--video-bit-rate={bitrate_mbps}M"));
    command
}

fn build_command(
    scrcpy_path: &str,
    adb_path: &str,
    serial: &str,
    mode: LaunchMode,
    options: &ScrcpyLaunchOptions,
) -> Result<Command, ScrcpyArgsError> {
    let extra_args = parse_extra_args(&options.extra_args)?;
    let mut command = scrcpy_command(scrcpy_path);
    command.args(["--serial", serial]).env("ADB", adb_path);
    if let LaunchMode::NewDisplay(display_options) = mode {
        command.arg(display_options.argument());
        if let Some(start_app) = display_options.start_app {
            command.args(["--start-app", start_app.as_str()]);
        }
    }
    if options.no_control {
        command.arg("--no-control");
    }
    if options.no_audio {
        command.arg("--no-audio");
    }
    command.args(extra_args);
    Ok(command)
}

fn parse_positive_dimension(input: &str, label: &str) -> Result<Option<u32>, String> {
    let input = input.trim();
    if input.is_empty() {
        return Ok(None);
    }
    let value = input
        .parse::<u32>()
        .map_err(|_| format!("Virtual display {label} must be a positive integer."))?;
    if value == 0 {
        return Err(format!(
            "Virtual display {label} must be a positive integer."
        ));
    }
    Ok(Some(value))
}

fn parse_scrcpy_version(output: &str) -> Option<ScrcpyVersion> {
    let mut tokens = output.split_whitespace();
    while let Some(token) = tokens.next() {
        if token.eq_ignore_ascii_case("scrcpy") {
            return tokens.next().and_then(parse_version_token);
        }
    }
    None
}

fn parse_version_token(token: &str) -> Option<ScrcpyVersion> {
    let token = token.trim_start_matches('v');
    let numeric = token
        .split_once(|character: char| !character.is_ascii_digit() && character != '.')
        .map(|(numeric, _)| numeric)
        .unwrap_or(token);
    let mut parts = numeric.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some(ScrcpyVersion {
        major,
        minor,
        patch,
    })
}

fn combined_output(output: &Output) -> String {
    combined_text_output(
        &String::from_utf8_lossy(&output.stdout),
        &String::from_utf8_lossy(&output.stderr),
    )
}

fn combined_text_output(stdout: &str, stderr: &str) -> String {
    let stdout = stdout.trim();
    let stderr = stderr.trim();
    match (stdout.is_empty(), stderr.is_empty()) {
        (false, false) => format!("{stdout}\n{stderr}"),
        (false, true) => stdout.to_owned(),
        (true, false) => stderr.to_owned(),
        (true, true) => String::new(),
    }
}

fn scrcpy_command(scrcpy_path: &str) -> Command {
    let mut command = Command::new(scrcpy_path);
    hide_console_window(&mut command);
    command
}

#[cfg(target_os = "windows")]
fn hide_console_window(command: &mut Command) {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(target_os = "windows"))]
fn hide_console_window(_command: &mut Command) {}

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
    use super::{
        LaunchMode, MAX_EXTRA_ARG_BYTES, MAX_EXTRA_ARGS, MAX_EXTRA_ARGS_BYTES, NewDisplayOptions,
        ScrcpyArgsError, ScrcpyLaunchOptions, ScrcpyVersion, build_command,
        build_recording_command, parse_extra_args, parse_scrcpy_version,
    };
    use std::ffi::OsStr;

    #[test]
    fn parses_standard_and_prefixed_scrcpy_versions() {
        assert_eq!(
            parse_scrcpy_version("scrcpy 3.3.4 <https://github.com/Genymobile/scrcpy>"),
            Some(ScrcpyVersion {
                major: 3,
                minor: 3,
                patch: 4,
            })
        );
        assert_eq!(
            parse_scrcpy_version("scrcpy v4.0"),
            Some(ScrcpyVersion {
                major: 4,
                minor: 0,
                patch: 0,
            })
        );
    }

    #[test]
    fn recording_requires_scrcpy_two_or_later() {
        assert!(
            !ScrcpyVersion {
                major: 1,
                minor: 25,
                patch: 0,
            }
            .supports_recording()
        );
        assert!(
            ScrcpyVersion {
                major: 2,
                minor: 0,
                patch: 0,
            }
            .supports_recording()
        );
    }

    #[test]
    fn recording_command_is_headless_and_records_to_the_requested_mp4() {
        let command = build_recording_command(
            "scrcpy",
            "C:/Android/platform-tools/adb.exe",
            "192.168.0.8:5555",
            std::path::Path::new(r"C:\Recordings\pixel.mp4"),
            12,
        );
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(
            args,
            vec![
                OsStr::new("--serial"),
                OsStr::new("192.168.0.8:5555"),
                OsStr::new("--no-playback"),
                OsStr::new("--no-audio"),
                OsStr::new(r"--record=C:\Recordings\pixel.mp4"),
                OsStr::new("--record-format=mp4"),
                OsStr::new("--video-bit-rate=12M"),
            ]
        );
        assert_eq!(
            command
                .get_envs()
                .find(|(name, _)| *name == OsStr::new("ADB"))
                .and_then(|(_, value)| value),
            Some(OsStr::new("C:/Android/platform-tools/adb.exe"))
        );
    }

    #[test]
    fn new_display_requires_scrcpy_three_or_later() {
        assert!(
            !ScrcpyVersion {
                major: 2,
                minor: 7,
                patch: 0,
            }
            .supports_new_display()
        );
        assert!(
            ScrcpyVersion {
                major: 3,
                minor: 0,
                patch: 0,
            }
            .supports_new_display()
        );
    }

    #[test]
    fn new_display_arguments_cover_default_and_custom_sizes() {
        assert_eq!(NewDisplayOptions::default().argument(), "--new-display");
        assert_eq!(
            NewDisplayOptions::custom("1280", "960", "160")
                .expect("custom display")
                .argument(),
            "--new-display=1280x960/160"
        );
    }

    #[test]
    fn new_display_rejects_missing_or_invalid_dimensions() {
        assert!(NewDisplayOptions::custom("1280", "", "160").is_err());
        assert!(NewDisplayOptions::custom("0", "960", "160").is_err());
        assert!(NewDisplayOptions::custom("1280", "960", "invalid").is_err());
    }

    #[test]
    fn launch_command_uses_selected_device_and_configured_adb() {
        let command = build_command(
            "scrcpy",
            "C:/Android/platform-tools/adb.exe",
            "192.168.0.8:5555",
            LaunchMode::NewDisplay(
                NewDisplayOptions::custom("1280", "960", "160")
                    .expect("display options")
                    .with_start_app("com.android.settings")
                    .expect("start app"),
            ),
            &ScrcpyLaunchOptions::default(),
        )
        .expect("valid launch options");
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(
            args,
            vec![
                OsStr::new("--serial"),
                OsStr::new("192.168.0.8:5555"),
                OsStr::new("--new-display=1280x960/160"),
                OsStr::new("--start-app"),
                OsStr::new("com.android.settings"),
            ]
        );
        assert_eq!(
            command
                .get_envs()
                .find(|(name, _)| *name == OsStr::new("ADB"))
                .and_then(|(_, value)| value),
            Some(OsStr::new("C:/Android/platform-tools/adb.exe"))
        );
    }

    #[test]
    fn default_launch_options_preserve_the_original_mirror_command() {
        let options = ScrcpyLaunchOptions::default();
        assert!(!options.no_control);
        assert!(!options.no_audio);
        assert!(options.extra_args.is_empty());

        let command =
            build_command("scrcpy", "adb", "device-1", LaunchMode::Mirror, &options).unwrap();
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args, [OsStr::new("--serial"), OsStr::new("device-1")]);
    }

    #[test]
    fn fixed_launch_flags_are_independent_and_ordered() {
        for (no_control, no_audio, expected) in [
            (true, false, vec!["--no-control"]),
            (false, true, vec!["--no-audio"]),
            (true, true, vec!["--no-control", "--no-audio"]),
            (false, false, vec![]),
        ] {
            let command = build_command(
                "scrcpy",
                "adb",
                "device-1",
                LaunchMode::Mirror,
                &ScrcpyLaunchOptions {
                    no_control,
                    no_audio,
                    extra_args: String::new(),
                },
            )
            .unwrap();
            let args: Vec<_> = command.get_args().skip(2).collect();
            let expected: Vec<_> = expected.into_iter().map(OsStr::new).collect();
            assert_eq!(args, expected);
        }
    }

    #[test]
    fn mirror_appends_fixed_and_custom_arguments_after_the_managed_target() {
        let command = build_command(
            "scrcpy",
            "adb",
            "device-1",
            LaunchMode::Mirror,
            &ScrcpyLaunchOptions {
                no_control: true,
                no_audio: true,
                extra_args: "--max-size=1280\n--window-title=My Phone".into(),
            },
        )
        .unwrap();
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(
            args,
            [
                OsStr::new("--serial"),
                OsStr::new("device-1"),
                OsStr::new("--no-control"),
                OsStr::new("--no-audio"),
                OsStr::new("--max-size=1280"),
                OsStr::new("--window-title=My Phone"),
            ]
        );
    }

    #[test]
    fn new_display_keeps_mode_arguments_before_shared_launch_options() {
        let command = build_command(
            "scrcpy",
            "adb",
            "device-1",
            LaunchMode::NewDisplay(
                NewDisplayOptions::custom("1280", "960", "160")
                    .unwrap()
                    .with_start_app("com.android.settings")
                    .unwrap(),
            ),
            &ScrcpyLaunchOptions {
                no_control: false,
                no_audio: true,
                extra_args: "--max-size=1280".into(),
            },
        )
        .unwrap();
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(
            args,
            [
                OsStr::new("--serial"),
                OsStr::new("device-1"),
                OsStr::new("--new-display=1280x960/160"),
                OsStr::new("--start-app"),
                OsStr::new("com.android.settings"),
                OsStr::new("--no-audio"),
                OsStr::new("--max-size=1280"),
            ]
        );
    }

    #[test]
    fn extra_args_use_one_argv_per_nonempty_line() {
        let parsed = parse_extra_args(
            "  --max-size=1280  \r\n\n--video-codec\r\nh265\n--window-title=My Phone\n--record=C:\\Temp\\My Phone.mp4\n",
        )
        .unwrap();
        assert_eq!(
            parsed,
            [
                "--max-size=1280",
                "--video-codec",
                "h265",
                "--window-title=My Phone",
                r"--record=C:\Temp\My Phone.mp4",
            ]
        );
    }

    #[test]
    fn extra_args_reject_nul_and_every_device_selector_form() {
        assert_eq!(
            parse_extra_args("--max-size=1280\0"),
            Err(ScrcpyArgsError::ContainsNul)
        );
        for selector in [
            "--serial",
            "--serial=other",
            "--seri=other",
            "--ser=other",
            "--s=other",
            "--s",
            "-s",
            "-s=other",
            "-sother",
            "-nsother",
            "-nSs=other",
        ] {
            assert_eq!(
                parse_extra_args(selector),
                Err(ScrcpyArgsError::DeviceSelector { line: 1 }),
                "selector must be rejected: {selector}"
            );
        }
        assert_eq!(
            parse_extra_args("--serializable").unwrap(),
            ["--serializable"]
        );
        assert_eq!(parse_extra_args("--stay-awake").unwrap(), ["--stay-awake"]);
        assert_eq!(parse_extra_args("-nS").unwrap(), ["-nS"]);
        assert_eq!(
            parse_extra_args(r"-rC:\Videos\My Phone.mp4").unwrap(),
            [r"-rC:\Videos\My Phone.mp4"]
        );
        assert_eq!(parse_extra_args("-Vverbose").unwrap(), ["-Vverbose"]);
    }

    #[test]
    fn invalid_persisted_args_still_fail_command_construction() {
        let error = build_command(
            "scrcpy",
            "adb",
            "device-1",
            LaunchMode::Mirror,
            &ScrcpyLaunchOptions {
                extra_args: "--serial=other".into(),
                ..Default::default()
            },
        )
        .expect_err("builder revalidates manually edited config");
        assert_eq!(error, ScrcpyArgsError::DeviceSelector { line: 1 });
    }

    #[test]
    fn extra_args_enforce_single_argument_byte_limit() {
        let valid = "x".repeat(MAX_EXTRA_ARG_BYTES);
        assert_eq!(parse_extra_args(&valid).unwrap(), [valid]);
        let invalid = "x".repeat(MAX_EXTRA_ARG_BYTES + 1);
        assert_eq!(
            parse_extra_args(&invalid),
            Err(ScrcpyArgsError::ArgumentTooLong {
                line: 1,
                max_bytes: MAX_EXTRA_ARG_BYTES,
            })
        );
    }

    #[test]
    fn extra_args_enforce_input_and_count_limits() {
        let too_many = (0..=MAX_EXTRA_ARGS)
            .map(|index| format!("--window-title=Phone {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            parse_extra_args(&too_many),
            Err(ScrcpyArgsError::TooManyArguments {
                max: MAX_EXTRA_ARGS
            })
        );
        let oversized = "x".repeat(MAX_EXTRA_ARGS_BYTES + 1);
        assert_eq!(
            parse_extra_args(&oversized),
            Err(ScrcpyArgsError::InputTooLong {
                max_bytes: MAX_EXTRA_ARGS_BYTES
            })
        );
    }

    #[test]
    fn recording_command_never_reads_mirror_launch_options() {
        let launch_options = ScrcpyLaunchOptions {
            no_control: true,
            no_audio: false,
            extra_args: "--max-size=1280\n--window-title=Mirror Only".into(),
        };
        assert!(launch_options.no_control);
        let command = build_recording_command(
            "scrcpy",
            "adb",
            "device-1",
            std::path::Path::new("recording.mp4"),
            8,
        );
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(
            args,
            [
                OsStr::new("--serial"),
                OsStr::new("device-1"),
                OsStr::new("--no-playback"),
                OsStr::new("--no-audio"),
                OsStr::new("--record=recording.mp4"),
                OsStr::new("--record-format=mp4"),
                OsStr::new("--video-bit-rate=8M"),
            ]
        );
        assert!(!args.contains(&OsStr::new("--no-control")));
        assert!(!args.contains(&OsStr::new("--max-size=1280")));
        assert!(!args.contains(&OsStr::new("--window-title=Mirror Only")));
    }

    #[test]
    fn new_display_allows_an_optional_start_app_but_rejects_whitespace() {
        assert!(
            NewDisplayOptions::default()
                .with_start_app("com.example.app")
                .is_ok()
        );
        assert!(
            NewDisplayOptions::default()
                .with_start_app("com.example app")
                .is_err()
        );
    }
}
