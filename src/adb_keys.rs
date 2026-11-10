//! Local adb authentication key (`adbkey` / `adbkey.pub`) discovery and generation.
//!
//! Windows platform-tools adb resolves the user key directory as
//! `%USERPROFILE%\.android` and — measured against current platform-tools on
//! Windows — ignores `ANDROID_USER_HOME`, `ANDROID_SDK_HOME` and `HOME` when
//! loading and generating the key. We mirror that exact resolution so the key
//! we show is always the one adb actually authenticates with; honoring those
//! variables would display a key the local adb never uses.
use std::{
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

use crate::adb_executor::AdbExecutor;

/// `adb keygen` is a client-side command; 30s is generous even on cold starts.
const KEYGEN_TIMEOUT: Duration = Duration::from_secs(30);
/// A real RSA-2048 adb public key is ~400 bytes; anything past this cap is
/// treated as suspicious rather than displayed.
const PUBLIC_KEY_MAX_BYTES: u64 = 8 * 1024;

/// Directory holding the local adb user keys (`%USERPROFILE%\.android`).
pub fn android_dir() -> PathBuf {
    // `directories` is already a dependency; `BaseDirs::home_dir()` resolves
    // USERPROFILE on Windows and $HOME elsewhere, matching adb.
    directories::BaseDirs::new()
        .map(|dirs| dirs.home_dir().to_path_buf())
        .unwrap_or_default()
        .join(".android")
}

pub fn private_key_path() -> PathBuf {
    android_dir().join("adbkey")
}

pub fn public_key_path() -> PathBuf {
    android_dir().join("adbkey.pub")
}

/// `adb keygen FILE` derives the public key path as `FILE.pub`; mirror that.
fn pub_path_for(private_key: &Path) -> PathBuf {
    let mut name = private_key.as_os_str().to_os_string();
    name.push(".pub");
    PathBuf::from(name)
}

/// Read the public key at `pub_path`. `Ok(None)` when the file is missing.
pub fn read_public_key_at(pub_path: &Path) -> Result<Option<String>, String> {
    let display = crate::fs_utils::display_path(pub_path);
    let metadata = match fs::metadata(pub_path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(format!(
                "Failed to read the ADB public key at {display}: {err}"
            ));
        }
    };
    if !metadata.is_file() {
        return Err(format!("ADB public key path {display} is not a file"));
    }
    if metadata.len() > PUBLIC_KEY_MAX_BYTES {
        return Err(format!(
            "ADB public key at {display} is unexpectedly large ({} bytes)",
            metadata.len()
        ));
    }
    let content = fs::read_to_string(pub_path)
        .map_err(|err| format!("Failed to read the ADB public key at {display}: {err}"))?;
    let content = content.trim();
    if content.is_empty() {
        return Err(format!("ADB public key file {display} is empty"));
    }
    Ok(Some(content.to_owned()))
}

/// Read the local adb public key from `%USERPROFILE%\.android\adbkey.pub`.
/// `Ok(None)` when no key has been generated yet.
pub fn read_public_key() -> Result<Option<String>, String> {
    read_public_key_at(&public_key_path())
}

/// Generate a fresh key pair via `adb keygen` only when neither the private
/// nor the public key exists; never overwrites an existing key (regenerating
/// would invalidate every authorization granted to the old key).
/// `Ok(None)` means a key already existed and nothing was generated.
pub fn generate_key_at(adb_path: &str, private_key: &Path) -> Result<Option<String>, String> {
    let pub_key = pub_path_for(private_key);
    if private_key.exists() || pub_key.exists() {
        return Ok(None);
    }
    if let Some(parent) = private_key.parent() {
        fs::create_dir_all(parent).map_err(|err| {
            format!(
                "Failed to create the ADB key directory {}: {err}",
                crate::fs_utils::display_path(parent)
            )
        })?;
    }
    let key_arg = private_key.to_string_lossy().into_owned();
    log::info!("Generating local ADB key pair via `{adb_path} keygen {key_arg}`");
    let output = AdbExecutor::new(adb_path)
        .execute_with_timeout(&["keygen", &key_arg], KEYGEN_TIMEOUT)
        .map_err(|err| format!("Failed to run `{adb_path} keygen`: {err}"))?;
    if !output.success() {
        let stderr = output.stderr_lossy();
        let stdout = output.stdout_lossy();
        let detail = if stderr.trim().is_empty() {
            stdout.trim()
        } else {
            stderr.trim()
        };
        let detail = if detail.is_empty() {
            "unknown error"
        } else {
            detail
        };
        return Err(format!("Failed to generate the ADB key pair: {detail}"));
    }
    let public_key = read_public_key_at(&pub_key)?.ok_or_else(|| {
        format!(
            "`{adb_path} keygen` reported success but no public key was created at {}",
            crate::fs_utils::display_path(&pub_key)
        )
    })?;
    Ok(Some(public_key))
}

/// Generate the local adb key pair at the default location when missing.
/// `Ok(Some(public_key))` on generation, `Ok(None)` when a key already exists.
pub fn generate_key_if_missing(adb_path: &str) -> Result<Option<String>, String> {
    generate_key_at(adb_path, &private_key_path())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn key_paths_live_under_android_dir_with_adb_names() {
        assert!(private_key_path().ends_with("adbkey"));
        assert!(public_key_path().ends_with("adbkey.pub"));
        assert_eq!(
            private_key_path().parent(),
            public_key_path().parent(),
            "both keys must share the .android directory"
        );
        assert!(android_dir().ends_with(".android"));
    }

    #[test]
    fn pub_path_appends_suffix_exactly_like_adb_keygen() {
        assert_eq!(
            pub_path_for(Path::new(r"C:\Users\u\.android\adbkey")),
            PathBuf::from(r"C:\Users\u\.android\adbkey.pub")
        );
        // `with_extension` would mangle dotted names; suffix append must not.
        assert_eq!(
            pub_path_for(Path::new("/tmp/my.key.v2")),
            PathBuf::from("/tmp/my.key.v2.pub")
        );
    }

    #[test]
    fn read_public_key_missing_file_is_none() {
        let dir = temp_dir();
        assert_eq!(
            read_public_key_at(&dir.path().join("adbkey.pub")).unwrap(),
            None
        );
    }

    #[test]
    fn read_public_key_trims_trailing_whitespace() {
        let dir = temp_dir();
        let path = dir.path().join("adbkey.pub");
        std::fs::write(&path, "QAAAAFake-key user@host\r\n\n").unwrap();
        assert_eq!(
            read_public_key_at(&path).unwrap().as_deref(),
            Some("QAAAAFake-key user@host")
        );
    }

    #[test]
    fn read_public_key_rejects_empty_file() {
        let dir = temp_dir();
        let path = dir.path().join("adbkey.pub");
        std::fs::write(&path, "  \n").unwrap();
        let err = read_public_key_at(&path).unwrap_err();
        assert!(err.contains("empty"), "unexpected error: {err}");
    }

    #[test]
    fn generate_key_writes_pair_and_reads_public_key() {
        let fixture = crate::wireless::tests::Fixture::new("");
        let dir = temp_dir();
        let private_key = dir.path().join("nested").join("adbkey");
        let generated = generate_key_at(&fixture.adb, &private_key).unwrap();
        assert_eq!(
            generated.as_deref(),
            Some("QAAAAFIXTURE-PUBKEY user@fixture")
        );
        assert!(private_key.exists(), "private key file must be created");
        assert!(pub_path_for(&private_key).exists());
        let calls = std::fs::read_to_string(fixture.dir.path().join("calls.log")).unwrap();
        assert!(
            calls.contains(&format!("keygen {}", private_key.display())),
            "expected keygen invocation in calls.log: {calls}"
        );
    }

    #[test]
    fn generate_key_never_touches_existing_key() {
        let fixture = crate::wireless::tests::Fixture::new("");
        let dir = temp_dir();
        let private_key = dir.path().join("adbkey");
        std::fs::write(&private_key, "existing private key").unwrap();
        std::fs::write(pub_path_for(&private_key), "existing user@host").unwrap();
        assert_eq!(generate_key_at(&fixture.adb, &private_key).unwrap(), None);
        assert_eq!(
            std::fs::read_to_string(&private_key).unwrap(),
            "existing private key",
            "existing private key must not be overwritten"
        );
        let calls =
            std::fs::read_to_string(fixture.dir.path().join("calls.log")).unwrap_or_default();
        assert!(
            !calls.contains("keygen"),
            "adb keygen must not be invoked when a key exists: {calls}"
        );
    }

    #[test]
    fn generate_key_reports_adb_failure() {
        let fixture = crate::wireless::tests::Fixture::new("keygen=fail");
        let dir = temp_dir();
        let private_key = dir.path().join("adbkey");
        let err = generate_key_at(&fixture.adb, &private_key).unwrap_err();
        assert!(err.contains("keygen refused"), "unexpected error: {err}");
        assert!(!private_key.exists());
    }
}
