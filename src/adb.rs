use crate::models::DeviceInfo;
use std::{
    fs::File,
    path::Path,
    process::{Child, Command, Stdio},
};

pub fn validate_adb_path(adb_path: &str) -> Result<(), String> {
    let trimmed = adb_path.trim();
    if trimmed.is_empty() {
        return Err("ADB executable path cannot be empty".to_owned());
    }

    let output = adb_command(trimmed)
        .arg("version")
        .output()
        .map_err(|err| format!("Failed to execute `{trimmed} version`: {err}"))?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "ADB validation failed for `{trimmed}`: {}",
            stderr.trim().if_empty("unknown error")
        ))
    }
}

pub fn list_devices(adb_path: &str) -> Result<Vec<DeviceInfo>, String> {
    let output = adb_command(adb_path)
        .arg("devices")
        .output()
        .map_err(|err| format!("Failed to run `{adb_path} devices`: {err}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "`{adb_path} devices` failed: {}",
            stderr.trim().if_empty("unknown error")
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(parse_devices_output(&stdout))
}

pub fn connect_device(adb_path: &str, target: &str) -> Result<String, String> {
    let target = target.trim();
    if target.is_empty() {
        return Err("Device endpoint cannot be empty".to_owned());
    }

    let output = adb_command(adb_path)
        .args(["connect", target])
        .output()
        .map_err(|err| format!("Failed to run `{adb_path} connect {target}`: {err}"))?;

    parse_connect_output(target, &output)
}

pub fn spawn_logcat(adb_path: &str, serial: &str, output_path: &Path) -> Result<Child, String> {
    let parent = output_path
        .parent()
        .ok_or_else(|| format!("Invalid output path: {}", output_path.display()))?;
    std::fs::create_dir_all(parent).map_err(|err| {
        format!(
            "Failed to create output directory {}: {err}",
            parent.display()
        )
    })?;

    let stdout_file = File::create(output_path)
        .map_err(|err| format!("Failed to create log file {}: {err}", output_path.display()))?;
    let stderr_file = stdout_file.try_clone().map_err(|err| {
        format!(
            "Failed to prepare stderr log file {}: {err}",
            output_path.display()
        )
    })?;

    adb_command(adb_path)
        .args(["-s", serial, "logcat"])
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file))
        .spawn()
        .map_err(|err| format!("Failed to start logcat for {serial}: {err}"))
}

fn adb_command(adb_path: &str) -> Command {
    let mut command = Command::new(adb_path);
    hide_window(&mut command);
    command
}

#[cfg(target_os = "windows")]
fn hide_window(command: &mut Command) {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(target_os = "windows"))]
fn hide_window(_command: &mut Command) {}

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

fn parse_devices_output(stdout: &str) -> Vec<DeviceInfo> {
    stdout
        .lines()
        .skip(1)
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            let mut parts = line.split_whitespace();
            let serial = parts.next()?;
            let state = parts.next().unwrap_or("unknown");
            Some(DeviceInfo {
                serial: serial.to_owned(),
                state: state.to_owned(),
            })
        })
        .collect()
}

fn parse_connect_output(target: &str, output: &std::process::Output) -> Result<String, String> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let combined = [stdout.trim(), stderr.trim()]
        .into_iter()
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let lower = combined.to_ascii_lowercase();

    if output.status.success()
        && (lower.contains("connected to") || lower.contains("already connected to"))
    {
        return Ok(if combined.is_empty() {
            format!("Connected to {target}.")
        } else {
            combined
        });
    }

    if combined.is_empty() {
        Err(format!("Failed to connect to {target}: unknown error"))
    } else {
        Err(format!("Failed to connect to {target}: {combined}"))
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_connect_output, parse_devices_output};
    use std::process::Output;

    #[test]
    fn parse_devices_output_parses_multiple_device_states() {
        let output = "\
List of devices attached
emulator-5554\tdevice
ZY223JQ9K\toffline
0123456789ABCDEF\tunauthorized
";

        let devices = parse_devices_output(output);
        assert_eq!(devices.len(), 3);
        assert_eq!(devices[0].serial, "emulator-5554");
        assert_eq!(devices[0].state, "device");
        assert_eq!(devices[1].state, "offline");
        assert_eq!(devices[2].state, "unauthorized");
    }

    #[test]
    fn parse_connect_output_accepts_connected_message() {
        let output = Output {
            status: exit_status(0),
            stdout: b"connected to 192.168.0.8:5555".to_vec(),
            stderr: Vec::new(),
        };

        let message = parse_connect_output("192.168.0.8:5555", &output).expect("connect success");
        assert!(message.contains("connected to 192.168.0.8:5555"));
    }

    #[test]
    fn parse_connect_output_rejects_failed_message() {
        let output = Output {
            status: exit_status(1),
            stdout: b"".to_vec(),
            stderr: b"failed to connect".to_vec(),
        };

        let error = parse_connect_output("192.168.0.8:5555", &output).expect_err("connect error");
        assert!(error.contains("failed to connect"));
    }

    #[cfg(unix)]
    fn exit_status(code: i32) -> std::process::ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(code)
    }

    #[cfg(windows)]
    fn exit_status(code: u32) -> std::process::ExitStatus {
        use std::os::windows::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(code)
    }
}
