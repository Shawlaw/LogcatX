//! Signed application updates backed by the DeskFoundry `desktop-updater`
//! contract: a Raw GitHub manifest describes the latest portable ZIP, its
//! Ed25519 signature is verified against the public key compiled into this
//! build, and a helper executable replaces the installation after the app
//! exits. When `LOGCATX_UPDATE_PUBLIC_KEY` is not set at build time the whole
//! feature reports itself as unconfigured instead of contacting the network.

use crate::config::{UpdateProxyConfig, UpdateProxyMode};
use chrono::{Local, Timelike};
use desktop_updater::ApplyRequest;
use desktop_updater::PortableLayout;
use desktop_updater::UpdateCandidate;
use desktop_updater::UpdateConfig;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub const APP_ID: &str = "com.logcatx.app";
pub const CHANNEL: &str = "stable";
pub const DEFAULT_PROXY_TEST_URL: &str = "https://github.com/";
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
const PROXY_TEST_TIMEOUT: Duration = Duration::from_secs(10);

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

pub fn update_config(
    current_version: &str,
    update_proxy: &UpdateProxyConfig,
) -> Result<Option<UpdateConfig>, UpdateProxyValidationError> {
    validate_update_proxy(update_proxy)?;
    let Some(public_key) = public_key() else {
        return Ok(None);
    };
    let mut config = UpdateConfig::new(
        APP_ID,
        CHANNEL,
        current_version,
        MANIFEST_URL,
        SIGNATURE_URL,
        public_key,
    );
    config.proxy_url = update_proxy.custom_url().map(str::to_owned);
    Ok(Some(config))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateProxyValidationError {
    MissingUrl,
    UnsupportedScheme,
    MissingHostOrPort,
    AuthenticationNotSupported,
    InvalidUrl,
}

/// Validates user-entered settings without returning a proxy URL in an error.
/// Credentials are deliberately unsupported: this portable app persists its
/// settings in plain JSON and writes update diagnostics.
pub fn validate_update_proxy(
    update_proxy: &UpdateProxyConfig,
) -> Result<(), UpdateProxyValidationError> {
    if update_proxy.mode == UpdateProxyMode::Automatic {
        return Ok(());
    }

    let url = update_proxy
        .custom_url()
        .ok_or(UpdateProxyValidationError::MissingUrl)?;
    let parsed = reqwest::Url::parse(url).map_err(|_| UpdateProxyValidationError::InvalidUrl)?;
    if !matches!(parsed.scheme(), "http" | "socks5" | "socks5h") {
        return Err(UpdateProxyValidationError::UnsupportedScheme);
    }
    if parsed.host_str().is_none() || parsed.port().is_none() {
        return Err(UpdateProxyValidationError::MissingHostOrPort);
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(UpdateProxyValidationError::AuthenticationNotSupported);
    }
    reqwest::Proxy::all(url).map_err(|_| UpdateProxyValidationError::InvalidUrl)?;
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UpdateConnectionTestResult {
    pub status_code: u16,
    pub elapsed_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateConnectionTestError {
    InvalidTarget,
    InvalidProxy,
    RequestFailed,
    HttpStatus(u16),
}

/// Tests an HTTPS target with the same proxy policy used by update checks.
/// Response content is discarded and network errors are intentionally reduced
/// to safe categories before they reach the UI.
pub fn test_update_connection(
    update_proxy: &UpdateProxyConfig,
    target_url: &str,
) -> Result<UpdateConnectionTestResult, UpdateConnectionTestError> {
    validate_update_proxy(update_proxy).map_err(|_| UpdateConnectionTestError::InvalidProxy)?;
    let target = parse_proxy_test_target(target_url)?;

    let mut builder = reqwest::blocking::Client::builder().timeout(PROXY_TEST_TIMEOUT);
    if let Some(proxy_url) = update_proxy.custom_url() {
        let proxy =
            reqwest::Proxy::all(proxy_url).map_err(|_| UpdateConnectionTestError::InvalidProxy)?;
        builder = builder.proxy(proxy);
    }
    let client = builder
        .build()
        .map_err(|_| UpdateConnectionTestError::InvalidProxy)?;
    let started = Instant::now();
    let response = client
        .get(target)
        .header(reqwest::header::CACHE_CONTROL, "no-cache")
        .send()
        .map_err(|_| UpdateConnectionTestError::RequestFailed)?;
    let status = response.status();
    if !status.is_success() {
        return Err(UpdateConnectionTestError::HttpStatus(status.as_u16()));
    }
    Ok(UpdateConnectionTestResult {
        status_code: status.as_u16(),
        elapsed_ms: started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
    })
}

fn parse_proxy_test_target(target_url: &str) -> Result<reqwest::Url, UpdateConnectionTestError> {
    let target = reqwest::Url::parse(target_url.trim())
        .map_err(|_| UpdateConnectionTestError::InvalidTarget)?;
    if target.scheme() != "https" || target.host_str().is_none() {
        return Err(UpdateConnectionTestError::InvalidTarget);
    }
    Ok(target)
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

const HELPER_COPY_PREFIX: &str = "helper-";
const HELPER_COPY_SUFFIX: &str = ".exe";
const HELPER_CLEANUP_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(250);
const HELPER_CLEANUP_MAX_RETRIES: usize = 40;

/// The updater helper cannot delete its own copied executable on Windows
/// while it is still running, so a `helper-*.exe` survives every apply. The
/// restarted application is expected to sweep those copies after
/// acknowledging the applied update; retries cover the short window until
/// the helper process exits.
pub fn cleanup_helper_copies(updates_dir: &Path) {
    for attempt in 0..=HELPER_CLEANUP_MAX_RETRIES {
        match cleanup_helper_copies_once(updates_dir) {
            Ok(false) | Err(_) => return,
            Ok(true) if attempt == HELPER_CLEANUP_MAX_RETRIES => return,
            Ok(true) => std::thread::sleep(HELPER_CLEANUP_RETRY_DELAY),
        }
    }
}

/// Removes only helper executables copied by `desktop-updater` into the
/// updates directory. Returns whether a currently locked helper should be
/// retried after it exits.
fn cleanup_helper_copies_once(updates_dir: &Path) -> std::io::Result<bool> {
    let entries = match fs::read_dir(updates_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let mut retry = false;
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if !file_name.starts_with(HELPER_COPY_PREFIX) || !file_name.ends_with(HELPER_COPY_SUFFIX) {
            continue;
        }
        match fs::remove_file(entry.path()) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => retry = true,
            Err(_) => {}
        }
    }
    Ok(retry)
}

/// Local demo preview of the update flow (mirrors QuotaBarWin's
/// QBWIN_DEMO_APP_UPDATE). Enabled by `LOGCATX_DEMO_APP_UPDATE=1` in debug
/// builds or with the `update-preview` feature: fakes an available candidate,
/// synthesizes a layout-valid package locally, and exercises the real helper
/// apply path — all without contacting GitHub or persisting update state.
#[cfg(any(debug_assertions, feature = "update-preview"))]
pub mod demo {
    use super::{APP_ID, CHANNEL, RELEASE_REPLACE_FILES};
    use chrono::Local;
    use desktop_updater::{UpdateAsset, UpdateCandidate, UpdateManifest};
    use std::path::{Path, PathBuf};
    use std::{fs, io::Write as _};

    pub const DEMO_ENV: &str = "LOGCATX_DEMO_APP_UPDATE";
    pub const DEMO_VERSION: &str = "9.9.9-demo.1";
    /// Small delay so the simulated check feels like the real network round
    /// trip instead of an instant UI flip.
    pub const DEMO_DELAY: std::time::Duration = std::time::Duration::from_millis(700);

    pub fn requested() -> bool {
        std::env::var(DEMO_ENV)
            .ok()
            .as_deref()
            .is_some_and(is_requested_value)
    }

    pub fn is_requested_value(value: &str) -> bool {
        matches!(value.trim(), "1" | "true" | "TRUE")
    }

    pub fn candidate() -> UpdateCandidate {
        UpdateCandidate {
            manifest: UpdateManifest {
                schema_version: desktop_updater::UPDATE_MANIFEST_SCHEMA_VERSION,
                app_id: APP_ID.to_owned(),
                channel: CHANNEL.to_owned(),
                version: DEMO_VERSION.to_owned(),
                published_at: Local::now().to_rfc3339(),
                target: "windows-x64".to_owned(),
                asset: UpdateAsset {
                    url: "https://github.com/Shawlaw/LogcatX/releases".to_owned(),
                    sha256: "0".repeat(64),
                    size: 1,
                },
                notes_url: Some("https://github.com/Shawlaw/LogcatX/releases".to_owned()),
            },
        }
    }

    /// Builds a local update ZIP from the current installation directory so
    /// "download" and "restart and update" can be exercised without a signed
    /// release. `README.md` gets a visible demo marker appended inside the
    /// archive so a successful apply is observable.
    pub fn build_package(install_dir: &Path, updates_dir: &Path) -> Result<PathBuf, String> {
        let missing: Vec<&str> = RELEASE_REPLACE_FILES
            .iter()
            .filter(|file| !install_dir.join(*file).is_file())
            .copied()
            .collect();
        if !missing.is_empty() {
            return Err(format!(
                "The update demo needs a release-layout directory (missing: {}). \
                 Unzip the packaged release zip and run the demo build from there.",
                missing.join(", ")
            ));
        }

        fs::create_dir_all(updates_dir).map_err(|error| {
            format!(
                "Failed to create demo update directory {}: {error}",
                updates_dir.display()
            )
        })?;
        let package_path = updates_dir.join("logcatx-demo-update.zip");
        let file = fs::File::create(&package_path)
            .map_err(|error| format!("Failed to create demo update package: {error}"))?;
        let mut writer = zip::ZipWriter::new(file);
        let options =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for entry in RELEASE_REPLACE_FILES {
            let bytes = fs::read(install_dir.join(entry))
                .map_err(|error| format!("Failed to read {entry} for the demo package: {error}"))?;
            writer
                .start_file(*entry, options)
                .map_err(|error| format!("Failed to add demo package entry {entry}: {error}"))?;
            write_entry(&mut writer, entry, &bytes)?;
        }
        writer
            .finish()
            .map_err(|error| format!("Failed to finalize demo update package: {error}"))?;
        Ok(package_path)
    }

    fn write_entry(
        writer: &mut zip::ZipWriter<fs::File>,
        entry: &str,
        bytes: &[u8],
    ) -> Result<(), String> {
        writer
            .write_all(bytes)
            .map_err(|error| format!("Failed to write demo package entry {entry}: {error}"))?;
        if entry == "README.md" {
            let marker = format!(
                "\n\n> Demo update applied at {}.\n",
                Local::now().format("%Y-%m-%d %H:%M:%S")
            );
            writer
                .write_all(marker.as_bytes())
                .map_err(|error| format!("Failed to write demo package entry {entry}: {error}"))?;
        }
        Ok(())
    }
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
        DEFAULT_PROXY_TEST_URL, UpdateConnectionTestError, UpdateProxyValidationError,
        UpdateStatusCache, automatic_check_is_due, load_status_cache, parse_proxy_test_target,
        today_local, validate_update_proxy, write_status_cache,
    };
    use crate::config::{UpdateProxyConfig, UpdateProxyMode};
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
    fn proxy_test_target_defaults_to_github_and_requires_https() {
        let default_target =
            parse_proxy_test_target(DEFAULT_PROXY_TEST_URL).expect("default GitHub test target");
        assert_eq!(default_target.as_str(), DEFAULT_PROXY_TEST_URL);
        assert_eq!(
            parse_proxy_test_target("http://github.com/"),
            Err(UpdateConnectionTestError::InvalidTarget)
        );
        assert_eq!(
            parse_proxy_test_target("not a url"),
            Err(UpdateConnectionTestError::InvalidTarget)
        );
    }

    #[test]
    fn update_proxy_validation_accepts_automatic_http_and_socks5h() {
        assert!(validate_update_proxy(&UpdateProxyConfig::default()).is_ok());
        assert!(
            validate_update_proxy(&UpdateProxyConfig {
                mode: UpdateProxyMode::Custom,
                url: "http://127.0.0.1:7890".to_owned(),
            })
            .is_ok()
        );
        assert!(
            validate_update_proxy(&UpdateProxyConfig {
                mode: UpdateProxyMode::Custom,
                url: "socks5h://127.0.0.1:7890".to_owned(),
            })
            .is_ok()
        );
    }

    #[test]
    fn socks5h_update_proxy_can_build_a_client() {
        let proxy = UpdateProxyConfig {
            mode: UpdateProxyMode::Custom,
            url: "socks5h://127.0.0.1:7890".to_owned(),
        };
        validate_update_proxy(&proxy).expect("validate SOCKS5H proxy");
        let client_proxy =
            reqwest::Proxy::all(proxy.custom_url().unwrap()).expect("construct SOCKS5H proxy");
        reqwest::blocking::Client::builder()
            .proxy(client_proxy)
            .build()
            .expect("build client with SOCKS5H proxy");
    }

    #[test]
    fn update_proxy_validation_rejects_unsafe_or_incomplete_urls() {
        let invalid = |url: &str| UpdateProxyConfig {
            mode: UpdateProxyMode::Custom,
            url: url.to_owned(),
        };

        assert_eq!(
            validate_update_proxy(&invalid("")),
            Err(UpdateProxyValidationError::MissingUrl)
        );
        assert_eq!(
            validate_update_proxy(&invalid("https://127.0.0.1:7890")),
            Err(UpdateProxyValidationError::UnsupportedScheme)
        );
        assert_eq!(
            validate_update_proxy(&invalid("http://127.0.0.1")),
            Err(UpdateProxyValidationError::MissingHostOrPort)
        );
        assert_eq!(
            validate_update_proxy(&invalid("http://user:secret@127.0.0.1:7890")),
            Err(UpdateProxyValidationError::AuthenticationNotSupported)
        );
    }

    #[test]
    fn helper_cleanup_removes_only_helper_copies() {
        let temp_dir = std::env::temp_dir().join(format!(
            "logcatx-helper-cleanup-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&temp_dir);
        let updates_dir = temp_dir.join("updates");
        fs::create_dir_all(&updates_dir).expect("create updates dir");
        fs::write(updates_dir.join("helper-1785769510724498000.exe"), b"x")
            .expect("write helper copy");
        fs::write(updates_dir.join("0.7.0-0123456789abcdef0123.zip"), b"pkg")
            .expect("write cached package");
        fs::write(updates_dir.join("helper-notes.txt"), b"keep me")
            .expect("write similarly named non-executable");
        fs::create_dir_all(updates_dir.join("staging-1")).expect("create staging dir");

        super::cleanup_helper_copies(&updates_dir);

        assert!(!updates_dir.join("helper-1785769510724498000.exe").exists());
        assert!(updates_dir.join("0.7.0-0123456789abcdef0123.zip").exists());
        assert!(updates_dir.join("helper-notes.txt").exists());
        assert!(updates_dir.join("staging-1").exists());

        // A missing updates directory is not an error and stops the loop.
        assert!(!super::cleanup_helper_copies_once(&temp_dir.join("absent")).unwrap());

        let _ = fs::remove_dir_all(&temp_dir);
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

    #[cfg(any(debug_assertions, feature = "update-preview"))]
    #[test]
    fn demo_env_values_are_recognized() {
        use super::demo;
        assert!(demo::is_requested_value("1"));
        assert!(demo::is_requested_value(" true "));
        assert!(demo::is_requested_value("TRUE"));
        assert!(!demo::is_requested_value("0"));
        assert!(!demo::is_requested_value("yes"));
        assert!(!demo::is_requested_value(""));
    }

    #[cfg(any(debug_assertions, feature = "update-preview"))]
    #[test]
    fn demo_candidate_describes_stable_channel() {
        let candidate = super::demo::candidate();
        assert_eq!(candidate.version(), super::demo::DEMO_VERSION);
        assert_eq!(candidate.manifest.app_id, super::APP_ID);
        assert_eq!(candidate.manifest.channel, super::CHANNEL);
        assert_eq!(candidate.manifest.target, "windows-x64");
        assert!(candidate.manifest.asset.url.starts_with("https://"));
    }

    #[cfg(any(debug_assertions, feature = "update-preview"))]
    #[test]
    fn demo_package_matches_release_layout() {
        use std::io::Read as _;

        let temp_dir =
            std::env::temp_dir().join(format!("logcatx-demo-pkg-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&temp_dir);
        let install_dir = temp_dir.join("install");
        fs::create_dir_all(install_dir.join("icons")).expect("create install layout");
        for file in super::RELEASE_REPLACE_FILES {
            fs::write(install_dir.join(file), b"demo file").expect("write layout file");
        }

        let updates_dir = temp_dir.join("updates");
        let package =
            super::demo::build_package(&install_dir, &updates_dir).expect("build demo package");

        // The package must satisfy the exact validator the release pipeline
        // and the updater helper use.
        let layout = super::apply_request(&install_dir).layout;
        desktop_updater::validate_release_archive(&package, &layout)
            .expect("demo package matches release layout");

        let file = fs::File::open(&package).expect("open demo package");
        let mut archive = zip::ZipArchive::new(file).expect("read demo package");
        let mut readme = String::new();
        archive
            .by_name("README.md")
            .expect("README entry exists")
            .read_to_string(&mut readme)
            .expect("read README entry");
        assert!(readme.contains("Demo update applied"));

        let _ = fs::remove_dir_all(&temp_dir);
    }
}
