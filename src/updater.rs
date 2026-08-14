//! Signed application updates backed by the DeskFoundry `desktop-updater`
//! contract: a Raw GitHub manifest describes the latest portable ZIP, its
//! Ed25519 signature is verified against the public key compiled into this
//! build, and a helper executable replaces the installation after the app
//! exits. When `LOGCATX_UPDATE_PUBLIC_KEY` is not set at build time the whole
//! feature reports itself as unconfigured instead of contacting the network.

use chrono::{Local, Timelike};
use desktop_updater::ApplyRequest;
use desktop_updater::PortableLayout;
use desktop_updater::UpdateCandidate;
use desktop_updater::UpdateConfig;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
};

pub const APP_ID: &str = "com.logcatx.app";
pub const CHANNEL: &str = "stable";
pub const MANIFEST_URL: &str =
    "https://raw.githubusercontent.com/Shawlaw/LogcatX/master/updates/stable.json";
pub const SIGNATURE_URL: &str =
    "https://raw.githubusercontent.com/Shawlaw/LogcatX/master/updates/stable.json.sig";
pub const MAIN_EXE_NAME: &str = "LogcatX.exe";
pub const HELPER_EXE_NAME: &str = "LogcatX.Updater.exe";
pub const STATUS_CACHE_FILE: &str = "app_update_status.logcatx.json";
const STATUS_CACHE_SCHEMA_VERSION: u8 = 1;
/// Automatic checks stay quiet before this local hour so a freshly opened
/// machine does not spend its first minutes on update traffic.
const AUTOMATIC_CHECK_START_HOUR: u32 = 8;

/// Files a release ZIP is allowed to replace in the installation directory.
/// Must stay in sync with `desktop-update.toml` at the repository root.
pub const RELEASE_REPLACE_FILES: &[&str] = &[
    MAIN_EXE_NAME,
    HELPER_EXE_NAME,
    "README.md",
    "README.en.md",
    "CHANGELOG.md",
    "CHANGELOG.en.md",
    "LICENSE",
    "config.example.json",
    "icons/icon_128.png",
];

pub fn public_key() -> Option<&'static str> {
    option_env!("LOGCATX_UPDATE_PUBLIC_KEY")
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

pub fn updates_configured() -> bool {
    public_key().is_some()
}

pub fn update_config(current_version: &str) -> Option<UpdateConfig> {
    let public_key = public_key()?;
    Some(UpdateConfig::new(
        APP_ID,
        CHANNEL,
        current_version,
        MANIFEST_URL,
        SIGNATURE_URL,
        public_key,
    ))
}

pub fn status_cache_path(config_dir: &Path) -> PathBuf {
    config_dir.join(STATUS_CACHE_FILE)
}

pub fn updates_dir(config_dir: &Path) -> PathBuf {
    config_dir.join("updates")
}

/// Describes the flat portable installation the update helper is allowed to
/// rewrite. Derived from the running executable's directory so dev builds
/// report a clear error instead of touching unrelated folders.
pub fn apply_request(install_dir: &Path) -> ApplyRequest {
    ApplyRequest {
        helper_path: install_dir.join(HELPER_EXE_NAME),
        restart_executable: install_dir.join(MAIN_EXE_NAME),
        install_dir: install_dir.to_path_buf(),
        layout: PortableLayout::flat(RELEASE_REPLACE_FILES.iter().map(|file| file.to_string())),
    }
}

pub fn install_dir_from_current_exe() -> Result<PathBuf, String> {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .ok_or_else(|| "Unable to resolve the application installation directory".to_owned())
}

pub fn today_local() -> String {
    Local::now().date_naive().to_string()
}

pub fn local_hour() -> u32 {
    Local::now().hour()
}

pub fn automatic_check_is_due(
    auto_check_enabled: bool,
    local_hour: u32,
    today: &str,
    last_automatic_check_date: Option<&str>,
) -> bool {
    auto_check_enabled
        && local_hour >= AUTOMATIC_CHECK_START_HOUR
        && last_automatic_check_date != Some(today)
}

/// Persisted snapshot of the last check so the UI badge survives restarts and
/// automatic checks happen at most once per local day.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UpdateStatusCache {
    pub schema_version: u8,
    pub last_automatic_check_date: Option<String>,
    pub checked_at: Option<String>,
    pub current_version: Option<String>,
    pub available: bool,
    pub version: Option<String>,
    pub notes_url: Option<String>,
    pub error: Option<String>,
    pub dismissed_version: Option<String>,
}

impl UpdateStatusCache {
    /// Discards stale state written by a different application version.
    pub fn reset_for_version(&mut self, current_version: &str) {
        if self.current_version.as_deref() != Some(current_version) {
            *self = Self::default();
        }
    }

    pub fn record_check(
        &mut self,
        current_version: &str,
        candidate: Option<&UpdateCandidate>,
        automatic: bool,
    ) {
        self.reset_for_version(current_version);
        self.schema_version = STATUS_CACHE_SCHEMA_VERSION;
        self.current_version = Some(current_version.to_owned());
        self.checked_at = Some(Local::now().to_rfc3339());
        self.error = None;
        match candidate {
            None => {
                self.available = false;
                self.version = None;
                self.notes_url = None;
                self.dismissed_version = None;
            }
            Some(candidate) => {
                let version = candidate.version().to_owned();
                if self.version.as_deref() != Some(version.as_str()) {
                    self.dismissed_version = None;
                }
                self.available = true;
                self.version = Some(version);
                self.notes_url = candidate.notes_url().map(str::to_owned);
            }
        }
        if automatic {
            self.last_automatic_check_date = Some(today_local());
        }
    }

    pub fn record_failure(&mut self, current_version: &str, error: String, automatic: bool) {
        self.reset_for_version(current_version);
        self.schema_version = STATUS_CACHE_SCHEMA_VERSION;
        self.current_version = Some(current_version.to_owned());
        self.checked_at = Some(Local::now().to_rfc3339());
        self.error = Some(error);
        if automatic {
            self.last_automatic_check_date = Some(today_local());
        }
    }

    /// Marks today as automatically checked even though no check could run,
    /// so unconfigured builds do not retry every focus event.
    pub fn record_automatic_skipped(&mut self, current_version: &str) {
        self.reset_for_version(current_version);
        self.schema_version = STATUS_CACHE_SCHEMA_VERSION;
        self.current_version = Some(current_version.to_owned());
        self.checked_at = Some(Local::now().to_rfc3339());
        self.last_automatic_check_date = Some(today_local());
    }

    pub fn dismiss_available(&mut self) {
        if self.available {
            self.dismissed_version = self.version.clone();
        }
    }

    pub fn is_available_for(&self, current_version: &str) -> bool {
        self.current_version.as_deref() == Some(current_version) && self.available
    }

    pub fn is_dismissed(&self) -> bool {
        self.available && self.dismissed_version == self.version
    }
}

pub fn load_status_cache(path: &Path) -> UpdateStatusCache {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_default(),
        Err(_) => UpdateStatusCache::default(),
    }
}

pub fn write_status_cache(path: &Path, cache: &UpdateStatusCache) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            format!(
                "Failed to create application update status directory {}: {error}",
                path.display()
            )
        })?;
    }
    let bytes = serde_json::to_vec_pretty(cache)
        .map_err(|error| format!("Failed to encode update status: {error}"))?;
    fs::write(path, bytes).map_err(|error| {
        format!(
            "Failed to write application update status {}: {error}",
            path.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::{
        UpdateStatusCache, automatic_check_is_due, load_status_cache, today_local,
        write_status_cache,
    };
    use desktop_updater::{UpdateAsset, UpdateCandidate, UpdateManifest};
    use std::fs;
    use std::path::{Path, PathBuf};

    fn candidate(version: &str) -> UpdateCandidate {
        UpdateCandidate {
            manifest: UpdateManifest {
                schema_version: 1,
                app_id: super::APP_ID.to_owned(),
                channel: super::CHANNEL.to_owned(),
                version: version.to_owned(),
                published_at: "2026-08-15T00:00:00Z".to_owned(),
                target: "windows-x64".to_owned(),
                asset: UpdateAsset {
                    url: "https://example.com/app.zip".to_owned(),
                    sha256: "0".repeat(64),
                    size: 1,
                },
                notes_url: Some("https://example.com/notes".to_owned()),
            },
        }
    }

    #[test]
    fn automatic_check_only_runs_once_after_eight_am() {
        assert!(!automatic_check_is_due(true, 7, "2026-08-15", None));
        assert!(automatic_check_is_due(true, 8, "2026-08-15", None));
        assert!(!automatic_check_is_due(
            true,
            9,
            "2026-08-15",
            Some("2026-08-15")
        ));
        assert!(automatic_check_is_due(
            true,
            9,
            "2026-08-15",
            Some("2026-08-14")
        ));
        assert!(!automatic_check_is_due(false, 9, "2026-08-15", None));
    }

    #[test]
    fn apply_layout_only_allows_release_files() {
        let request = super::apply_request(Path::new("C:/Tools/LogcatX"));
        assert_eq!(
            request.helper_path,
            PathBuf::from("C:/Tools/LogcatX/LogcatX.Updater.exe")
        );
        assert_eq!(
            request.restart_executable,
            PathBuf::from("C:/Tools/LogcatX/LogcatX.exe")
        );
        assert_eq!(
            request.layout.replace_files.len(),
            super::RELEASE_REPLACE_FILES.len()
        );
        assert!(
            request
                .layout
                .replace_files
                .contains(&super::MAIN_EXE_NAME.to_owned())
        );
        assert!(
            request
                .layout
                .replace_files
                .contains(&super::HELPER_EXE_NAME.to_owned())
        );
        assert!(request.layout.preserve_files.is_empty());
    }

    #[test]
    fn record_check_tracks_candidate_and_resets_dismissal_for_new_version() {
        let mut cache = UpdateStatusCache::default();
        cache.record_check("0.6.0", Some(&candidate("0.7.0")), false);
        assert!(cache.is_available_for("0.6.0"));
        assert_eq!(cache.version.as_deref(), Some("0.7.0"));
        assert_eq!(
            cache.notes_url.as_deref(),
            Some("https://example.com/notes")
        );

        cache.dismiss_available();
        assert!(cache.is_dismissed());

        cache.record_check("0.6.0", Some(&candidate("0.8.0")), false);
        assert!(!cache.is_dismissed());
        assert_eq!(cache.version.as_deref(), Some("0.8.0"));
    }

    #[test]
    fn record_check_without_candidate_clears_availability() {
        let mut cache = UpdateStatusCache::default();
        cache.record_check("0.6.0", Some(&candidate("0.7.0")), false);
        cache.record_check("0.6.0", None, true);
        assert!(!cache.is_available_for("0.6.0"));
        assert!(cache.version.is_none());
        assert_eq!(
            cache.last_automatic_check_date.as_deref(),
            Some(today_local().as_str())
        );
    }

    #[test]
    fn stale_cache_is_reset_when_version_changes() {
        let mut cache = UpdateStatusCache::default();
        cache.record_check("0.6.0", Some(&candidate("0.7.0")), false);
        cache.record_check("0.8.0", None, false);
        assert_eq!(cache.current_version.as_deref(), Some("0.8.0"));
        assert!(!cache.is_available_for("0.6.0"));
    }

    #[test]
    fn status_cache_round_trips_and_ignores_corrupt_files() {
        let temp_dir =
            std::env::temp_dir().join(format!("logcatx-update-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_dir);
        fs::create_dir_all(&temp_dir).expect("create temp dir");
        let path: PathBuf = temp_dir.join(super::STATUS_CACHE_FILE);

        let mut cache = UpdateStatusCache::default();
        cache.record_check("0.6.0", Some(&candidate("0.7.0")), true);
        write_status_cache(&path, &cache).expect("write cache");
        assert_eq!(load_status_cache(&path), cache);

        fs::write(&path, b"not json").expect("write corrupt cache");
        assert_eq!(load_status_cache(&path), UpdateStatusCache::default());

        let _ = fs::remove_dir_all(&temp_dir);
    }
}
