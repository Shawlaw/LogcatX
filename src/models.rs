use std::{
    path::PathBuf,
    process::Child,
    sync::{Arc, Mutex},
    time::SystemTime,
};

pub type SharedChild = Arc<Mutex<Option<Child>>>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    pub serial: String,
    pub state: String,
    pub android_version: Option<String>,
}

#[derive(Clone, Debug)]
pub struct DeviceEntry {
    pub info: DeviceInfo,
    pub run_state: DeviceRunState,
    pub output_path: Option<PathBuf>,
    pub started_at: Option<SystemTime>,
    pub child: Option<SharedChild>,
}

impl DeviceEntry {
    pub fn new(info: DeviceInfo) -> Self {
        Self {
            info,
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
}

#[derive(Clone, Debug)]
pub enum DeviceRunState {
    Idle,
    Starting,
    Running,
    Stopping,
    Error(String),
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
