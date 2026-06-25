use crate::{
    managed_child::ManagedChild,
    models::{DeviceInfo, ForegroundApp},
};
use std::{
    fs::File,
    path::Path,
    process::{Command, Output, Stdio},
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
            let metadata = query_device_metadata(adb_path, &device.serial);
            if let Some(identity_key) = metadata.identity_key {
                device.identity_key = identity_key;
            }
            device.android_version = metadata.android_version;
            device.manufacturer = metadata.manufacturer;
            device.model = metadata.model;
        }
    }
    Ok(devices)
}

pub fn query_foreground_app(adb_path: &str, serial: &str) -> Result<ForegroundApp, String> {
    let activity_output =
        adb_shell_command(adb_path, serial, &["dumpsys", "activity", "activities"])
            .map_err(|err| format!("Failed to query foreground app for {serial}: {err}"))?;
    if activity_output.status.success() {
        let stdout = String::from_utf8_lossy(&activity_output.stdout);
        if let Some(app) = parse_foreground_app_from_activity_dump(&stdout) {
            return Ok(app);
        }
    }

    let window_output = adb_shell_command(adb_path, serial, &["dumpsys", "window", "windows"])
        .map_err(|err| format!("Failed to query foreground app for {serial}: {err}"))?;
    if window_output.status.success() {
        let stdout = String::from_utf8_lossy(&window_output.stdout);
        if let Some(app) = parse_foreground_app_from_window_dump(&stdout) {
            return Ok(app);
        }
    }

    let activity_output_text = combined_output(&activity_output);
    let window_output_text = combined_output(&window_output);
    let activity_details = activity_output_text.if_empty("no output");
    let window_details = window_output_text.if_empty("no output");
    Err(format!(
        "Failed to determine the current foreground app for {serial}. Activity dump: {activity_details}; window dump: {window_details}"
    ))
}

pub fn force_stop_package(adb_path: &str, serial: &str, package: &str) -> Result<String, String> {
    let output =
        adb_shell_command(adb_path, serial, &["am", "force-stop", package]).map_err(|err| {
            format!("Failed to run `{adb_path} -s {serial} shell am force-stop {package}`: {err}")
        })?;

    let combined = combined_output(&output);
    if output.status.success() {
        if combined.is_empty() {
            Ok(format!("Force-stopped {package}."))
        } else {
            Ok(combined)
        }
    } else {
        Err(format!(
            "Failed to force-stop {package} on {serial}: {}",
            combined.if_empty("unknown error")
        ))
    }
}

pub fn clear_package_data(adb_path: &str, serial: &str, package: &str) -> Result<String, String> {
    let output = adb_shell_command(adb_path, serial, &["pm", "clear", package]).map_err(|err| {
        format!("Failed to run `{adb_path} -s {serial} shell pm clear {package}`: {err}")
    })?;

    if package_command_succeeded(&output) {
        return Ok(package_command_success_message(
            &output,
            format!("Cleared data for {package}."),
        ));
    }

    let primary_error = format!(
        "Failed to clear data for {package} on {serial}: {}",
        combined_output(&output).if_empty("unknown error")
    );
    if !should_retry_clear_with_run_as(&output) {
        return Err(primary_error);
    }

    let run_as_output = adb_shell_command(adb_path, serial, &["run-as", package, "pm", "clear", package])
        .map_err(|err| {
            format!(
                "{primary_error}\nFailed to run `{adb_path} -s {serial} shell run-as {package} pm clear {package}`: {err}"
            )
        })?;

    if package_command_succeeded(&run_as_output) {
        let combined = combined_output(&run_as_output);
        return Ok(if combined.is_empty() {
            format!("Cleared data for {package} via run-as fallback.")
        } else {
            format!("{combined}\n(run-as fallback)")
        });
    }

    Err(format!(
        "{primary_error}\nRetry via `run-as {package} pm clear {package}` also failed: {}",
        combined_output(&run_as_output).if_empty("unknown error")
    ))
}

pub fn uninstall_package(adb_path: &str, serial: &str, package: &str) -> Result<String, String> {
    let output =
        adb_shell_command(adb_path, serial, &["pm", "uninstall", package]).map_err(|err| {
            format!("Failed to run `{adb_path} -s {serial} shell pm uninstall {package}`: {err}")
        })?;

    if package_command_succeeded(&output) {
        Ok(package_command_success_message(
            &output,
            format!("Uninstalled {package}."),
        ))
    } else {
        Err(format!(
            "Failed to uninstall {package} on {serial}: {}",
            combined_output(&output).if_empty("unknown error")
        ))
    }
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

pub fn spawn_logcat(
    adb_path: &str,
    serial: &str,
    output_path: &Path,
    extra_args: &[String],
) -> Result<ManagedChild, String> {
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

    let mut cmd = adb_command(adb_path);
    cmd.args(["-s", serial, "logcat"])
        .args(extra_args)
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file));

    cmd.spawn()
        .map(ManagedChild::new)
        .map_err(|err| format!("Failed to start logcat for {serial}: {err}"))
}

pub fn parse_logcat_args(input: &str) -> Vec<String> {
    let input = input.trim();
    if input.is_empty() {
        return Vec::new();
    }

    let mut args = Vec::new();
    let mut chars = input.chars().peekable();

    while let Some(&ch) = chars.peek() {
        if ch.is_whitespace() {
            chars.next();
            continue;
        }

        if ch == '"' {
            chars.next();
            let mut token = String::new();
            while let Some(&c) = chars.peek() {
                if c == '"' {
                    chars.next();
                    break;
                }
                token.push(chars.next().unwrap());
            }
            args.push(token);
        } else if ch == '\'' {
            chars.next();
            let mut token = String::new();
            while let Some(&c) = chars.peek() {
                if c == '\'' {
                    chars.next();
                    break;
                }
                token.push(chars.next().unwrap());
            }
            args.push(token);
        } else {
            let mut token = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() {
                    break;
                }
                token.push(chars.next().unwrap());
            }
            args.push(token);
        }
    }

    args
}

fn adb_command(adb_path: &str) -> Command {
    let mut command = Command::new(adb_path);
    hide_window(&mut command);
    command
}

fn adb_shell_command(adb_path: &str, serial: &str, args: &[&str]) -> Result<Output, String> {
    adb_command(adb_path)
        .args(["-s", serial, "shell"])
        .args(args)
        .output()
        .map_err(|err| {
            format!(
                "Failed to run `{adb_path} -s {serial} shell {}`: {err}",
                args.join(" ")
            )
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

#[derive(Default)]
struct DeviceMetadata {
    identity_key: Option<String>,
    android_version: Option<String>,
    manufacturer: Option<String>,
    model: Option<String>,
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
                identity_key: serial.to_owned(),
                state: state.to_owned(),
                android_version: None,
                manufacturer: None,
                model: None,
            })
        })
        .collect()
}

fn query_device_metadata(adb_path: &str, serial: &str) -> DeviceMetadata {
    DeviceMetadata {
        identity_key: query_device_identity(adb_path, serial),
        android_version: query_android_version(adb_path, serial),
        manufacturer: adb_shell_getprop(adb_path, serial, "ro.product.manufacturer")
            .or_else(|| adb_shell_getprop(adb_path, serial, "ro.product.brand")),
        model: adb_shell_getprop(adb_path, serial, "ro.product.model"),
    }
}

fn query_device_identity(adb_path: &str, serial: &str) -> Option<String> {
    adb_shell_getprop(adb_path, serial, "ro.serialno")
        .or_else(|| adb_shell_getprop(adb_path, serial, "ro.boot.serialno"))
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
    let value = normalize_shell_value(stdout.as_ref());
    if value.is_empty() { None } else { Some(value) }
}

fn normalize_shell_value(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
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

fn package_command_succeeded(output: &Output) -> bool {
    let combined = combined_output(output);
    let lower = combined.to_ascii_lowercase();
    output.status.success() && (combined.is_empty() || lower.contains("success"))
}

fn package_command_success_message(output: &Output, fallback: String) -> String {
    let combined = combined_output(output);
    if combined.is_empty() {
        fallback
    } else {
        combined
    }
}

fn should_retry_clear_with_run_as(output: &Output) -> bool {
    let lower = combined_output(output).to_ascii_lowercase();
    lower.contains("android.permission.clear_app_user_data")
        || (lower.contains("securityexception")
            && lower.contains("clear")
            && (lower.contains("user data") || lower.contains("applicationuserdata")))
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

fn parse_foreground_app_from_activity_dump(output: &str) -> Option<ForegroundApp> {
    for line in output.lines() {
        let line = line.trim();
        if !line.contains("ResumedActivity") && !line.contains("topResumedActivity") {
            continue;
        }
        if let Some(app) = extract_foreground_app_from_line(line) {
            return Some(app);
        }
    }
    None
}

fn parse_foreground_app_from_window_dump(output: &str) -> Option<ForegroundApp> {
    for line in output.lines() {
        let line = line.trim();
        if !line.contains("mCurrentFocus") && !line.contains("mFocusedApp") {
            continue;
        }
        if let Some(app) = extract_foreground_app_from_line(line) {
            return Some(app);
        }
    }
    None
}

fn extract_foreground_app_from_line(line: &str) -> Option<ForegroundApp> {
    line.split_whitespace().find_map(parse_component_token)
}

fn parse_component_token(token: &str) -> Option<ForegroundApp> {
    let cleaned = token.trim_matches(|ch: char| {
        matches!(
            ch,
            '{' | '}' | '(' | ')' | '[' | ']' | ',' | ';' | ':' | '"' | '\''
        )
    });
    let (package, activity) = cleaned.split_once('/')?;
    if !is_valid_package_name(package) {
        return None;
    }

    let activity = activity.trim_matches(|ch: char| {
        matches!(
            ch,
            '{' | '}' | '(' | ')' | '[' | ']' | ',' | ';' | ':' | '"' | '\''
        )
    });
    if activity.is_empty() {
        return None;
    }

    let activity_name = if activity.starts_with('.') {
        format!("{package}{activity}")
    } else {
        activity.to_owned()
    };

    Some(ForegroundApp {
        package_name: package.to_owned(),
        activity_name: Some(activity_name),
    })
}

fn is_valid_package_name(package: &str) -> bool {
    (package == "android" || package.contains('.'))
        && package
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '$' | '-'))
}

#[cfg(test)]
mod tests {
    use super::{
        format_android_version, is_network_device_serial, package_command_succeeded,
        parse_component_token, parse_connect_output, parse_devices_output, parse_disconnect_output,
        parse_foreground_app_from_activity_dump, parse_foreground_app_from_window_dump,
        parse_logcat_args, should_retry_clear_with_run_as,
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
        assert_eq!(devices[0].identity_key, "emulator-5554");
        assert_eq!(devices[0].state, "device");
        assert_eq!(devices[0].android_version, None);
        assert_eq!(devices[0].manufacturer, None);
        assert_eq!(devices[0].model, None);
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

    #[test]
    fn parse_component_token_expands_relative_activity_names() {
        let app =
            parse_component_token("com.example/.MainActivity}").expect("foreground component");
        assert_eq!(app.package_name, "com.example");
        assert_eq!(
            app.activity_name.as_deref(),
            Some("com.example.MainActivity")
        );
    }

    #[test]
    fn parse_foreground_app_from_activity_dump_detects_resumed_activity() {
        let output = "\
mResumedActivity: ActivityRecord{829f731 u0 com.tencent.mm/com.tencent.mm.ui.LauncherUI t198}
";

        let app = parse_foreground_app_from_activity_dump(output).expect("foreground app");
        assert_eq!(app.package_name, "com.tencent.mm");
        assert_eq!(
            app.activity_name.as_deref(),
            Some("com.tencent.mm.ui.LauncherUI")
        );
    }

    #[test]
    fn parse_foreground_app_from_window_dump_detects_current_focus() {
        let output = "\
mCurrentFocus=Window{41dff5a u0 com.android.settings/com.android.settings.Settings}
";

        let app = parse_foreground_app_from_window_dump(output).expect("foreground app");
        assert_eq!(app.package_name, "com.android.settings");
        assert_eq!(
            app.activity_name.as_deref(),
            Some("com.android.settings.Settings")
        );
    }

    #[test]
    fn clear_data_run_as_fallback_detects_permission_failure() {
        let output = Output {
            status: exit_status(1),
            stdout: Vec::new(),
            stderr: b"Exception occurred while executing 'clear':\njava.lang.SecurityException: PID 16791 does not have permission android.permission.CLEAR_APP_USER_DATA to clear data of package com.example.app".to_vec(),
        };

        assert!(should_retry_clear_with_run_as(&output));
    }

    #[test]
    fn clear_data_run_as_fallback_ignores_unrelated_failures() {
        let output = Output {
            status: exit_status(1),
            stdout: Vec::new(),
            stderr: b"Failed\nUnknown package: com.example.app".to_vec(),
        };

        assert!(!should_retry_clear_with_run_as(&output));
    }

    #[test]
    fn package_command_succeeded_accepts_empty_or_success_output() {
        let empty_success = Output {
            status: exit_status(0),
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        let explicit_success = Output {
            status: exit_status(0),
            stdout: b"Success".to_vec(),
            stderr: Vec::new(),
        };
        let failed = Output {
            status: exit_status(1),
            stdout: b"Success".to_vec(),
            stderr: Vec::new(),
        };

        assert!(package_command_succeeded(&empty_success));
        assert!(package_command_succeeded(&explicit_success));
        assert!(!package_command_succeeded(&failed));
    }

    #[test]
    fn parse_logcat_args_splits_simple_flags() {
        assert_eq!(
            parse_logcat_args("-v threadtime -t 100"),
            vec!["-v", "threadtime", "-t", "100"],
        );
    }

    #[test]
    fn parse_logcat_args_handles_double_quoted_values() {
        assert_eq!(
            parse_logcat_args("-e \"some pattern\" -s Tag:V"),
            vec!["-e", "some pattern", "-s", "Tag:V"],
        );
    }

    #[test]
    fn parse_logcat_args_handles_single_quoted_values() {
        assert_eq!(
            parse_logcat_args("-e 'hello world'"),
            vec!["-e", "hello world"],
        );
    }

    #[test]
    fn parse_logcat_args_returns_empty_for_blank_input() {
        assert_eq!(parse_logcat_args(""), Vec::<String>::new());
        assert_eq!(parse_logcat_args("   "), Vec::<String>::new());
    }

    #[test]
    fn parse_logcat_args_handles_mixed_quotes_and_flags() {
        assert_eq!(
            parse_logcat_args("-v threadtime -e \"WindowManager:*\" *:E"),
            vec!["-v", "threadtime", "-e", "WindowManager:*", "*:E"],
        );
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
