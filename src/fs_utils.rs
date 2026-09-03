use chrono::Local;
#[cfg(target_os = "windows")]
use std::process::Command;
use std::{
    collections::HashSet,
    fs,
    path::{Component, Path, PathBuf},
    time::SystemTime,
};

/// A recursive size breakdown of the configured device-log directory. Each
/// direct child directory is treated as one device log bucket.
#[derive(Clone, Debug, Default)]
pub struct LogStorageReport {
    pub total_bytes: u64,
    pub log_bytes: u64,
    pub log_file_count: usize,
    pub other_file_bytes: u64,
    pub device_directories: Vec<DeviceLogUsage>,
}

#[derive(Clone, Debug, Default)]
pub struct DeviceLogUsage {
    /// Empty when the files live directly in the configured log directory.
    pub directory_name: String,
    pub total_bytes: u64,
    pub log_bytes: u64,
    pub log_file_count: usize,
    pub oldest_log_modified: Option<SystemTime>,
    pub newest_log_modified: Option<SystemTime>,
}

/// Selection used by both the cleanup preview and the actual cleanup. `None`
/// means every top-level directory; `Some([])` intentionally selects none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CleanupFilter {
    pub device_directories: Option<Vec<String>>,
    /// Only files modified strictly before this instant match.
    pub older_than: Option<SystemTime>,
}

#[derive(Clone, Debug, Default)]
pub struct CleanupPreview {
    pub matching_files: usize,
    pub matching_bytes: u64,
    pub protected_files: usize,
    pub protected_bytes: u64,
}

#[derive(Clone, Debug, Default)]
pub struct CleanupOutcome {
    pub deleted_files: usize,
    pub freed_bytes: u64,
    pub failed_paths: Vec<String>,
}

pub fn device_log_dir(base_dir: &Path, serial: &str, alias: Option<&str>) -> PathBuf {
    let directory_name = alias
        .map(str::trim)
        .filter(|alias| !alias.is_empty())
        .map(sanitize_serial)
        .unwrap_or_else(|| sanitize_serial(serial));

    base_dir.join(directory_name)
}

pub fn session_log_path(base_dir: &Path, serial: &str, alias: Option<&str>) -> PathBuf {
    let file_stem = alias
        .map(str::trim)
        .filter(|alias| !alias.is_empty())
        .map(sanitize_serial)
        .unwrap_or_else(|| sanitize_serial(serial));
    let file_name = format!("{}-{}.log", file_stem, Local::now().format("%Y%m%d-%H%M%S"));
    device_log_dir(base_dir, serial, alias).join(file_name)
}

pub fn sanitize_serial(serial: &str) -> String {
    desktop_fs::sanitize_path_component(serial)
}

pub fn file_size(path: &Path) -> Result<u64, String> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(metadata.len()),
        Ok(_) => Ok(0),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(err) => Err(format!(
            "Failed to read metadata for {}: {err}",
            path.display()
        )),
    }
}

pub fn scan_log_storage(base_dir: &Path) -> Result<LogStorageReport, String> {
    if !base_dir.exists() {
        return Ok(LogStorageReport::default());
    }

    let mut report = LogStorageReport::default();
    let mut root_usage = DeviceLogUsage::default();
    for entry in fs::read_dir(base_dir)
        .map_err(|err| format!("Failed to read directory {}: {err}", base_dir.display()))?
    {
        let entry = entry.map_err(|err| format!("Failed to read directory entry: {err}"))?;
        let metadata = entry.metadata().map_err(|err| {
            format!(
                "Failed to read metadata for {}: {err}",
                entry.path().display()
            )
        })?;
        if !metadata.is_dir() {
            accumulate_log_usage(&entry.path(), &mut root_usage)?;
            continue;
        }
        let directory_name = entry.file_name().to_string_lossy().into_owned();
        let mut usage = DeviceLogUsage {
            directory_name,
            ..Default::default()
        };
        accumulate_log_usage(&entry.path(), &mut usage)?;
        if usage.total_bytes > 0 || usage.log_file_count > 0 {
            report.total_bytes += usage.total_bytes;
            report.log_bytes += usage.log_bytes;
            report.log_file_count += usage.log_file_count;
            report.device_directories.push(usage);
        }
    }

    if root_usage.total_bytes > 0 || root_usage.log_file_count > 0 {
        report.total_bytes += root_usage.total_bytes;
        report.log_bytes += root_usage.log_bytes;
        report.log_file_count += root_usage.log_file_count;
        report.device_directories.push(root_usage);
    }

    report.other_file_bytes = report.total_bytes.saturating_sub(report.log_bytes);
    report.device_directories.sort_by(|left, right| {
        right
            .total_bytes
            .cmp(&left.total_bytes)
            .then_with(|| left.directory_name.cmp(&right.directory_name))
    });
    Ok(report)
}

pub fn preview_log_cleanup(
    base_dir: &Path,
    filter: &CleanupFilter,
    protected_paths: &[PathBuf],
) -> Result<CleanupPreview, String> {
    let (_, preview) = collect_cleanup_candidates(base_dir, filter, protected_paths)?;
    Ok(preview)
}

pub fn cleanup_matching_logs(
    base_dir: &Path,
    filter: &CleanupFilter,
    protected_paths: &[PathBuf],
) -> Result<CleanupOutcome, String> {
    let (candidates, _) = collect_cleanup_candidates(base_dir, filter, protected_paths)?;
    let mut outcome = CleanupOutcome::default();

    for candidate in candidates {
        match fs::remove_file(&candidate.path) {
            Ok(()) => {
                outcome.deleted_files += 1;
                outcome.freed_bytes += candidate.bytes;
            }
            Err(err) => outcome
                .failed_paths
                .push(format!("{}: {err}", candidate.path.display())),
        }
    }

    if base_dir.exists() {
        for entry in fs::read_dir(base_dir)
            .map_err(|err| format!("Failed to read directory {}: {err}", base_dir.display()))?
        {
            let entry = entry.map_err(|err| format!("Failed to read directory entry: {err}"))?;
            if entry
                .metadata()
                .map_err(|err| {
                    format!(
                        "Failed to read metadata for {}: {err}",
                        entry.path().display()
                    )
                })?
                .is_dir()
            {
                // Empty-folder cleanup is cosmetic. A locked folder must not
                // turn an otherwise successful file cleanup into a failure.
                let _ = remove_empty_dirs(&entry.path());
            }
        }
    }

    Ok(outcome)
}

pub fn format_bytes(bytes: u64) -> String {
    desktop_fs::format_bytes(bytes)
}

pub fn display_path(path: &Path) -> String {
    desktop_fs::display_path(path)
}

pub fn display_path_string(path: &str) -> String {
    desktop_fs::display_path_string(path)
}

pub fn normalize_display_path(path: &Path) -> PathBuf {
    desktop_fs::normalize_display_path(path)
}

fn accumulate_log_usage(path: &Path, usage: &mut DeviceLogUsage) -> Result<(), String> {
    let metadata = fs::metadata(path)
        .map_err(|err| format!("Failed to read metadata for {}: {err}", path.display()))?;
    if metadata.is_dir() {
        for entry in fs::read_dir(path)
            .map_err(|err| format!("Failed to read directory {}: {err}", path.display()))?
        {
            let entry = entry.map_err(|err| format!("Failed to read directory entry: {err}"))?;
            accumulate_log_usage(&entry.path(), usage)?;
        }
        return Ok(());
    }

    if !metadata.is_file() {
        return Ok(());
    }

    usage.total_bytes += metadata.len();
    if !is_log_file(path) {
        return Ok(());
    }

    usage.log_bytes += metadata.len();
    usage.log_file_count += 1;
    let modified = metadata
        .modified()
        .map_err(|err| format!("Failed to read modified time for {}: {err}", path.display()))?;
    usage.oldest_log_modified = Some(
        usage
            .oldest_log_modified
            .map(|current| current.min(modified))
            .unwrap_or(modified),
    );
    usage.newest_log_modified = Some(
        usage
            .newest_log_modified
            .map(|current| current.max(modified))
            .unwrap_or(modified),
    );
    Ok(())
}

#[derive(Clone, Debug)]
struct CleanupCandidate {
    path: PathBuf,
    bytes: u64,
}

fn collect_cleanup_candidates(
    base_dir: &Path,
    filter: &CleanupFilter,
    protected_paths: &[PathBuf],
) -> Result<(Vec<CleanupCandidate>, CleanupPreview), String> {
    if !base_dir.exists() {
        return Ok((Vec::new(), CleanupPreview::default()));
    }

    let protected: HashSet<PathBuf> = protected_paths
        .iter()
        .map(|path| normalize_path(path))
        .collect();
    let selected_directories = filter
        .device_directories
        .as_ref()
        .map(|directories| directories.iter().collect::<HashSet<_>>());
    let mut candidates = Vec::new();
    let mut preview = CleanupPreview::default();
    collect_cleanup_candidates_recursive(
        base_dir,
        base_dir,
        filter,
        selected_directories.as_ref(),
        &protected,
        &mut candidates,
        &mut preview,
    )?;
    Ok((candidates, preview))
}

fn collect_cleanup_candidates_recursive(
    base_dir: &Path,
    path: &Path,
    filter: &CleanupFilter,
    selected_directories: Option<&HashSet<&String>>,
    protected: &HashSet<PathBuf>,
    candidates: &mut Vec<CleanupCandidate>,
    preview: &mut CleanupPreview,
) -> Result<(), String> {
    for entry in fs::read_dir(path)
        .map_err(|err| format!("Failed to read directory {}: {err}", path.display()))?
    {
        let entry = entry.map_err(|err| format!("Failed to read directory entry: {err}"))?;
        let entry_path = entry.path();
        let metadata = entry.metadata().map_err(|err| {
            format!(
                "Failed to read metadata for {}: {err}",
                entry_path.display()
            )
        })?;

        if metadata.is_dir() {
            collect_cleanup_candidates_recursive(
                base_dir,
                &entry_path,
                filter,
                selected_directories,
                protected,
                candidates,
                preview,
            )?;
            continue;
        }
        if !metadata.is_file() || !is_log_file(&entry_path) {
            continue;
        }

        let device_directory = top_level_directory_name(base_dir, &entry_path);
        if selected_directories.is_some_and(|directories| !directories.contains(&device_directory))
        {
            continue;
        }
        let modified = metadata.modified().map_err(|err| {
            format!(
                "Failed to read modified time for {}: {err}",
                entry_path.display()
            )
        })?;
        if filter.older_than.is_some_and(|cutoff| modified >= cutoff) {
            continue;
        }

        if protected.contains(&normalize_path(&entry_path)) {
            preview.protected_files += 1;
            preview.protected_bytes += metadata.len();
            continue;
        }

        preview.matching_files += 1;
        preview.matching_bytes += metadata.len();
        candidates.push(CleanupCandidate {
            path: entry_path,
            bytes: metadata.len(),
        });
    }
    Ok(())
}

fn top_level_directory_name(base_dir: &Path, path: &Path) -> String {
    path.strip_prefix(base_dir)
        .ok()
        .and_then(|relative| {
            relative
                .components()
                .next()
                .and_then(|component| match component {
                    Component::Normal(name) if relative.components().count() > 1 => {
                        Some(name.to_string_lossy().into_owned())
                    }
                    _ => None,
                })
        })
        .unwrap_or_default()
}

fn is_log_file(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("log"))
}

fn remove_empty_dirs(path: &Path) -> Result<bool, String> {
    let mut is_empty = true;

    for entry in fs::read_dir(path)
        .map_err(|err| format!("Failed to read directory {}: {err}", path.display()))?
    {
        let entry = entry.map_err(|err| format!("Failed to read directory entry: {err}"))?;
        let child_path = entry.path();
        let metadata = entry.metadata().map_err(|err| {
            format!(
                "Failed to read metadata for {}: {err}",
                child_path.display()
            )
        })?;

        if metadata.is_dir() {
            if !remove_empty_dirs(&child_path)? {
                is_empty = false;
            }
        } else {
            is_empty = false;
        }
    }

    if is_empty {
        match fs::remove_dir(path) {
            Ok(_) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => Ok(false),
            Err(err) => Err(format!(
                "Failed to remove empty directory {}: {err}",
                path.display()
            )),
        }
    } else {
        Ok(false)
    }
}

fn normalize_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

pub fn open_path(path: &Path) -> Result<(), String> {
    desktop_fs::open_path(path)
}

pub fn rename_device_log_dir(
    base_dir: &Path,
    serial: &str,
    previous_alias: Option<&str>,
    new_alias: Option<&str>,
) -> Result<Option<PathBuf>, String> {
    let current_dir = device_log_dir(base_dir, serial, previous_alias);
    let next_dir = device_log_dir(base_dir, serial, new_alias);

    if current_dir == next_dir {
        return Ok(Some(next_dir));
    }

    if !current_dir.exists() {
        return Ok(None);
    }

    if next_dir.exists() {
        return Err(format!(
            "Cannot rename device log directory to {} because it already exists.",
            next_dir.display()
        ));
    }

    fs::rename(&current_dir, &next_dir).map_err(|err| {
        format!(
            "Failed to rename device log directory from {} to {}: {err}",
            current_dir.display(),
            next_dir.display()
        )
    })?;

    Ok(Some(next_dir))
}

pub fn open_url(url: &str) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;

        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map_err(|err| format!("Failed to open URL {url}: {err}"))?;
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(url)
            .spawn()
            .map_err(|err| format!("Failed to open URL {url}: {err}"))?;
        return Ok(());
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        std::process::Command::new("xdg-open")
            .arg(url)
            .spawn()
            .map_err(|err| format!("Failed to open URL {url}: {err}"))?;
        Ok(())
    }
}

pub fn open_device_shell(adb_path: &str, serial: &str) -> Result<String, String> {
    #[cfg(target_os = "windows")]
    {
        let terminal_args = build_windows_terminal_shell_args(adb_path, serial);
        match Command::new("wt").args(&terminal_args).spawn() {
            Ok(_) => {
                return Ok(format!(
                    "Opened a device shell for {serial} in Windows Terminal."
                ));
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(format!(
                    "Failed to open Windows Terminal for {serial}: {err}"
                ));
            }
        }

        let powershell_args = build_powershell_shell_args(adb_path, serial);
        Command::new("powershell")
            .args(&powershell_args)
            .spawn()
            .map_err(|err| format!("Failed to open PowerShell for {serial}: {err}"))?;
        return Ok(format!("Opened a device shell for {serial} in PowerShell."));
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = adb_path;
        let _ = serial;
        Err("Opening a device shell is currently supported on Windows only.".to_owned())
    }
}

#[cfg(any(test, target_os = "windows"))]
fn build_windows_terminal_shell_args(adb_path: &str, serial: &str) -> Vec<String> {
    let mut args = vec!["new-tab".to_owned(), "powershell".to_owned()];
    args.extend(build_powershell_shell_args(adb_path, serial));
    args
}

#[cfg(any(test, target_os = "windows"))]
fn build_powershell_shell_args(adb_path: &str, serial: &str) -> Vec<String> {
    vec![
        "-NoExit".to_owned(),
        "-Command".to_owned(),
        build_powershell_shell_command(adb_path, serial),
    ]
}

#[cfg(any(test, target_os = "windows"))]
fn build_powershell_shell_command(adb_path: &str, serial: &str) -> String {
    format!(
        "& {} -s {} shell",
        quote_powershell_literal(adb_path),
        quote_powershell_literal(serial)
    )
}

#[cfg(any(test, target_os = "windows"))]
fn quote_powershell_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::{
        CleanupFilter, build_powershell_shell_args, build_windows_terminal_shell_args,
        cleanup_matching_logs, device_log_dir, display_path_string, format_bytes,
        preview_log_cleanup, rename_device_log_dir, sanitize_serial, scan_log_storage,
        session_log_path,
    };
    use std::{
        fs,
        path::{Path, PathBuf},
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn sanitize_serial_replaces_path_unsafe_characters() {
        assert_eq!(sanitize_serial("emulator:5554/usb"), "emulator_5554_usb");
    }

    #[test]
    fn session_log_path_keeps_device_subdirectory_and_log_extension() {
        let path = session_log_path(Path::new("/tmp/logs"), "serial:1", Some("Pixel 8"));
        assert_eq!(path.parent(), Some(Path::new("/tmp/logs/Pixel_8")));
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        assert!(file_name.starts_with("Pixel_8-"));
        assert!(file_name.ends_with(".log"));
    }

    #[test]
    fn session_log_path_preserves_unicode_alias_in_path_components() {
        let path = session_log_path(Path::new("/tmp/logs"), "serial:1", Some("小米 14"));
        assert_eq!(path.parent(), Some(Path::new("/tmp/logs/小米_14")));
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        assert!(file_name.starts_with("小米_14-"));
        assert!(file_name.ends_with(".log"));
    }

    #[test]
    fn format_bytes_uses_readable_units() {
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.00 KB");
    }

    #[test]
    fn display_path_string_strips_windows_verbatim_prefix() {
        let raw = r"\\?\F:\logs\demo";
        let cleaned = display_path_string(raw);
        if cfg!(target_os = "windows") {
            assert_eq!(cleaned, r"F:\logs\demo");
        } else {
            assert_eq!(cleaned, raw);
        }
    }

    #[test]
    fn device_log_dir_prefers_alias_directory_name() {
        let path = device_log_dir(Path::new("/tmp/logs"), "serial:1", Some("Pixel 8"));
        assert_eq!(path, Path::new("/tmp/logs").join("Pixel_8"));
    }

    #[test]
    fn rename_device_log_dir_moves_existing_directory() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("valid system time")
            .as_nanos();
        let temp_dir = std::env::temp_dir().join(format!("logcatx-fs-{unique}"));
        let source = device_log_dir(&temp_dir, "serial:1", None);
        let target = device_log_dir(&temp_dir, "serial:1", Some("Pixel 8"));

        fs::create_dir_all(&source).expect("create source directory");
        fs::write(source.join("serial_1-test.log"), b"demo").expect("write log file");

        let renamed = rename_device_log_dir(&temp_dir, "serial:1", None, Some("Pixel 8"))
            .expect("rename log dir");

        assert_eq!(renamed, Some(target.clone()));
        assert!(target.exists());
        assert!(!source.exists());

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn powershell_shell_args_quote_adb_path_and_serial() {
        let args = build_powershell_shell_args(r"C:\Tools\adb.exe", "192.168.0.8:5555");
        assert_eq!(args[0], "-NoExit");
        assert_eq!(args[1], "-Command");
        assert!(args[2].contains("'C:\\Tools\\adb.exe'"));
        assert!(args[2].contains("'192.168.0.8:5555'"));
    }

    #[test]
    fn windows_terminal_shell_args_start_with_new_tab() {
        let args = build_windows_terminal_shell_args("adb", "serial-1");
        assert_eq!(args[0], "new-tab");
        assert_eq!(args[1], "powershell");
        assert_eq!(args[2], "-NoExit");
    }

    #[test]
    fn scan_log_storage_groups_files_by_top_level_directory() {
        let base_dir = unique_temp_dir("storage-report");
        fs::create_dir_all(base_dir.join("Pixel_8")).expect("create Pixel directory");
        fs::create_dir_all(base_dir.join("Tablet").join("nested"))
            .expect("create Tablet directory");
        fs::write(base_dir.join("Pixel_8").join("first.log"), b"abc").expect("write first log");
        fs::write(base_dir.join("Pixel_8").join("notes.txt"), b"12").expect("write notes");
        fs::write(
            base_dir.join("Tablet").join("nested").join("second.log"),
            b"12345",
        )
        .expect("write second log");
        fs::write(base_dir.join("root.log"), b"12").expect("write root log");

        let report = scan_log_storage(&base_dir).expect("scan storage");

        assert_eq!(report.total_bytes, 12);
        assert_eq!(report.log_bytes, 10);
        assert_eq!(report.other_file_bytes, 2);
        assert_eq!(report.log_file_count, 3);
        assert_eq!(
            report
                .device_directories
                .iter()
                .find(|usage| usage.directory_name == "Pixel_8")
                .expect("Pixel usage")
                .total_bytes,
            5
        );
        assert_eq!(
            report
                .device_directories
                .iter()
                .find(|usage| usage.directory_name.is_empty())
                .expect("root usage")
                .log_file_count,
            1
        );

        let _ = fs::remove_dir_all(&base_dir);
    }

    #[test]
    fn cleanup_preview_and_execution_honor_device_time_and_protected_filters() {
        let base_dir = unique_temp_dir("cleanup-filter");
        let pixel_log = base_dir.join("Pixel_8").join("pixel.log");
        let tablet_log = base_dir.join("Tablet").join("tablet.log");
        fs::create_dir_all(pixel_log.parent().expect("Pixel parent")).expect("create Pixel");
        fs::create_dir_all(tablet_log.parent().expect("Tablet parent")).expect("create Tablet");
        fs::write(&pixel_log, b"pixel").expect("write Pixel log");
        fs::write(&tablet_log, b"tablet").expect("write Tablet log");

        let selected_pixel = CleanupFilter {
            device_directories: Some(vec!["Pixel_8".to_owned()]),
            older_than: Some(SystemTime::now() + Duration::from_secs(60)),
        };
        let preview = preview_log_cleanup(&base_dir, &selected_pixel, &[tablet_log.clone()])
            .expect("preview cleanup");
        assert_eq!(preview.matching_files, 1);
        assert_eq!(preview.matching_bytes, 5);
        assert_eq!(preview.protected_files, 0);

        let outcome = cleanup_matching_logs(&base_dir, &selected_pixel, &[tablet_log.clone()])
            .expect("clean Pixel log");
        assert_eq!(outcome.deleted_files, 1);
        assert_eq!(outcome.freed_bytes, 5);
        assert!(outcome.failed_paths.is_empty());
        assert!(!pixel_log.exists());
        assert!(tablet_log.exists());

        let all_devices = CleanupFilter {
            device_directories: None,
            older_than: Some(SystemTime::now() + Duration::from_secs(60)),
        };
        let protected_preview = preview_log_cleanup(&base_dir, &all_devices, &[tablet_log.clone()])
            .expect("preview protected log");
        assert_eq!(protected_preview.matching_files, 0);
        assert_eq!(protected_preview.protected_files, 1);
        assert_eq!(protected_preview.protected_bytes, 6);

        let _ = fs::remove_dir_all(&base_dir);
    }

    fn unique_temp_dir(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("valid system time")
            .as_nanos();
        std::env::temp_dir().join(format!("logcatx-{label}-{unique}"))
    }
}
