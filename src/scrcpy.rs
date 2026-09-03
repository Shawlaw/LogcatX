use std::{
    cmp::Ordering,
    process::{Command, Output},
};

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
}

impl NewDisplayOptions {
    pub fn custom(width: &str, height: &str, dpi: &str) -> Result<Self, String> {
        let width = parse_positive_dimension(width, "width")?;
        let height = parse_positive_dimension(height, "height")?;
        let dpi = parse_positive_dimension(dpi, "DPI")?;

        if width.is_none() || height.is_none() {
            return Err("Both virtual display width and height are required.".to_owned());
        }

        Ok(Self { width, height, dpi })
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
) -> Result<(), String> {
    build_command(scrcpy_path, adb_path, serial, mode)
        .spawn()
        .map(|_| ())
        .map_err(|err| format!("Failed to start scrcpy for {serial}: {err}"))
}

fn build_command(scrcpy_path: &str, adb_path: &str, serial: &str, mode: LaunchMode) -> Command {
    let mut command = scrcpy_command(scrcpy_path);
    command.args(["--serial", serial]).env("ADB", adb_path);
    if let LaunchMode::NewDisplay(options) = mode {
        command.arg(options.argument());
    }
    command
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
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    match (stdout.is_empty(), stderr.is_empty()) {
        (false, false) => format!("{stdout}\n{stderr}"),
        (false, true) => stdout,
        (true, false) => stderr,
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
        LaunchMode, NewDisplayOptions, ScrcpyVersion, build_command, parse_scrcpy_version,
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
                NewDisplayOptions::custom("1280", "960", "160").expect("display options"),
            ),
        );
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(
            args,
            vec![
                OsStr::new("--serial"),
                OsStr::new("192.168.0.8:5555"),
                OsStr::new("--new-display=1280x960/160"),
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
}
