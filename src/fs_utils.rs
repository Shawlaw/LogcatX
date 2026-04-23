use chrono::Local;
use std::{
    collections::HashSet,
    fs,
    path::{Path, PathBuf},
    process::Command,
};

pub fn session_log_path(base_dir: &Path, serial: &str) -> PathBuf {
    let file_name = format!(
        "{}-{}.log",
        sanitize_serial(serial),
        Local::now().format("%Y%m%d-%H%M%S")
    );
    base_dir.join(sanitize_serial(serial)).join(file_name)
}

pub fn sanitize_serial(serial: &str) -> String {
    serial
        .chars()
        .map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '-' | '_' | '.' => ch,
            _ => '_',
        })
        .collect()
}

pub fn dir_size(path: &Path) -> Result<u64, String> {
    if !path.exists() {
        return Ok(0);
    }
    dir_size_inner(path)
}

pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit_idx = 0usize;

    while value >= 1024.0 && unit_idx < UNITS.len() - 1 {
        value /= 1024.0;
        unit_idx += 1;
    }

    if unit_idx == 0 {
        format!("{bytes} {}", UNITS[unit_idx])
    } else {
        format!("{value:.2} {}", UNITS[unit_idx])
    }
}

pub fn display_path(path: &Path) -> String {
    simplify_windows_path(&path.to_string_lossy())
}

pub fn display_path_string(path: &str) -> String {
    simplify_windows_path(path)
}

pub fn normalize_display_path(path: &Path) -> PathBuf {
    PathBuf::from(display_path(path))
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

fn simplify_windows_path(path: &str) -> String {
    #[cfg(target_os = "windows")]
    {
        if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
            return format!(r"\\{}", rest);
        }
        if let Some(rest) = path.strip_prefix(r"\\?\") {
            return rest.to_owned();
        }
    }

    path.to_owned()
}

pub fn open_path(path: &Path) -> Result<(), String> {
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut cmd = Command::new("explorer");
        cmd.arg(path);
        cmd
    };

    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut cmd = Command::new("open");
        cmd.arg(path);
        cmd
    };

    #[cfg(all(not(target_os = "windows"), not(target_os = "macos")))]
    let mut cmd = {
        let mut cmd = Command::new("xdg-open");
        cmd.arg(path);
        cmd
    };

    cmd.spawn()
        .map(|_| ())
        .map_err(|err| format!("Failed to open {}: {err}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::{display_path_string, format_bytes, sanitize_serial, session_log_path};
    use std::path::Path;

    #[test]
    fn sanitize_serial_replaces_path_unsafe_characters() {
        assert_eq!(sanitize_serial("emulator:5554/usb"), "emulator_5554_usb");
    }

    #[test]
    fn session_log_path_keeps_device_subdirectory_and_log_extension() {
        let path = session_log_path(Path::new("/tmp/logs"), "serial:1");
        let text = path.display().to_string();
        assert!(text.contains("/tmp/logs/serial_1/serial_1-"));
        assert!(text.ends_with(".log"));
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
}
