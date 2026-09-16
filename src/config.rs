use crate::{fs_utils, i18n};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

pub type AppPaths = desktop_config::PortableAppPaths;
pub const MAX_RECENT_CONNECTIONS: usize = 8;

/// How signed update traffic reaches the release endpoints.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateProxyMode {
    #[default]
    Automatic,
    Custom,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateProxyConfig {
    #[serde(default)]
    pub mode: UpdateProxyMode,
    #[serde(default)]
    pub url: String,
}

impl UpdateProxyConfig {
    pub fn custom_url(&self) -> Option<&str> {
        (self.mode == UpdateProxyMode::Custom)
            .then_some(self.url.trim())
            .filter(|url| !url.is_empty())
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub adb_path: String,
    #[serde(default)]
    pub scrcpy_path: String,
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
    /// Recent endpoints known to use wireless debugging. Re-discover their
    /// hosts on reconnect; legacy endpoints on the same host stay independent.
    #[serde(default)]
    pub wireless_connections: Vec<String>,
    /// Devices (identity keys) whose dropped APKs install directly without
    /// the per-drop confirmation dialog.
    #[serde(default)]
    pub apk_auto_install_devices: Vec<String>,
    #[serde(default)]
    pub device_logcat_args: BTreeMap<String, String>,
    #[serde(default = "default_auto_check_updates")]
    pub auto_check_updates: bool,
    #[serde(default)]
    pub update_proxy: UpdateProxyConfig,
}

impl AppConfig {
    pub fn with_defaults(paths: &AppPaths) -> Self {
        let default_log_dir = paths.exe_dir.join("logs");

        Self {
            adb_path: detect_adb_path()
                .map(|path| fs_utils::display_path_string(&path))
                .unwrap_or_default(),
            scrcpy_path: detect_scrcpy_path()
                .map(|path| fs_utils::display_path_string(&path))
                .unwrap_or_default(),
            log_dir: fs_utils::display_path(&default_log_dir),
            app_log_max_size_mb: default_app_log_max_size_mb(),
            language: i18n::detect_system_language(),
            device_aliases: BTreeMap::new(),
            pinned_devices: Vec::new(),
            recent_connections: Vec::new(),
            wireless_connections: Vec::new(),
            apk_auto_install_devices: Vec::new(),
            device_logcat_args: BTreeMap::new(),
            auto_check_updates: default_auto_check_updates(),
            update_proxy: UpdateProxyConfig::default(),
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
    normalized.scrcpy_path = fs_utils::display_path_string(&normalized.scrcpy_path);
    if normalized.app_log_max_size_mb == 0 {
        normalized.app_log_max_size_mb = default_app_log_max_size_mb();
    }
    normalized.device_aliases = normalize_aliases(normalized.device_aliases);
    normalized.pinned_devices = normalize_serial_list(normalized.pinned_devices);
    normalized.recent_connections = normalize_recent_connections(normalized.recent_connections);
    normalized.wireless_connections = normalize_wireless_connections(
        normalized.wireless_connections,
        &normalized.recent_connections,
    );
    normalized.apk_auto_install_devices = normalize_serial_list(normalized.apk_auto_install_devices);
    normalized.device_logcat_args = normalize_logcat_args(normalized.device_logcat_args);
    normalized.update_proxy = normalize_update_proxy(normalized.update_proxy);

    desktop_config::save_pretty_json(path, &normalized)
}

/// Commit history and its transport metadata together, leaving memory unchanged
/// if saving or reloading the candidate fails.
pub fn remember_recent_connection(
    config: &mut AppConfig,
    paths: &AppPaths,
    target: &str,
    wireless: bool,
) -> Result<(), String> {
    let mut candidate = config.clone();
    candidate.recent_connections.retain(|value| {
        crate::wireless::parse_endpoint(value, false)
            .map(|endpoint| endpoint.to_string() != target)
            .unwrap_or(value != target)
    });
    candidate.recent_connections.insert(0, target.to_owned());
    candidate
        .recent_connections
        .truncate(MAX_RECENT_CONNECTIONS);
    candidate
        .wireless_connections
        .retain(|value| value != target);
    if wireless {
        candidate.wireless_connections.push(target.to_owned());
    }
    save_config(&paths.config_path, &candidate)?;
    *config = load_config(&paths.config_path, paths)?;
    Ok(())
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

    if config.scrcpy_path.trim().is_empty() {
        config.scrcpy_path = defaults.scrcpy_path;
    } else {
        config.scrcpy_path = fs_utils::display_path_string(&config.scrcpy_path);
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
    config.wireless_connections =
        normalize_wireless_connections(config.wireless_connections, &config.recent_connections);
    config.apk_auto_install_devices = normalize_serial_list(config.apk_auto_install_devices);
    config.device_logcat_args = normalize_logcat_args(config.device_logcat_args);
    config.update_proxy = normalize_update_proxy(config.update_proxy);

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
    let values = values
        .into_iter()
        .map(|value| {
            crate::wireless::parse_endpoint(&value, false)
                .map(|endpoint| endpoint.to_string())
                .unwrap_or(value)
        })
        .collect();
    let mut normalized = normalize_serial_list(values);
    normalized.truncate(MAX_RECENT_CONNECTIONS);
    normalized
}

fn normalize_wireless_connections(values: Vec<String>, recent: &[String]) -> Vec<String> {
    normalize_recent_connections(values)
        .into_iter()
        .filter(|target| recent.contains(target))
        .collect()
}

fn normalize_logcat_args(args: BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut normalized = BTreeMap::new();
    for (serial, args_str) in args {
        let serial = serial.trim();
        let args_str = args_str.trim();
        if serial.is_empty() || args_str.is_empty() {
            continue;
        }
        normalized.insert(serial.to_owned(), args_str.to_owned());
    }
    normalized
}

fn normalize_update_proxy(mut proxy: UpdateProxyConfig) -> UpdateProxyConfig {
    proxy.url = proxy.url.trim().to_owned();
    if proxy.mode == UpdateProxyMode::Automatic {
        proxy.url.clear();
    }
    proxy
}

fn default_app_log_max_size_mb() -> u32 {
    2
}

fn default_auto_check_updates() -> bool {
    true
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

pub fn detect_scrcpy_path() -> Option<String> {
    let executable_name = if cfg!(target_os = "windows") {
        "scrcpy.exe"
    } else {
        "scrcpy"
    };

    search_path_for(executable_name).map(|path| path.to_string_lossy().into_owned())
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
        normalize_logcat_args, normalize_recent_connections, normalize_serial_list,
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

    #[test]
    fn recent_connections_normalize_width_and_keep_transport_types_independent() {
        let recent = normalize_recent_connections(vec![
            "１２７。０。０。１：５５５５".into(),
            "127.0.0.1".into(),
            "127.0.0.1:39001".into(),
        ]);
        assert_eq!(recent, vec!["127.0.0.1:5555", "127.0.0.1:39001"]);
        let wireless = super::normalize_wireless_connections(
            vec![
                "１２７。０。０。１：３９００１".into(),
                "127.0.0.1:39000".into(),
            ],
            &recent,
        );
        assert_eq!(wireless, vec!["127.0.0.1:39001"]);
        let old: super::AppConfig = serde_json::from_str("{}").unwrap();
        assert!(old.wireless_connections.is_empty());
    }

    fn history_test_paths(root: &std::path::Path) -> super::AppPaths {
        super::AppPaths {
            exe_dir: root.to_owned(),
            config_dir: root.to_owned(),
            config_path: root.join("config.json"),
            app_log_path: root.join("test.log"),
            portable_mode: true,
        }
    }

    #[test]
    fn failed_history_save_preserves_evicted_entries_and_transport_changes() {
        let dir = tempfile::tempdir().unwrap();
        let paths = history_test_paths(dir.path());
        // A directory at the config path reliably rejects writes on all platforms.
        fs::create_dir(&paths.config_path).unwrap();
        let original = super::AppConfig {
            recent_connections: (1..=MAX_RECENT_CONNECTIONS)
                .map(|index| format!("127.0.0.{index}:39001"))
                .collect(),
            wireless_connections: vec!["127.0.0.1:39001".into(), "127.0.0.8:39001".into()],
            ..Default::default()
        };
        for (target, wireless) in [
            ("127.0.0.9:39001", true),  // insertion would evict a wireless entry
            ("127.0.0.1:39001", false), // conversion would remove its marker
            ("127.0.0.2:39001", true),  // conversion would add its marker
        ] {
            let mut config = original.clone();
            assert!(
                super::remember_recent_connection(&mut config, &paths, target, wireless).is_err()
            );
            assert_eq!(
                serde_json::to_value(config).unwrap(),
                serde_json::to_value(&original).unwrap()
            );
        }
    }

    #[test]
    fn successful_history_save_commits_matching_history_and_transport_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let paths = history_test_paths(dir.path());
        let mut config = super::AppConfig {
            recent_connections: (1..=MAX_RECENT_CONNECTIONS)
                .map(|index| format!("127.0.0.{index}:39001"))
                .collect(),
            wireless_connections: vec!["127.0.0.1:39001".into(), "127.0.0.8:39001".into()],
            ..Default::default()
        };
        super::remember_recent_connection(&mut config, &paths, "127.0.0.9:39001", true).unwrap();
        assert_eq!(config.recent_connections.len(), MAX_RECENT_CONNECTIONS);
        assert_eq!(config.recent_connections[0], "127.0.0.9:39001");
        assert_eq!(
            config.wireless_connections,
            ["127.0.0.1:39001", "127.0.0.9:39001"]
        );
        super::remember_recent_connection(&mut config, &paths, "127.0.0.1:39001", false).unwrap();
        assert_eq!(config.wireless_connections, ["127.0.0.9:39001"]);
        let saved = super::load_config(&paths.config_path, &paths).unwrap();
        assert_eq!(
            serde_json::to_value(config).unwrap(),
            serde_json::to_value(saved).unwrap()
        );
    }

    #[test]
    fn apk_auto_install_devices_round_trip_and_normalize() {
        let dir = tempfile::tempdir().unwrap();
        let paths = history_test_paths(dir.path());
        let legacy: super::AppConfig = serde_json::from_str("{}").unwrap();
        assert!(legacy.apk_auto_install_devices.is_empty());
        super::save_config(
            &paths.config_path,
            &super::AppConfig {
                apk_auto_install_devices: vec![
                    "  Google Pixel 8  ".into(),
                    "Google Pixel 8".into(),
                ],
                ..Default::default()
            },
        )
        .unwrap();
        let loaded = super::load_config(&paths.config_path, &paths).unwrap();
        assert_eq!(loaded.apk_auto_install_devices, ["Google Pixel 8"]);
    }

    #[test]
    fn normalize_logcat_args_trims_values_and_discards_empty() {
        let raw = BTreeMap::from([
            ("serial-1".to_owned(), "  -v threadtime  ".to_owned()),
            ("serial-2".to_owned(), "   ".to_owned()),
            ("serial-3".to_owned(), "-s Tag:V".to_owned()),
        ]);
        let normalized = normalize_logcat_args(raw);
        assert_eq!(normalized.len(), 2);
        assert_eq!(
            normalized.get("serial-1"),
            Some(&"-v threadtime".to_owned())
        );
        assert_eq!(normalized.get("serial-3"), Some(&"-s Tag:V".to_owned()));
    }

    #[test]
    fn normalize_logcat_args_discards_empty_serials() {
        let raw = BTreeMap::from([
            ("".to_owned(), "-v threadtime".to_owned()),
            (" ".to_owned(), "-s Tag:V".to_owned()),
        ]);
        let normalized = normalize_logcat_args(raw);
        assert!(normalized.is_empty());
    }

    #[test]
    fn config_round_trips_device_logcat_args() {
        let mut config = super::AppConfig::default();
        config
            .device_logcat_args
            .insert("serial-1".into(), "-v threadtime".into());
        let json = serde_json::to_string(&config).unwrap();
        let loaded: super::AppConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.device_logcat_args.len(), 1);
        assert_eq!(
            loaded.device_logcat_args.get("serial-1"),
            Some(&"-v threadtime".to_owned())
        );
    }

    #[test]
    fn config_deserializes_without_logcat_args_field() {
        let json = r#"{"adb_path":"","log_dir":"","app_log_max_size_mb":2,"language":"","device_aliases":{},"pinned_devices":[],"recent_connections":[]}"#;
        let loaded: super::AppConfig = serde_json::from_str(json).unwrap();
        assert!(loaded.device_logcat_args.is_empty());
        assert!(loaded.scrcpy_path.is_empty());
    }

    #[test]
    fn config_defaults_auto_check_updates_for_existing_files() {
        let json = r#"{"adb_path":"","log_dir":"","app_log_max_size_mb":2,"language":"","device_aliases":{},"pinned_devices":[],"recent_connections":[],"device_logcat_args":{}}"#;
        let loaded: super::AppConfig = serde_json::from_str(json).unwrap();
        assert!(loaded.auto_check_updates);

        let json = r#"{"adb_path":"","log_dir":"","app_log_max_size_mb":2,"language":"","device_aliases":{},"pinned_devices":[],"recent_connections":[],"device_logcat_args":{},"auto_check_updates":false}"#;
        let loaded: super::AppConfig = serde_json::from_str(json).unwrap();
        assert!(!loaded.auto_check_updates);
    }

    #[test]
    fn config_defaults_update_proxy_for_existing_files() {
        let json = r#"{"adb_path":"","log_dir":"","app_log_max_size_mb":2,"language":"","device_aliases":{},"pinned_devices":[],"recent_connections":[],"device_logcat_args":{}}"#;
        let loaded: super::AppConfig = serde_json::from_str(json).unwrap();

        assert_eq!(loaded.update_proxy.mode, super::UpdateProxyMode::Automatic);
        assert!(loaded.update_proxy.url.is_empty());
    }

    #[test]
    fn normalize_update_proxy_trims_custom_url_and_clears_automatic_url() {
        let custom = super::normalize_update_proxy(super::UpdateProxyConfig {
            mode: super::UpdateProxyMode::Custom,
            url: "  socks5h://127.0.0.1:7890  ".to_owned(),
        });
        assert_eq!(custom.custom_url(), Some("socks5h://127.0.0.1:7890"));

        let automatic = super::normalize_update_proxy(super::UpdateProxyConfig {
            mode: super::UpdateProxyMode::Automatic,
            url: "http://should-not-be-used:7890".to_owned(),
        });
        assert!(automatic.url.is_empty());
    }
}
