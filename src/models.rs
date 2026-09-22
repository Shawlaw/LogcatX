use crate::{fs_utils, managed_child::ManagedChild};
use desktop_updater::{DownloadedUpdate, UpdateCandidate};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::SystemTime,
};

pub type SharedChild = Arc<Mutex<Option<ManagedChild>>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    pub serial: String,
    pub identity_key: String,
    pub state: String,
    pub android_version: Option<String>,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
}

#[derive(Clone, Debug)]
pub struct DeviceEntry {
    pub info: DeviceInfo,
    pub transport_serials: Vec<String>,
    pub run_state: DeviceRunState,
    pub output_path: Option<PathBuf>,
    pub started_at: Option<SystemTime>,
    pub child: Option<SharedChild>,
}

impl DeviceEntry {
    pub fn new(info: DeviceInfo) -> Self {
        let primary_serial = info.serial.clone();
        Self {
            info,
            transport_serials: vec![primary_serial],
            run_state: DeviceRunState::Idle,
            output_path: None,
            started_at: None,
            child: None,
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(
            self.run_state,
            DeviceRunState::Starting | DeviceRunState::Running | DeviceRunState::Stopping
        )
    }

    pub fn matches_serial(&self, serial_or_identity: &str) -> bool {
        self.info.identity_key == serial_or_identity
            || self.info.serial == serial_or_identity
            || self
                .transport_serials
                .iter()
                .any(|serial| serial == serial_or_identity)
    }

    /// State transition when a logcat session's process handle arrives.
    pub fn session_spawned(&mut self, output_path: PathBuf, child: SharedChild) {
        self.run_state = DeviceRunState::Running;
        self.output_path = Some(output_path);
        self.child = Some(child);
        self.started_at = Some(SystemTime::now());
    }

    /// State transition when a logcat session ends (stopped, exited, or
    /// failed to spawn). The latest-log path is deliberately RETAINED: the
    /// table's latest-log column and the copy-path action read it after the
    /// session is over. Only live process state is cleared. This invariant
    /// regressed once (0.9.0 trial bug 1) and is pinned by tests.
    pub fn session_ended(&mut self, error: Option<String>) {
        self.child = None;
        self.started_at = None;
        self.run_state = match error {
            Some(err) => DeviceRunState::Error(err),
            None => DeviceRunState::Idle,
        };
    }
}

#[derive(Clone, Debug)]
pub enum DeviceRunState {
    Idle,
    Starting,
    Running,
    Stopping,
    Error(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForegroundApp {
    pub package_name: String,
    pub activity_name: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Screenshot {
    pub width: usize,
    pub height: usize,
    pub rgba_pixels: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForegroundAppAction {
    Inspect,
    ForceStop,
    ClearData,
    Uninstall,
}

/// 拖放 APK 安装结果的逐文件明细，供运行消息展示。
/// 文件推送不再在此出现：它们进入 TransferManager 队列（PRD §6.4）。
#[derive(Clone, Debug, Default)]
pub struct DropOutcome {
    /// 安装成功的本地 APK 路径。
    pub installed: Vec<String>,
}

impl DropOutcome {
    pub fn success_count(&self) -> usize {
        self.installed.len()
    }
}

/// Pending batch deletion confirmation for the Files page (PRD §6.6).
#[derive(Clone, Debug)]
pub struct FilesDeletePending {
    pub names: Vec<String>,
    pub contains_directory: bool,
}

#[derive(Debug)]
pub enum AppEvent {
    /// Discovery responses carry the generation they were spawned with;
    /// anything older than the app's current generation is discarded
    /// (PRD §18: a late reply must never overwrite newer state).
    DevicesRefreshed {
        generation: u64,
        result: Result<crate::adb::DiscoveryOutcome, String>,
    },
    DevicesPolled {
        generation: u64,
        result: Result<crate::adb::DiscoveryOutcome, String>,
    },
    /// Files page listing response; stale generations are discarded.
    FilesListed {
        generation: u64,
        result: Result<Vec<crate::remote_fs::RemoteEntry>, String>,
    },
    /// Files page mutation (mkdir/rename/move/delete) finished.
    FilesOpFinished {
        generation: u64,
        result: Result<(), String>,
    },
    /// Release-notes fetch finished; stale generations are discarded
    /// (PRD §30: notes never block the update flow).
    UpdateNotesFetched {
        generation: u64,
        result: Result<String, String>,
    },
    LogStorageRefreshed(Result<fs_utils::LogStorageReport, String>),
    CleanupPreviewed {
        request_id: u64,
        filter: fs_utils::CleanupFilter,
        result: Result<fs_utils::CleanupPreview, String>,
    },
    ScreenshotFinished {
        serial: String,
        result: Result<Screenshot, String>,
    },
    ScrcpyAppsLoaded {
        device_id: String,
        result: Result<Vec<String>, String>,
    },
    DeviceConnectFinished {
        target: String,
        result: Result<String, crate::wireless::Failure>,
    },
    DeviceDisconnectFinished {
        serial: String,
        result: Result<String, String>,
    },
    AdbServerRestartFinished(Result<String, String>),
    DeviceDropFinished {
        serial: String,
        result: Result<DropOutcome, String>,
    },
    ForegroundAppResolved {
        serial: String,
        action: ForegroundAppAction,
        result: Result<ForegroundApp, String>,
    },
    ForegroundAppActionFinished {
        serial: String,
        action: ForegroundAppAction,
        app: ForegroundApp,
        result: Result<String, String>,
    },
    CollectionSpawned {
        serial: String,
        output_path: PathBuf,
        child: SharedChild,
    },
    CollectionEnded {
        serial: String,
        /// Session log path so the active-log registry can unregister it
        /// precisely (PRD §24).
        output_path: Option<PathBuf>,
        exit_code: Option<i32>,
        error: Option<String>,
    },
    CleanupFinished(Result<fs_utils::CleanupOutcome, String>),
    UpdateCheckFinished {
        automatic: bool,
        result: Result<Option<UpdateCandidate>, String>,
    },
    UpdateConnectionTestFinished(
        Result<
            crate::updater::UpdateConnectionTestResult,
            crate::updater::UpdateConnectionTestError,
        >,
    ),
    UpdateDownloadFinished(Result<DownloadedUpdate, String>),
    UpdateApplyStarted(Result<(), String>),
}

#[cfg(test)]
mod tests {
    use super::{DeviceEntry, DeviceInfo, DeviceRunState};
    use std::path::{Path, PathBuf};
    use std::time::SystemTime;

    #[test]
    fn device_entry_matches_identity_primary_and_secondary_serials() {
        let mut entry = DeviceEntry {
            info: DeviceInfo {
                serial: "ZY223JQ9K".to_owned(),
                identity_key: "ZY223JQ9K".to_owned(),
                state: "device".to_owned(),
                android_version: None,
                manufacturer: None,
                model: None,
            },
            transport_serials: vec!["ZY223JQ9K".to_owned(), "192.168.0.8:5555".to_owned()],
            run_state: DeviceRunState::Idle,
            output_path: None,
            started_at: None,
            child: None,
        };

        assert!(entry.matches_serial("ZY223JQ9K"));
        assert!(entry.matches_serial("192.168.0.8:5555"));

        entry.info.identity_key = "ABC123".to_owned();
        assert!(entry.matches_serial("ABC123"));
    }

    fn sample_entry() -> DeviceEntry {
        DeviceEntry {
            info: DeviceInfo {
                serial: "ZY223JQ9K".to_owned(),
                identity_key: "ZY223JQ9K".to_owned(),
                state: "device".to_owned(),
                android_version: None,
                manufacturer: None,
                model: None,
            },
            transport_serials: vec!["ZY223JQ9K".to_owned()],
            run_state: DeviceRunState::Idle,
            output_path: None,
            started_at: None,
            child: None,
        }
    }

    /// Trial bug 1 regression pin: a session that ran and stopped must keep
    /// its latest-log path — the copy-path action and the table column read
    /// it after the session is over. (0.9.0 RC cleared it on session end.)
    #[test]
    fn session_end_retains_latest_log_path() {
        let mut entry = sample_entry();
        entry.output_path = Some(PathBuf::from(r"C:\logs\ZY223JQ9K-20260923.log"));
        entry.run_state = DeviceRunState::Running;
        entry.started_at = Some(SystemTime::now());

        entry.session_ended(None);

        assert_eq!(
            entry.output_path.as_deref(),
            Some(Path::new(r"C:\logs\ZY223JQ9K-20260923.log")),
            "latest-log path must survive session end"
        );
        assert!(matches!(entry.run_state, DeviceRunState::Idle));
        assert!(entry.child.is_none());
        assert!(entry.started_at.is_none());
        assert!(!entry.is_active());
    }

    #[test]
    fn session_end_maps_error_state_and_keeps_path() {
        let mut entry = sample_entry();
        entry.output_path = Some(PathBuf::from("/logs/x.log"));
        entry.run_state = DeviceRunState::Stopping;

        entry.session_ended(Some("collector crashed".to_owned()));

        assert!(matches!(
            &entry.run_state,
            DeviceRunState::Error(message) if message == "collector crashed"
        ));
        assert_eq!(entry.output_path.as_deref(), Some(Path::new("/logs/x.log")));
    }

    #[test]
    fn session_spawned_transitions_to_running_with_path_and_time() {
        let mut entry = sample_entry();
        let before = SystemTime::now();
        entry.session_spawned(
            PathBuf::from("/logs/new.log"),
            std::sync::Arc::new(std::sync::Mutex::new(None)),
        );

        assert!(matches!(entry.run_state, DeviceRunState::Running));
        assert_eq!(
            entry.output_path.as_deref(),
            Some(Path::new("/logs/new.log"))
        );
        assert!(entry.started_at.is_some_and(|started| started >= before));
        assert!(entry.is_active());
    }
}

#[derive(Clone, Debug)]
pub struct StatusMessage {
    pub text: String,
    pub is_error: bool,
    pub timestamp: String,
}

impl StatusMessage {
    pub fn info(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
            timestamp: chrono::Local::now().format("%H:%M:%S").to_string(),
        }
    }

    pub fn error(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
            timestamp: chrono::Local::now().format("%H:%M:%S").to_string(),
        }
    }
}
