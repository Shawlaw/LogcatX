use crate::{
    adb_executor::{
        AdbError, AdbExecutor, AdbOutput, DEFAULT_STDOUT_LIMIT as SHELL_STDOUT_LIMIT, ExecOptions,
    },
    managed_child::ManagedChild,
    models::{DeviceInfo, ForegroundApp, Screenshot},
};
use std::{
    fs::File,
    path::{Path, PathBuf},
    process::Stdio,
    thread,
    time::Duration,
};

/// Timeout budget per adb operation class (PRD §15).
const DEVICES_TIMEOUT: Duration = Duration::from_secs(10);
const SHELL_TIMEOUT: Duration = Duration::from_secs(10);
const DUMPSYS_TIMEOUT: Duration = Duration::from_secs(15);
const SCREENSHOT_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(12);
const DISCONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const SERVER_RESTART_TIMEOUT: Duration = Duration::from_secs(30);
const INSTALL_TIMEOUT: Duration = Duration::from_secs(600);
/// How long to wait before retrying a transient `adb devices` failure.
const TRANSIENT_RETRY_DELAY: Duration = Duration::from_millis(600);
/// Post-restart readiness probes: a freshly started daemon often faults its
/// very first client query or answers before USB enumeration completes.
const SERVER_RESTART_PROBES: u32 = 3;
const SERVER_RESTART_PROBE_DELAY: Duration = Duration::from_millis(750);
/// dumpsys activity/window dumps can be large; give them a raised cap.
const DUMPSYS_STDOUT_LIMIT: usize = 4 * 1024 * 1024;
/// Screen captures are PNG payloads; keep generous headroom for tall screens.
const SCREENSHOT_STDOUT_LIMIT: usize = 32 * 1024 * 1024;

/// A completed adb command as seen by the parsing helpers.
struct ShellOutcome {
    success: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

impl From<AdbOutput> for ShellOutcome {
    fn from(output: AdbOutput) -> Self {
        Self {
            success: output.success(),
            stdout: output.stdout,
            stderr: output.stderr,
        }
    }
}

pub fn validate_adb_path(adb_path: &str) -> Result<(), String> {
    let trimmed = adb_path.trim();
    if trimmed.is_empty() {
        return Err("ADB executable path cannot be empty".to_owned());
    }

    let output = AdbExecutor::new(trimmed)
        .execute_with_timeout(&["version"], SHELL_TIMEOUT)
        .map_err(|err| format!("Failed to execute `{trimmed} version`: {err}"))?;

    if output.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(format!(
            "ADB validation failed for `{trimmed}`: {}",
            stderr.trim().if_empty("unknown error")
        ))
    }
}

/// Outcome of a device discovery round: the snapshot plus the (possibly
/// updated) metadata cache, returned so the caller can keep owning it.
#[derive(Debug)]
pub struct DiscoveryOutcome {
    pub devices: Vec<DeviceInfo>,
    pub metadata_cache: DeviceMetadataCache,
}

/// Static device metadata cached across polls (PRD §17). Re-queried only on
/// first discovery, reconnect (serial pruned then seen again), manual
/// refresh, or identity change — not on every two-second poll.
#[derive(Clone, Default, Debug)]
pub struct DeviceMetadataCache {
    entries: std::collections::HashMap<String, DeviceMetadata>,
}

impl DeviceMetadataCache {
    pub fn get(&self, serial: &str) -> Option<&DeviceMetadata> {
        self.entries.get(serial)
    }

    pub fn insert(&mut self, serial: &str, metadata: DeviceMetadata) {
        self.entries.insert(serial.to_owned(), metadata);
    }

    /// Prune entries for serials that are no longer connected; a serial that
    /// reconnects later is treated as a fresh discovery and re-queried.
    pub fn retain_serials(&mut self, connected: &[String]) {
        self.entries
            .retain(|serial, _| connected.iter().any(|current| current == serial));
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

pub fn list_devices(
    adb_path: &str,
    mut metadata_cache: DeviceMetadataCache,
    force_metadata_refresh: bool,
) -> Result<DiscoveryOutcome, String> {
    let mut output = run_devices(adb_path)?;
    // The daemon's smart socket can reset the first query after it (re)starts
    // ("protocol fault … connection reset"); one quick retry keeps that
    // transient fault from surfacing as a user-visible error.
    if !output.success() && is_transient_adb_failure(&String::from_utf8_lossy(&output.stderr)) {
        thread::sleep(TRANSIENT_RETRY_DELAY);
        output = run_devices(adb_path)?;
    }

    if !output.success() {
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
            if !force_metadata_refresh
                && let Some(cached) = metadata_cache.get(&device.serial).cloned()
            {
                apply_metadata(device, &cached);
                continue;
            }
            let metadata = query_device_metadata(adb_path, &device.serial);
            metadata_cache.insert(&device.serial, metadata.clone());
            apply_metadata(device, &metadata);
        }
    }
    Ok(DiscoveryOutcome {
        devices,
        metadata_cache,
    })
}

fn apply_metadata(device: &mut DeviceInfo, metadata: &DeviceMetadata) {
    if let Some(identity_key) = &metadata.identity_key {
        device.identity_key = identity_key.clone();
    }
    device.android_version = metadata.android_version.clone();
    device.manufacturer = metadata.manufacturer.clone();
    device.model = metadata.model.clone();
}

pub fn capture_screenshot(adb_path: &str, serial: &str) -> Result<Screenshot, String> {
    let output = AdbExecutor::new(adb_path)
        .execute_with_options(
            &["-s", serial, "exec-out", "screencap", "-p"],
            ExecOptions {
                timeout: Some(SCREENSHOT_TIMEOUT),
                stdout_limit: Some(SCREENSHOT_STDOUT_LIMIT),
                ..Default::default()
            },
        )
        .map_err(|err| format!("Failed to capture screen from {serial}: {err}"))?;

    if !output.success() {
        let details = combined_output(&ShellOutcome::from(output));
        return Err(format!(
            "Failed to capture screen from {serial}: {}",
            details.if_empty("unknown error")
        ));
    }

    decode_screenshot_png(&output.stdout)
        .map_err(|err| format!("Failed to decode screen capture from {serial}: {err}"))
}

pub fn list_installed_packages(adb_path: &str, serial: &str) -> Result<Vec<String>, String> {
    let primary = adb_shell(adb_path, serial, &["cmd", "package", "list", "packages"])
        .map_err(|err| format!("Failed to list packages on {serial}: {err}"))?;
    if primary.success {
        return Ok(parse_installed_packages(&String::from_utf8_lossy(
            &primary.stdout,
        )));
    }

    let fallback = adb_shell(adb_path, serial, &["pm", "list", "packages"])
        .map_err(|err| format!("Failed to list packages on {serial}: {err}"))?;
    if fallback.success {
        return Ok(parse_installed_packages(&String::from_utf8_lossy(
            &fallback.stdout,
        )));
    }

    let fallback_details = combined_output(&fallback);
    let primary_details = combined_output(&primary);
    Err(format!(
        "Failed to list packages on {serial}: {}",
        fallback_details.if_empty(primary_details.if_empty("unknown error"))
    ))
}

fn decode_screenshot_png(png: &[u8]) -> Result<Screenshot, String> {
    let image = image::load_from_memory_with_format(png, image::ImageFormat::Png)
        .map_err(|err| err.to_string())?
        .into_rgba8();

    Ok(Screenshot {
        width: image.width() as usize,
        height: image.height() as usize,
        rgba_pixels: image.into_raw(),
    })
}

fn parse_installed_packages(output: &str) -> Vec<String> {
    let mut packages: Vec<String> = output
        .lines()
        .filter_map(|line| line.trim().strip_prefix("package:"))
        .map(str::trim)
        .filter(|package| !package.is_empty())
        .map(str::to_owned)
        .collect();
    packages.sort_unstable();
    packages.dedup();
    packages
}

pub fn query_foreground_app(adb_path: &str, serial: &str) -> Result<ForegroundApp, String> {
    let activity_output = adb_shell_with_limit(
        adb_path,
        serial,
        &["dumpsys", "activity", "activities"],
        DUMPSYS_TIMEOUT,
        DUMPSYS_STDOUT_LIMIT,
    )
    .map_err(|err| format!("Failed to query foreground app for {serial}: {err}"))?;
    if activity_output.success {
        let stdout = String::from_utf8_lossy(&activity_output.stdout);
        if let Some(app) = parse_foreground_app_from_activity_dump(&stdout) {
            return Ok(app);
        }
    }

    let window_output = adb_shell_with_limit(
        adb_path,
        serial,
        &["dumpsys", "window", "windows"],
        DUMPSYS_TIMEOUT,
        DUMPSYS_STDOUT_LIMIT,
    )
    .map_err(|err| format!("Failed to query foreground app for {serial}: {err}"))?;
    if window_output.success {
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
    let output = adb_shell(adb_path, serial, &["am", "force-stop", package]).map_err(|err| {
        format!("Failed to run `{adb_path} -s {serial} shell am force-stop {package}`: {err}")
    })?;

    let combined = combined_output(&output);
    if output.success {
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
    // Single `pm clear`, no run-as fallback: `run-as <pkg> pm clear <pkg>`
    // executes as the app's own uid, which cannot grant the shell the
    // CLEAR_APP_USER_DATA permission — the old retry only restated the
    // failure with a misleading "permission escalation" framing (PRD §9).
    // run-as remains valuable for file access on debuggable apps (Files
    // page), which is what it actually supports.
    let output = adb_shell(adb_path, serial, &["pm", "clear", package]).map_err(|err| {
        format!("Failed to run `{adb_path} -s {serial} shell pm clear {package}`: {err}")
    })?;

    if package_command_succeeded(&output) {
        return Ok(package_command_success_message(
            &output,
            format!("Cleared data for {package}."),
        ));
    }

    Err(format!(
        "Failed to clear data for {package} on {serial}: {}",
        combined_output(&output).if_empty("unknown error")
    ))
}

pub fn uninstall_package(adb_path: &str, serial: &str, package: &str) -> Result<String, String> {
    let output = adb_shell(adb_path, serial, &["pm", "uninstall", package]).map_err(|err| {
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

pub fn connect_device(adb_path: &str, target: &str) -> Result<String, crate::wireless::Failure> {
    let target = target.trim();
    if target.is_empty() {
        return Err(crate::wireless::Failure::new("connect.error.empty", ""));
    }

    let output = AdbExecutor::new(adb_path)
        .execute_with_timeout(&["connect", target], CONNECT_TIMEOUT)
        .map_err(crate::wireless::Failure::from_adb_error)?;

    parse_connect_output(target, &ShellOutcome::from(output))
        .map_err(|err| crate::wireless::Failure::new("connect.error.connection", err))
}

pub fn disconnect_device(adb_path: &str, target: &str) -> Result<String, String> {
    let target = target.trim();
    if target.is_empty() {
        return Err("Device endpoint cannot be empty".to_owned());
    }

    let output = AdbExecutor::new(adb_path)
        .execute_with_timeout(&["disconnect", target], DISCONNECT_TIMEOUT)
        .map_err(|err| format!("Failed to run `{adb_path} disconnect {target}`: {err}"))?;

    parse_disconnect_output(target, &ShellOutcome::from(output))
}

fn run_devices(adb_path: &str) -> Result<AdbOutput, String> {
    AdbExecutor::new(adb_path)
        .execute_with_timeout(&["devices"], DEVICES_TIMEOUT)
        .map_err(|err| format!("Failed to run `{adb_path} devices`: {err}"))
}

/// Client-side errors that mean "the daemon was mid-(re)start", not "adb is
/// broken": the smart-socket handshake reset, a version check against a
/// just-spawned daemon, or a connect attempt while the old one is dying.
fn is_transient_adb_failure(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    [
        "protocol fault",
        "failed to check server version",
        "cannot connect to daemon",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

pub fn restart_server(adb_path: &str) -> Result<String, String> {
    let kill_output = AdbExecutor::new(adb_path)
        .execute_with_timeout(&["kill-server"], SERVER_RESTART_TIMEOUT)
        .map_err(|err| format!("Failed to run `{adb_path} kill-server`: {err}"))?;
    let kill_output = ShellOutcome::from(kill_output);
    if !kill_output.success {
        return Err(format!(
            "Failed to stop the ADB server: {}",
            combined_output(&kill_output).if_empty("unknown error")
        ));
    }

    let start_output = AdbExecutor::new(adb_path)
        .execute_with_timeout(&["start-server"], SERVER_RESTART_TIMEOUT)
        .map_err(|err| format!("Failed to run `{adb_path} start-server`: {err}"))?;
    let start_output = ShellOutcome::from(start_output);
    if !start_output.success {
        return Err(format!(
            "Failed to start the ADB server: {}",
            combined_output(&start_output).if_empty("unknown error")
        ));
    }

    // Wait briefly until the fresh daemon answers `adb devices` cleanly, so
    // the refresh that follows a "restarted" message does not hit the
    // first-query fault or a pre-enumeration empty list.
    wait_until_devices_respond(adb_path);

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

/// Best-effort readiness check after `start-server`: probe `adb devices`
/// until it exits cleanly (or the probes run out) — a fresh daemon commonly
/// faults its first query and succeeds on the second.
fn wait_until_devices_respond(adb_path: &str) {
    for attempt in 0..SERVER_RESTART_PROBES {
        let responded = AdbExecutor::new(adb_path)
            .execute_with_timeout(&["devices"], DEVICES_TIMEOUT)
            .map(|output| output.success())
            .unwrap_or(false);
        if responded || attempt + 1 == SERVER_RESTART_PROBES {
            return;
        }
        thread::sleep(SERVER_RESTART_PROBE_DELAY);
    }
}

/// adb 按文件名后缀校验安装包，`xxx.apk.1` 这类浏览器重复下载的文件需要先
/// 暂存为 `.apk` 结尾。优先硬链接（零拷贝），跨卷或不支持时回退复制。
/// 调用方持有返回的 TempDir 直至安装结束，删除即清理。
fn stage_apk_for_install(apk_path: &Path) -> Result<Option<(tempfile::TempDir, PathBuf)>, String> {
    let needs_staging = apk_path
        .file_name()
        .and_then(|name| name.to_str())
        .is_none_or(|name| {
            let lower = name.to_ascii_lowercase();
            !lower.ends_with(".apk") && !lower.ends_with(".apex")
        });
    if !needs_staging {
        return Ok(None);
    }
    let dir = tempfile::tempdir().map_err(|err| {
        format!(
            "Failed to create a staging directory for {}: {err}",
            apk_path.display()
        )
    })?;
    let staged_path = dir.path().join("install.apk");
    if std::fs::hard_link(apk_path, &staged_path).is_err()
        && let Err(err) = std::fs::copy(apk_path, &staged_path)
    {
        return Err(format!(
            "Failed to stage {} as an .apk file: {err}",
            apk_path.display()
        ));
    }
    Ok(Some((dir, staged_path)))
}

pub fn install_apk(adb_path: &str, serial: &str, apk_path: &Path) -> Result<String, String> {
    let staged = stage_apk_for_install(apk_path)?;
    let install_source = staged
        .as_ref()
        .map(|(_dir, path)| path.as_path())
        .unwrap_or(apk_path);
    let output = AdbExecutor::new(adb_path)
        .execute_with_timeout(
            &[
                "-s",
                serial,
                "install",
                "-r",
                &install_source.to_string_lossy(),
            ],
            INSTALL_TIMEOUT,
        )
        .map_err(|err| {
            format!(
                "Failed to run `{adb_path} -s {serial} install -r {}`: {err}",
                apk_path.display()
            )
        })?;
    let output = ShellOutcome::from(output);

    let combined = combined_output(&output);
    if output.success {
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

    let mut args: Vec<&str> = vec!["-s", serial, "logcat"];
    args.extend(extra_args.iter().map(String::as_str));
    AdbExecutor::new(adb_path)
        .spawn_streaming(&args, Stdio::from(stdout_file), Stdio::from(stderr_file))
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

fn adb_shell(adb_path: &str, serial: &str, args: &[&str]) -> Result<ShellOutcome, AdbError> {
    adb_shell_with_limit(adb_path, serial, args, SHELL_TIMEOUT, SHELL_STDOUT_LIMIT)
}

fn adb_shell_with_limit(
    adb_path: &str,
    serial: &str,
    args: &[&str],
    timeout: Duration,
    stdout_limit: usize,
) -> Result<ShellOutcome, AdbError> {
    let mut command = vec!["-s", serial, "shell"];
    command.extend_from_slice(args);
    let output = AdbExecutor::new(adb_path).execute_with_options(
        &command,
        ExecOptions {
            timeout: Some(timeout),
            stdout_limit: Some(stdout_limit),
            ..Default::default()
        },
    )?;
    Ok(ShellOutcome::from(output))
}

pub fn is_network_device_serial(serial: &str) -> bool {
    let serial = serial.trim().trim_end_matches('.');
    // ADB auto-connect may expose a service instance instead of IP:port.
    // Keep these transports behind USB in device merging and allow disconnect.
    if ["._adb-tls-connect._tcp", "._adb._tcp"]
        .iter()
        .any(|suffix| {
            serial
                .strip_suffix(suffix)
                .is_some_and(|instance| !instance.is_empty())
        })
    {
        return true;
    }
    let Some((host, port)) = serial.rsplit_once(':') else {
        return false;
    };

    !host.is_empty()
        && !host.starts_with("emulator-")
        && !port.is_empty()
        && port.chars().all(|ch| ch.is_ascii_digit())
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

#[derive(Clone, Default, Debug)]
pub struct DeviceMetadata {
    pub(crate) identity_key: Option<String>,
    pub(crate) android_version: Option<String>,
    pub(crate) manufacturer: Option<String>,
    pub(crate) model: Option<String>,
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

    release_or_codename
        .as_deref()
        .or(release.as_deref())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(format_android_version)
}

fn adb_shell_getprop(adb_path: &str, serial: &str, key: &str) -> Option<String> {
    let output = AdbExecutor::new(adb_path)
        .execute_with_timeout(&["-s", serial, "shell", "getprop", key], SHELL_TIMEOUT)
        .ok()?;

    if !output.success() {
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

fn combined_output(output: &ShellOutcome) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    [stdout.trim(), stderr.trim()]
        .into_iter()
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn package_command_succeeded(output: &ShellOutcome) -> bool {
    let combined = combined_output(output);
    let lower = combined.to_ascii_lowercase();
    output.success && (combined.is_empty() || lower.contains("success"))
}

fn package_command_success_message(output: &ShellOutcome, fallback: String) -> String {
    let combined = combined_output(output);
    if combined.is_empty() {
        fallback
    } else {
        combined
    }
}

fn parse_connect_output(target: &str, output: &ShellOutcome) -> Result<String, String> {
    let combined = combined_output(output);
    let lower = combined.to_ascii_lowercase();

    if output.success && (lower.contains("connected to") || lower.contains("already connected to"))
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

fn parse_disconnect_output(target: &str, output: &ShellOutcome) -> Result<String, String> {
    let combined = combined_output(output);
    let lower = combined.to_ascii_lowercase();

    if output.success
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
        DeviceMetadataCache, ShellOutcome, combined_output, decode_screenshot_png,
        format_android_version, is_network_device_serial, is_transient_adb_failure,
        package_command_succeeded, parse_component_token, parse_connect_output,
        parse_devices_output, parse_disconnect_output, parse_foreground_app_from_activity_dump,
        parse_foreground_app_from_window_dump, parse_installed_packages, parse_logcat_args,
    };
    use std::{io::Cursor, path::PathBuf};

    #[test]
    fn transient_adb_failures_are_recognized_by_marker() {
        for stderr in [
            "adb.exe: failed to check server version: protocol fault (couldn't read status): connection reset",
            "adb.exe: protocol fault (couldn't read status)",
            "cannot connect to daemon at tcp:5037: cannot connect to 127.0.0.1:5037",
        ] {
            assert!(
                is_transient_adb_failure(stderr),
                "should be transient: {stderr}"
            );
        }
        for stderr in [
            "adb: unrecognized arguments",
            "adb.exe: device or emulator not found",
            "",
        ] {
            assert!(
                !is_transient_adb_failure(stderr),
                "should not be transient: {stderr}"
            );
        }
    }

    #[test]
    fn metadata_cache_suppresses_getprop_storm_on_polls() {
        let fixture = crate::wireless::tests::Fixture::new("display_devices=true");

        // First discovery queries static metadata for both devices.
        let first =
            super::list_devices(&fixture.adb, DeviceMetadataCache::default(), false).unwrap();
        assert_eq!(first.devices.len(), 2);
        assert!(first.devices[0].model.is_some());
        let first_log = std::fs::read_to_string(fixture.dir.path().join("calls.log")).unwrap();
        let getprops_after_first = first_log.lines().filter(|l| l.contains("getprop")).count();
        assert!(getprops_after_first > 0);
        assert_eq!(first.metadata_cache.len(), 2);

        // A cached poll re-queries nothing (PRD §17).
        let second = super::list_devices(&fixture.adb, first.metadata_cache, false).unwrap();
        let second_log = std::fs::read_to_string(fixture.dir.path().join("calls.log")).unwrap();
        let getprops_after_second = second_log.lines().filter(|l| l.contains("getprop")).count();
        assert_eq!(getprops_after_second, getprops_after_first);
        // Metadata still comes back identical from the cache.
        assert_eq!(second.devices[0].model, first.devices[0].model);
        assert_eq!(second.devices[1].model, first.devices[1].model);

        // Manual refresh forces a re-read (PRD §17).
        let third = super::list_devices(&fixture.adb, second.metadata_cache, true).unwrap();
        let third_log = std::fs::read_to_string(fixture.dir.path().join("calls.log")).unwrap();
        let getprops_after_third = third_log.lines().filter(|l| l.contains("getprop")).count();
        assert!(getprops_after_third > getprops_after_second);
        drop(third);
    }

    #[test]
    fn metadata_cache_reconnect_triggers_refresh() {
        let fixture = crate::wireless::tests::Fixture::new("display_devices=true");
        let first =
            super::list_devices(&fixture.adb, DeviceMetadataCache::default(), false).unwrap();
        let log_before = std::fs::read_to_string(fixture.dir.path().join("calls.log")).unwrap();
        let getprops_before = log_before.lines().filter(|l| l.contains("getprop")).count();

        // Device B unplugs: pruning drops its entry, so a later reappearance
        // re-queries as a fresh discovery (PRD §17 reconnect rule).
        let mut cache = first.metadata_cache;
        cache.retain_serials(&["FIXTURE_USB_A".to_owned()]);
        assert_eq!(cache.len(), 1);

        let second = super::list_devices(&fixture.adb, cache, false).unwrap();
        let log_after = std::fs::read_to_string(fixture.dir.path().join("calls.log")).unwrap();
        let getprops_after = log_after.lines().filter(|l| l.contains("getprop")).count();
        assert!(getprops_after > getprops_before);
        assert!(
            second
                .devices
                .iter()
                .any(|d| d.serial == "FIXTURE_USB_B" && d.model.is_some())
        );
    }

    #[test]
    fn install_apk_stages_files_without_apk_suffix() {
        let fixture = crate::wireless::tests::Fixture::new("display_devices=true");
        let dir = tempfile::tempdir().unwrap();
        let redownload = dir.path().join("demo.apk.1");
        std::fs::write(&redownload, b"apk-bytes").unwrap();

        super::install_apk(&fixture.adb, "FIXTURE_USB_A", &redownload).unwrap();
        let log = std::fs::read_to_string(fixture.dir.path().join("calls.log")).unwrap();
        let install_line = log
            .lines()
            .find(|line| line.contains("install -r"))
            .expect("install call recorded");
        assert!(install_line.ends_with("install.apk"));
        assert!(!install_line.ends_with("demo.apk.1"));

        let plain = dir.path().join("plain.apk");
        std::fs::write(&plain, b"apk-bytes").unwrap();
        super::install_apk(&fixture.adb, "FIXTURE_USB_A", &plain).unwrap();
        let log = std::fs::read_to_string(fixture.dir.path().join("calls.log")).unwrap();
        assert!(log.contains(&format!("install -r {}", plain.display())));
    }

    #[test]
    fn stage_apk_for_install_rejects_missing_files() {
        let missing = PathBuf::from("definitely-missing.apk.1");
        assert!(super::stage_apk_for_install(&missing).is_err());
    }

    #[test]
    fn decode_screenshot_png_converts_pixels_to_rgba() {
        let source =
            image::RgbaImage::from_raw(2, 1, vec![1, 2, 3, 4, 5, 6, 7, 8]).expect("source image");
        let mut png = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(source)
            .write_to(&mut png, image::ImageFormat::Png)
            .expect("encode png");

        let screenshot = decode_screenshot_png(&png.into_inner()).expect("decode screenshot");
        assert_eq!((screenshot.width, screenshot.height), (2, 1));
        assert_eq!(screenshot.rgba_pixels, vec![1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn decode_screenshot_png_rejects_invalid_data() {
        assert!(decode_screenshot_png(b"not a png").is_err());
    }

    #[test]
    fn parse_installed_packages_sorts_deduplicates_and_skips_invalid_lines() {
        let packages = parse_installed_packages(
            "package:com.example.z\nwarning: ignored\npackage:com.example.a\npackage:com.example.z\npackage: \n",
        );
        assert_eq!(packages, vec!["com.example.a", "com.example.z"]);
    }

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
        let output = ShellOutcome {
            success: true,
            stdout: b"connected to 192.168.0.8:5555".to_vec(),
            stderr: Vec::new(),
        };

        let message = parse_connect_output("192.168.0.8:5555", &output).expect("connect success");
        assert!(message.contains("connected to 192.168.0.8:5555"));
    }

    #[test]
    fn parse_connect_output_rejects_failed_message() {
        let output = ShellOutcome {
            success: false,
            stdout: b"".to_vec(),
            stderr: b"failed to connect".to_vec(),
        };

        let error = parse_connect_output("192.168.0.8:5555", &output).expect_err("connect error");
        assert!(error.contains("failed to connect"));
    }

    #[test]
    fn parse_disconnect_output_accepts_success_and_missing_device() {
        let success = ShellOutcome {
            success: true,
            stdout: b"disconnected 192.168.0.8:5555".to_vec(),
            stderr: Vec::new(),
        };
        let missing = ShellOutcome {
            success: true,
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
        assert!(is_network_device_serial(
            "adb-serial-random._adb-tls-connect._tcp"
        ));
        assert!(is_network_device_serial("adb-serial._adb._tcp."));
        assert!(!is_network_device_serial(
            "adb-serial._adb-tls-pairing._tcp"
        ));
        assert!(!is_network_device_serial("localhost:"));
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
    fn clear_data_reports_permission_failure_directly() {
        // The run-as fallback is gone (PRD §9): a SecurityException from
        // `pm clear` surfaces verbatim instead of being retried under a
        // false "permission escalation" assumption.
        let output = ShellOutcome {
            success: false,
            stdout: Vec::new(),
            stderr: b"Exception occurred while executing 'clear':\njava.lang.SecurityException: PID 16791 does not have permission android.permission.CLEAR_APP_USER_DATA to clear data of package com.example.app".to_vec(),
        };

        assert!(!package_command_succeeded(&output));
        assert!(combined_output(&output).contains("CLEAR_APP_USER_DATA"));
    }

    #[test]
    fn package_command_succeeded_accepts_empty_or_success_output() {
        let empty_success = ShellOutcome {
            success: true,
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        let explicit_success = ShellOutcome {
            success: true,
            stdout: b"Success".to_vec(),
            stderr: Vec::new(),
        };
        let failed = ShellOutcome {
            success: false,
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
}
