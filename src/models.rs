use crate::managed_child::ManagedChild;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForegroundAppAction {
    Inspect,
    ForceStop,
    ClearData,
    Uninstall,
}

#[derive(Debug)]
pub enum AppEvent {
    DevicesRefreshed(Result<Vec<DeviceInfo>, String>),
    DevicesPolled(Result<Vec<DeviceInfo>, String>),
    LogSizeRefreshed(Result<u64, String>),
    DeviceConnectFinished {
        target: String,
        result: Result<String, String>,
    },
    DeviceDisconnectFinished {
        serial: String,
        result: Result<String, String>,
    },
    AdbServerRestartFinished(Result<String, String>),
    DeviceDropFinished {
        serial: String,
        result: Result<String, String>,
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
        exit_code: Option<i32>,
        error: Option<String>,
    },
    CleanupFinished(Result<(), String>),
}

#[cfg(test)]
mod tests {
    use super::{DeviceEntry, DeviceInfo, DeviceRunState};

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
