use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug)]
pub struct AppPaths {
    pub exe_dir: PathBuf,
    pub config_dir: PathBuf,
    pub config_path: PathBuf,
    pub app_log_path: PathBuf,
    pub portable_mode: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AppConfig {
    pub adb_path: String,
    pub log_dir: String,
    pub app_log_max_size_mb: u32,
}

impl AppConfig {
    pub fn with_defaults(paths: &AppPaths) -> Self {
        let default_log_dir = paths.exe_dir.join("logs");

        Self {
            adb_path: detect_adb_path().unwrap_or_default(),
            log_dir: default_log_dir.to_string_lossy().into_owned(),
            app_log_max_size_mb: 2,
        }
    }

    pub fn is_complete(&self) -> bool {
        !self.adb_path.trim().is_empty() && !self.log_dir.trim().is_empty()
    }
}

pub fn load_config(path: &Path) -> Result<AppConfig, String> {
    if !path.exists() {
        return Err(format!("Config file does not exist: {}", path.display()));
    }

    let bytes = fs::read(&path).map_err(|err| format!("Failed to read config: {err}"))?;
    serde_json::from_slice::<AppConfig>(&bytes)
        .map_err(|err| format!("Failed to parse config {}: {err}", path.display()))
}

pub fn save_config(path: &Path, config: &AppConfig) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "Config path has no parent directory".to_owned())?;
    fs::create_dir_all(parent).map_err(|err| {
        format!(
            "Failed to create config directory {}: {err}",
            parent.display()
        )
    })?;

    let content = serde_json::to_vec_pretty(config)
        .map_err(|err| format!("Failed to serialize config: {err}"))?;
    fs::write(&path, content)
        .map_err(|err| format!("Failed to write config {}: {err}", path.display()))
}

pub fn ensure_log_dir(path: &Path) -> Result<PathBuf, String> {
    fs::create_dir_all(path)
        .map_err(|err| format!("Failed to create log directory {}: {err}", path.display()))?;
    path.canonicalize()
        .or_else(|_| Ok(path.to_path_buf()))
        .map_err(|err: std::io::Error| {
            format!("Failed to resolve log directory {}: {err}", path.display())
        })
}

pub fn resolve_app_paths() -> Result<AppPaths, String> {
    let exe_path = std::env::current_exe()
        .map_err(|err| format!("Failed to resolve current exe path: {err}"))?;
    let exe_dir = exe_path
        .parent()
        .ok_or_else(|| {
            format!(
                "Executable path has no parent directory: {}",
                exe_path.display()
            )
        })?
        .to_path_buf();
    let portable_config_path = exe_dir.join("config.json");

    let (config_dir, portable_mode) = if portable_config_path.exists() || is_dir_writable(&exe_dir)
    {
        (exe_dir.clone(), true)
    } else {
        (appdata_config_dir()?, false)
    };

    Ok(AppPaths {
        exe_dir,
        app_log_path: config_dir.join(".adb-logcat-collector.log"),
        config_path: config_dir.join("config.json"),
        config_dir,
        portable_mode,
    })
}

fn appdata_config_dir() -> Result<PathBuf, String> {
    let dirs = project_dirs()
        .ok_or_else(|| "Unsupported platform: cannot resolve app config directory".to_owned())?;
    Ok(dirs.config_dir().to_path_buf())
}

fn project_dirs() -> Option<ProjectDirs> {
    ProjectDirs::from("com", "Copilot", "AdbLogcatCollector")
}

fn is_dir_writable(dir: &Path) -> bool {
    let probe = dir.join(".write-test.tmp");
    match fs::write(&probe, b"probe") {
        Ok(()) => {
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

pub fn detect_adb_path() -> Option<String> {
    let executable_name = if cfg!(target_os = "windows") {
        "adb.exe"
    } else {
        "adb"
    };

    if let Some(path) = search_path_for(executable_name) {
        return Some(path.to_string_lossy().into_owned());
    }

    for root_var in ["ANDROID_HOME", "ANDROID_SDK_ROOT"] {
        if let Some(root) = std::env::var_os(root_var) {
            let root = PathBuf::from(root);
            let candidate = root.join("platform-tools").join(executable_name);
            if is_executable_file(&candidate) {
                return Some(candidate.to_string_lossy().into_owned());
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
            let candidate = PathBuf::from(local_app_data)
                .join("Android")
                .join("Sdk")
                .join("platform-tools")
                .join(executable_name);
            if is_executable_file(&candidate) {
                return Some(candidate.to_string_lossy().into_owned());
            }
        }
    }

    None
}

fn search_path_for(executable_name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    let path_exts = executable_suffixes();

    for dir in std::env::split_paths(&path_var) {
        for suffix in &path_exts {
            let candidate = dir.join(format!("{executable_name}{suffix}"));
            if is_executable_file(&candidate) {
                return Some(candidate);
            }
        }
    }

    None
}

fn executable_suffixes() -> Vec<String> {
    if cfg!(target_os = "windows") {
        let mut suffixes = vec![String::new()];
        if let Some(path_ext) = std::env::var_os("PATHEXT") {
            for ext in path_ext
                .to_string_lossy()
                .split(';')
                .map(str::trim)
                .filter(|ext| !ext.is_empty())
            {
                suffixes.push(ext.to_ascii_lowercase());
            }
        }
        suffixes
    } else {
        vec![String::new()]
    }
}

fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

#[cfg(test)]
mod tests {
    use super::is_executable_file;
    use std::{fs, path::PathBuf};

    #[test]
    fn detect_adb_path_prefers_existing_candidate() {
        let temp_dir =
            std::env::temp_dir().join(format!("adb-logcat-collector-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).expect("create temp dir");

        let adb_name = if cfg!(target_os = "windows") {
            "adb.exe"
        } else {
            "adb"
        };
        let adb_path = temp_dir.join(adb_name);
        fs::write(&adb_path, b"stub").expect("write adb stub");

        assert!(is_executable_file(&adb_path));

        fs::remove_file(&adb_path).expect("cleanup adb stub");
        let _ = fs::remove_dir_all(PathBuf::from(&temp_dir));
    }
}
