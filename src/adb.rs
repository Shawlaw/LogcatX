use crate::models::DeviceInfo;
use std::{
    fs::File,
    path::Path,
    process::{Child, Command, Output, Stdio},
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
    let mut devices = parse_devices_output(&stdout);
    for device in &mut devices {
        if device.state == "device" {
            device.android_version = query_android_version(adb_path, &device.serial);
        }
    }
    Ok(devices)
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

pub fn disconnect_device(adb_path: &str, target: &str) -> Result<String, String> {
    let target = target.trim();
    if target.is_empty() {
        return Err("Device endpoint cannot be empty".to_owned());
    }

    let output = adb_command(adb_path)
        .args(["disconnect", target])
        .output()
        .map_err(|err| format!("Failed to run `{adb_path} disconnect {target}`: {err}"))?;

    parse_disconnect_output(target, &output)
}

pub fn restart_server(adb_path: &str) -> Result<String, String> {
    let kill_output = adb_command(adb_path)
        .arg("kill-server")
        .output()
        .map_err(|err| format!("Failed to run `{adb_path} kill-server`: {err}"))?;
    if !kill_output.status.success() {
        return Err(format!(
            "Failed to stop the ADB server: {}",
            combined_output(&kill_output).if_empty("unknown error")
        ));
    }

    let start_output = adb_command(adb_path)
        .arg("start-server")
        .output()
        .map_err(|err| format!("Failed to run `{adb_path} start-server`: {err}"))?;
    if !start_output.status.success() {
        return Err(format!(
            "Failed to start the ADB server: {}",
            combined_output(&start_output).if_empty("unknown error")
        ));
    }

    let message = [
        combined_output(&kill_output),
        combined_output(&start_output),
    ]
    .into_iter()
    .filter(|text| !text.is_empty())
    .collect::<Vec<_>>()
    .join("\n");
    if message.is_empty() {
        Ok("ADB server restarted.".to_owned())
    } else {
        Ok(message)
    }
}

pub fn install_apk(adb_path: &str, serial: &str, apk_path: &Path) -> Result<String, String> {
    let output = adb_command(adb_path)
        .args(["-s", serial, "install", "-r"])
        .arg(apk_path)
        .output()
        .map_err(|err| {
            format!(
                "Failed to run `{adb_path} -s {serial} install -r {}`: {err}",
                apk_path.display()
            )
        })?;

    let combined = combined_output(&output);
    if output.status.success() {
        if combined.is_empty() {
            Ok(format!("Installed {}.", apk_path.display()))
        } else {
            Ok(combined)
        }
    } else {
        Err(format!(
            "Failed to install {} on {serial}: {}",
            apk_path.display(),
            combined.if_empty("unknown error")
        ))
    }
}

pub fn push_file(
    adb_path: &str,
    serial: &str,
    source_path: &Path,
    remote_path: &str,
) -> Result<String, String> {
    let output = adb_command(adb_path)
        .args(["-s", serial, "push"])
        .arg(source_path)
        .arg(remote_path)
        .output()
        .map_err(|err| {
            format!(
                "Failed to run `{adb_path} -s {serial} push {} {remote_path}`: {err}",
                source_path.display()
            )
        })?;

    let combined = combined_output(&output);
    if output.status.success() {
        if combined.is_empty() {
            Ok(format!(
                "Pushed {} to {remote_path}.",
                source_path.display()
            ))
        } else {
            Ok(combined)
        }
    } else {
        Err(format!(
            "Failed to push {} to {remote_path}: {}",
            source_path.display(),
            combined.if_empty("unknown error")
        ))
    }
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

pub fn is_network_device_serial(serial: &str) -> bool {
    let Some((host, port)) = serial.trim().rsplit_once(':') else {
        return false;
    };

    !host.is_empty() && !host.starts_with("emulator-") && port.chars().all(|ch| ch.is_ascii_digit())
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
                android_version: None,
            })
        })
        .collect()
}

fn query_android_version(adb_path: &str, serial: &str) -> Option<String> {
    let release_or_codename =
        adb_shell_getprop(adb_path, serial, "ro.build.version.release_or_codename");
    let release = adb_shell_getprop(adb_path, serial, "ro.build.version.release");

    match release_or_codename
        .as_deref()
        .or(release.as_deref())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        Some(value) => Some(format_android_version(value)),
        None => None,
    }
}

fn adb_shell_getprop(adb_path: &str, serial: &str, key: &str) -> Option<String> {
    let output = adb_command(adb_path)
        .args(["-s", serial, "shell", "getprop", key])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let value = stdout.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_owned())
    }
}

fn format_android_version(version: &str) -> String {
    let trimmed = version.trim();
    if trimmed.is_empty() {
        String::new()
    } else if trimmed.to_ascii_lowercase().starts_with("android ") {
        trimmed.to_owned()
    } else {
        format!("Android {trimmed}")
    }
}

fn combined_output(output: &Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    [stdout.trim(), stderr.trim()]
        .into_iter()
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn parse_connect_output(target: &str, output: &Output) -> Result<String, String> {
    let combined = combined_output(output);
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

fn parse_disconnect_output(target: &str, output: &Output) -> Result<String, String> {
    let combined = combined_output(output);
    let lower = combined.to_ascii_lowercase();

    if output.status.success()
        && (lower.contains("disconnected")
            || lower.contains("no such device")
            || lower.contains("not connected"))
    {
        return Ok(if combined.is_empty() {
            format!("Disconnected {target}.")
        } else {
            combined
        });
    }

    if combined.is_empty() {
        Err(format!("Failed to disconnect {target}: unknown error"))
    } else {
        Err(format!("Failed to disconnect {target}: {combined}"))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        format_android_version, is_network_device_serial, parse_connect_output,
        parse_devices_output, parse_disconnect_output,
    };
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
        assert_eq!(devices[0].android_version, None);
        assert_eq!(devices[1].state, "offline");
        assert_eq!(devices[2].state, "unauthorized");
    }

    #[test]
    fn format_android_version_prefixes_plain_versions() {
        assert_eq!(format_android_version("14"), "Android 14");
        assert_eq!(format_android_version("Android 15"), "Android 15");
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

    #[test]
    fn parse_disconnect_output_accepts_success_and_missing_device() {
        let success = Output {
            status: exit_status(0),
            stdout: b"disconnected 192.168.0.8:5555".to_vec(),
            stderr: Vec::new(),
        };
        let missing = Output {
            status: exit_status(0),
            stdout: Vec::new(),
            stderr: b"no such device '192.168.0.8:5555'".to_vec(),
        };

        assert!(
            parse_disconnect_output("192.168.0.8:5555", &success)
                .expect("disconnect success")
                .contains("disconnected 192.168.0.8:5555")
        );
        assert!(
            parse_disconnect_output("192.168.0.8:5555", &missing)
                .expect("already disconnected")
                .contains("no such device")
        );
    }

    #[test]
    fn network_device_serial_detection_ignores_usb_and_emulators() {
        assert!(is_network_device_serial("192.168.0.8:5555"));
        assert!(is_network_device_serial("localhost:5555"));
        assert!(!is_network_device_serial("emulator-5554"));
        assert!(!is_network_device_serial("ZY223JQ9K"));
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
