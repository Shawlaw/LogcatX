use chrono::Local;
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
};

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

pub fn dir_size(path: &Path) -> Result<u64, String> {
    if !path.exists() {
        return Ok(0);
    }
    dir_size_inner(path)
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

pub fn clear_history_logs(base_dir: &Path, protected_paths: &[PathBuf]) -> Result<(), String> {
    if !base_dir.exists() {
        return Ok(());
    }

    let protected: HashSet<PathBuf> = protected_paths
        .iter()
        .map(|path| normalize_path(path))
        .collect();

    clear_logs_recursive(base_dir, &protected)?;
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
            remove_empty_dirs(&entry.path())?;
        }
    }
    Ok(())
}

fn dir_size_inner(path: &Path) -> Result<u64, String> {
    let mut total = 0u64;
    for entry in fs::read_dir(path)
        .map_err(|err| format!("Failed to read directory {}: {err}", path.display()))?
    {
        let entry = entry.map_err(|err| format!("Failed to read directory entry: {err}"))?;
        let metadata = entry.metadata().map_err(|err| {
            format!(
                "Failed to read metadata for {}: {err}",
                entry.path().display()
            )
        })?;
        if metadata.is_dir() {
            total += dir_size_inner(&entry.path())?;
        } else if metadata.is_file() {
            total += metadata.len();
        }
    }
    Ok(total)
}

fn clear_logs_recursive(path: &Path, protected: &HashSet<PathBuf>) -> Result<(), String> {
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
            clear_logs_recursive(&entry_path, protected)?;
        } else if metadata.is_file()
            && entry_path.extension().and_then(|ext| ext.to_str()) == Some("log")
        {
            let normalized = normalize_path(&entry_path);
            if protected.contains(&normalized) {
                continue;
            }
            fs::remove_file(&entry_path)
                .map_err(|err| format!("Failed to remove {}: {err}", entry_path.display()))?;
        }
    }

    Ok(())
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

#[cfg(test)]
mod tests {
    use super::{
        device_log_dir, display_path_string, format_bytes, rename_device_log_dir, sanitize_serial,
        session_log_path,
    };
    use std::{
        fs,
        path::Path,
        time::{SystemTime, UNIX_EPOCH},
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
}
