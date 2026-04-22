use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AppConfig {
    pub adb_path: String,
    pub log_dir: String,
}

impl AppConfig {
    pub fn with_defaults() -> Self {
        let default_log_dir = project_dirs()
            .map(|dirs| dirs.data_dir().join("logs"))
            .or_else(|| std::env::current_dir().ok().map(|dir| dir.join("logs")))
            .unwrap_or_else(|| PathBuf::from("logs"));

        Self {
            adb_path: "adb".to_owned(),
            log_dir: default_log_dir.to_string_lossy().into_owned(),
        }
    }

    pub fn is_complete(&self) -> bool {
        !self.adb_path.trim().is_empty() && !self.log_dir.trim().is_empty()
    }
}

pub fn load_config() -> Result<AppConfig, String> {
    let path = config_file_path()?;
    if !path.exists() {
        return Ok(AppConfig::with_defaults());
    }

    let bytes = fs::read(&path).map_err(|err| format!("Failed to read config: {err}"))?;
    serde_json::from_slice::<AppConfig>(&bytes)
        .map_err(|err| format!("Failed to parse config {}: {err}", path.display()))
}

pub fn save_config(config: &AppConfig) -> Result<(), String> {
    let path = config_file_path()?;
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

pub fn config_file_path() -> Result<PathBuf, String> {
    let dirs = project_dirs()
        .ok_or_else(|| "Unsupported platform: cannot resolve app config directory".to_owned())?;
    Ok(dirs.config_dir().join("config.json"))
}

pub fn config_file_exists() -> bool {
    config_file_path()
        .map(|path| path.exists())
        .unwrap_or(false)
}

fn project_dirs() -> Option<ProjectDirs> {
    ProjectDirs::from("com", "Copilot", "AdbLogcatCollector")
}
