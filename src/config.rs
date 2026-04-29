use crate::{fs_utils, i18n};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

pub type AppPaths = desktop_config::PortableAppPaths;
pub const MAX_RECENT_CONNECTIONS: usize = 8;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub adb_path: String,
    #[serde(default)]
    pub log_dir: String,
    #[serde(default = "default_app_log_max_size_mb")]
    pub app_log_max_size_mb: u32,
    #[serde(default)]
    pub language: String,
    #[serde(default)]
    pub device_aliases: BTreeMap<String, String>,
    #[serde(default)]
    pub pinned_devices: Vec<String>,
    #[serde(default)]
    pub recent_connections: Vec<String>,
}

impl AppConfig {
    pub fn with_defaults(paths: &AppPaths) -> Self {
        let default_log_dir = paths.exe_dir.join("logs");

        Self {
            adb_path: detect_adb_path()
                .map(|path| fs_utils::display_path_string(&path))
                .unwrap_or_default(),
            log_dir: fs_utils::display_path(&default_log_dir),
            app_log_max_size_mb: default_app_log_max_size_mb(),
            language: i18n::detect_system_language(),
            device_aliases: BTreeMap::new(),
            pinned_devices: Vec::new(),
            recent_connections: Vec::new(),
        }
    }

    pub fn is_complete(&self) -> bool {
        !self.adb_path.trim().is_empty() && !self.log_dir.trim().is_empty()
    }
}

pub fn load_config(path: &Path, paths: &AppPaths) -> Result<AppConfig, String> {
    if !path.exists() {
        return Err(format!("Config file does not exist: {}", path.display()));
    }

    let config = desktop_config::load_json::<AppConfig>(path)?;
    Ok(normalize_config(config, paths))
}

pub fn save_config(path: &Path, config: &AppConfig) -> Result<(), String> {
    let mut normalized = config.clone();
    normalized.language = i18n::normalize_language_code(&normalized.language).to_owned();
    normalized.log_dir = fs_utils::display_path_string(&normalized.log_dir);
    normalized.adb_path = fs_utils::display_path_string(&normalized.adb_path);
    if normalized.app_log_max_size_mb == 0 {
        normalized.app_log_max_size_mb = default_app_log_max_size_mb();
    }
    normalized.device_aliases = normalize_aliases(normalized.device_aliases);
    normalized.pinned_devices = normalize_serial_list(normalized.pinned_devices);
    normalized.recent_connections = normalize_recent_connections(normalized.recent_connections);

    desktop_config::save_pretty_json(path, &normalized)
}

pub fn ensure_log_dir(path: &Path) -> Result<PathBuf, String> {
    fs::create_dir_all(path)
        .map_err(|err| format!("Failed to create log directory {}: {err}", path.display()))?;
    path.canonicalize()
        .map(|canonical| fs_utils::normalize_display_path(&canonical))
        .or_else(|_| Ok(fs_utils::normalize_display_path(path)))
        .map_err(|err: std::io::Error| {
            format!("Failed to resolve log directory {}: {err}", path.display())
        })
}

pub fn resolve_app_paths() -> Result<AppPaths, String> {
    desktop_config::resolve_portable_app_paths(
        "com",
        "Copilot",
        "LogcatX",
        "config.json",
        ".logcatx.log",
    )
}

fn normalize_config(mut config: AppConfig, paths: &AppPaths) -> AppConfig {
    let defaults = AppConfig::with_defaults(paths);

    if config.adb_path.trim().is_empty() {
        config.adb_path = defaults.adb_path;
    } else {
        config.adb_path = fs_utils::display_path_string(&config.adb_path);
    }

    if config.log_dir.trim().is_empty() {
        config.log_dir = defaults.log_dir;
    } else {
        config.log_dir = fs_utils::display_path_string(&config.log_dir);
    }

    if config.app_log_max_size_mb == 0 {
        config.app_log_max_size_mb = default_app_log_max_size_mb();
    }

    config.language = if config.language.trim().is_empty() {
        i18n::detect_system_language()
    } else {
        i18n::normalize_language_code(&config.language).to_owned()
    };
    config.device_aliases = normalize_aliases(config.device_aliases);
    config.pinned_devices = normalize_serial_list(config.pinned_devices);
    config.recent_connections = normalize_recent_connections(config.recent_connections);

    config
}

fn normalize_aliases(aliases: BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut normalized = BTreeMap::new();

    for (serial, alias) in aliases {
        let serial = serial.trim();
        let alias = alias.trim();
        if serial.is_empty() || alias.is_empty() {
            continue;
        }
        normalized.insert(serial.to_owned(), alias.to_owned());
    }

    normalized
}

fn normalize_serial_list(values: Vec<String>) -> Vec<String> {
    let mut normalized = Vec::new();

    for value in values {
        let value = value.trim();
        if value.is_empty() || normalized.iter().any(|existing| existing == value) {
            continue;
        }
        normalized.push(value.to_owned());
    }

    normalized
}

fn normalize_recent_connections(values: Vec<String>) -> Vec<String> {
    let mut normalized = normalize_serial_list(values);
    normalized.truncate(MAX_RECENT_CONNECTIONS);
    normalized
}

fn default_app_log_max_size_mb() -> u32 {
    2
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
    use super::{
        MAX_RECENT_CONNECTIONS, default_app_log_max_size_mb, is_executable_file, normalize_aliases,
        normalize_recent_connections, normalize_serial_list,
    };
    use std::{collections::BTreeMap, fs, path::PathBuf};

    #[test]
    fn detect_adb_path_prefers_existing_candidate() {
        let temp_dir = std::env::temp_dir().join(format!("logcatx-test-{}", std::process::id()));
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

    #[test]
    fn default_app_log_size_is_nonzero() {
        assert_eq!(default_app_log_max_size_mb(), 2);
    }

    #[test]
    fn normalize_aliases_discards_empty_entries() {
        let mut aliases = BTreeMap::new();
        aliases.insert("serial-1".to_owned(), "Pixel".to_owned());
        aliases.insert(" ".to_owned(), "ignored".to_owned());
        aliases.insert("serial-2".to_owned(), "   ".to_owned());

        let normalized = normalize_aliases(aliases);
        assert_eq!(normalized.len(), 1);
        assert_eq!(normalized.get("serial-1"), Some(&"Pixel".to_owned()));
    }

    #[test]
    fn normalize_serial_list_deduplicates_and_trims() {
        let normalized = normalize_serial_list(vec![
            " serial-1 ".to_owned(),
            "serial-1".to_owned(),
            "".to_owned(),
            "serial-2".to_owned(),
        ]);

        assert_eq!(
            normalized,
            vec!["serial-1".to_owned(), "serial-2".to_owned()]
        );
    }

    #[test]
    fn normalize_recent_connections_caps_length() {
        let values: Vec<String> = (0..MAX_RECENT_CONNECTIONS + 3)
            .map(|index| format!("192.168.0.{index}:5555"))
            .collect();

        let normalized = normalize_recent_connections(values);
        assert_eq!(normalized.len(), MAX_RECENT_CONNECTIONS);
        assert_eq!(normalized[0], "192.168.0.0:5555");
    }
}
