//! Task identity and lifecycle for non-transfer background work (PRD §19-§20).
//!
//! Replaces application-global `*_in_progress` bools with per-device,
//! per-kind task records so one device's operation never blocks another's.
//! The registry is owned by the UI thread; worker threads report through the
//! existing mpsc event channel, exactly like logcat output today.
//!
//! Transfers keep their own richer model (TransferManager, PRD §10) but share
//! the same state vocabulary.

use std::collections::HashMap;

pub type TaskId = u64;

/// Categories of tracked background work (PRD §20).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TaskKind {
    Screenshot,
    Install,
    ForegroundApp,
    Package,
    Shell,
    DeviceRefresh,
    Update,
}

impl TaskKind {
    pub fn label(&self) -> &'static str {
        match self {
            TaskKind::Screenshot => "screenshot",
            TaskKind::Install => "install",
            TaskKind::ForegroundApp => "foreground-app",
            TaskKind::Package => "package",
            TaskKind::Shell => "shell",
            TaskKind::DeviceRefresh => "device-refresh",
            TaskKind::Update => "update",
        }
    }
}

/// Lifecycle states with guarded transitions (PRD §44).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskState {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl TaskState {
    /// Legal transitions (PRD §44):
    ///
    /// - Queued → Running, Queued → Cancelled
    /// - Running → Completed, Running → Failed, Running → Cancelled
    /// - Failed → Queued (retry)
    ///
    /// Everything else — including anything leaving Completed or Cancelled,
    /// or skipping Queued → Running — is rejected.
    pub fn can_transition_to(self, next: TaskState) -> bool {
        use TaskState::*;
        matches!(
            (self, next),
            (Queued, Running)
                | (Queued, Cancelled)
                | (Running, Completed)
                | (Running, Failed)
                | (Running, Cancelled)
                | (Failed, Queued)
        )
    }
}

#[derive(Clone, Debug)]
pub struct TaskStatus {
    pub id: TaskId,
    pub device: Option<String>,
    pub kind: TaskKind,
    pub state: TaskState,
    pub error: Option<String>,
}

impl TaskStatus {
    pub fn is_active(&self) -> bool {
        matches!(self.state, TaskState::Queued | TaskState::Running)
    }
}

#[derive(Debug)]
pub enum TaskError {
    UnknownTask(TaskId),
    InvalidTransition {
        id: TaskId,
        from: TaskState,
        to: TaskState,
    },
}

impl std::fmt::Display for TaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TaskError::UnknownTask(id) => write!(f, "unknown task {id}"),
            TaskError::InvalidTransition { id, from, to } => {
                write!(f, "task {id}: illegal transition {from:?} -> {to:?}")
            }
        }
    }
}

/// Registry of tracked tasks. Owned by one thread (the UI); no locking.
#[derive(Default)]
pub struct TaskManager {
    next_id: TaskId,
    tasks: HashMap<TaskId, TaskStatus>,
}

impl TaskManager {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a queued task and return its id.
    pub fn register(&mut self, device: Option<String>, kind: TaskKind) -> TaskId {
        self.next_id += 1;
        let id = self.next_id;
        self.tasks.insert(
            id,
            TaskStatus {
                id,
                device,
                kind,
                state: TaskState::Queued,
                error: None,
            },
        );
        id
    }

    pub fn get(&self, id: TaskId) -> Option<&TaskStatus> {
        self.tasks.get(&id)
    }

    pub fn transition(&mut self, id: TaskId, next: TaskState) -> Result<(), TaskError> {
        let status = self.tasks.get_mut(&id).ok_or(TaskError::UnknownTask(id))?;
        if !status.state.can_transition_to(next) {
            return Err(TaskError::InvalidTransition {
                id,
                from: status.state,
                to: next,
            });
        }
        status.state = next;
        Ok(())
    }

    pub fn start(&mut self, id: TaskId) -> Result<(), TaskError> {
        self.transition(id, TaskState::Running)
    }

    pub fn complete(&mut self, id: TaskId) -> Result<(), TaskError> {
        self.transition(id, TaskState::Completed)?;
        if let Some(status) = self.tasks.get_mut(&id) {
            status.error = None;
        }
        Ok(())
    }

    pub fn fail(&mut self, id: TaskId, error: String) -> Result<(), TaskError> {
        self.transition(id, TaskState::Failed)?;
        if let Some(status) = self.tasks.get_mut(&id) {
            status.error = Some(error);
        }
        Ok(())
    }

    pub fn cancel(&mut self, id: TaskId) -> Result<(), TaskError> {
        self.transition(id, TaskState::Cancelled)
    }

    /// Failed → Queued so the task can run again (PRD §44 retry path).
    pub fn retry(&mut self, id: TaskId) -> Result<(), TaskError> {
        self.transition(id, TaskState::Queued)?;
        if let Some(status) = self.tasks.get_mut(&id) {
            status.error = None;
        }
        Ok(())
    }

    /// Whether a device currently has an active (queued/running) task of a
    /// kind — the per-device replacement for the old global bools.
    pub fn is_active_for_device(&self, device: &str, kind: TaskKind) -> bool {
        self.tasks.values().any(|task| {
            task.kind == kind && task.is_active() && task.device.as_deref() == Some(device)
        })
    }

    /// All finished tasks (completed/failed/cancelled), oldest first.
    pub fn finished(&self) -> Vec<&TaskStatus> {
        let mut finished: Vec<_> = self
            .tasks
            .values()
            .filter(|task| !task.is_active())
            .collect();
        finished.sort_by_key(|task| task.id);
        finished
    }

    /// Drop finished tasks from the registry.
    pub fn clear_finished(&mut self) {
        self.tasks.retain(|_, task| task.is_active());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager_with_screenshot() -> (TaskManager, TaskId) {
        let mut manager = TaskManager::new();
        let id = manager.register(Some("serial-1".into()), TaskKind::Screenshot);
        (manager, id)
    }

    #[test]
    fn registered_task_starts_queued() {
        let (manager, id) = manager_with_screenshot();
        let status = manager.get(id).unwrap();
        assert_eq!(status.state, TaskState::Queued);
        assert_eq!(status.kind, TaskKind::Screenshot);
        assert_eq!(status.device.as_deref(), Some("serial-1"));
    }

    #[test]
    fn ids_are_monotonic() {
        let mut manager = TaskManager::new();
        let first = manager.register(None, TaskKind::Update);
        let second = manager.register(None, TaskKind::Shell);
        assert!(second > first);
    }

    #[test]
    fn legal_path_queued_running_completed() {
        let mut manager = TaskManager::new();
        let id = manager.register(None, TaskKind::Shell);
        manager.start(id).unwrap();
        manager.complete(id).unwrap();
        assert_eq!(manager.get(id).unwrap().state, TaskState::Completed);
    }

    #[test]
    fn legal_path_queued_cancelled_and_running_cancelled() {
        let mut manager = TaskManager::new();
        let queued = manager.register(None, TaskKind::Shell);
        manager.cancel(queued).unwrap();

        let running = manager.register(None, TaskKind::Shell);
        manager.start(running).unwrap();
        manager.cancel(running).unwrap();
    }

    #[test]
    fn legal_path_running_failed_then_retry_to_queued() {
        let mut manager = TaskManager::new();
        let id = manager.register(Some("dev".into()), TaskKind::Install);
        manager.start(id).unwrap();
        manager.fail(id, "device offline".into()).unwrap();
        assert_eq!(
            manager.get(id).unwrap().error.as_deref(),
            Some("device offline")
        );

        manager.retry(id).unwrap();
        assert_eq!(manager.get(id).unwrap().state, TaskState::Queued);
        assert!(manager.get(id).unwrap().error.is_none());
        // The retried task can run to completion.
        manager.start(id).unwrap();
        manager.complete(id).unwrap();
    }

    #[test]
    fn illegal_transitions_are_rejected() {
        let mut manager = TaskManager::new();

        // Queued cannot skip to Completed/Failed.
        let skip = manager.register(None, TaskKind::Shell);
        assert!(manager.complete(skip).is_err());
        assert!(manager.fail(skip, "x".into()).is_err());

        // Running cannot go back to Queued.
        let running = manager.register(None, TaskKind::Shell);
        manager.start(running).unwrap();
        assert!(manager.retry(running).is_err());

        // Completed is terminal.
        let done = manager.register(None, TaskKind::Shell);
        manager.start(done).unwrap();
        manager.complete(done).unwrap();
        assert!(manager.start(done).is_err());
        assert!(manager.cancel(done).is_err());
        assert!(manager.fail(done, "x".into()).is_err());
        assert!(manager.retry(done).is_err());

        // Cancelled is terminal.
        let cancelled = manager.register(None, TaskKind::Shell);
        manager.cancel(cancelled).unwrap();
        assert!(manager.start(cancelled).is_err());
        assert!(manager.retry(cancelled).is_err());

        // Failed can only retry, not run directly or cancel.
        let failed = manager.register(None, TaskKind::Shell);
        manager.start(failed).unwrap();
        manager.fail(failed, "x".into()).unwrap();
        assert!(manager.complete(failed).is_err());
        assert!(manager.cancel(failed).is_err());
        assert!(manager.start(failed).is_err());

        // Unknown ids error instead of panicking.
        assert!(matches!(
            manager.start(9999),
            Err(TaskError::UnknownTask(9999))
        ));
    }

    #[test]
    fn activity_is_per_device_and_kind() {
        let mut manager = TaskManager::new();
        let a = manager.register(Some("device-a".into()), TaskKind::Screenshot);
        manager.start(a).unwrap();
        let b = manager.register(Some("device-b".into()), TaskKind::Screenshot);
        manager.start(b).unwrap();

        assert!(manager.is_active_for_device("device-a", TaskKind::Screenshot));
        assert!(manager.is_active_for_device("device-b", TaskKind::Screenshot));
        // Different kind on the same device is independent.
        assert!(!manager.is_active_for_device("device-a", TaskKind::Shell));
        // Finishing device-a must not clear device-b (PRD §19).
        manager.complete(a).unwrap();
        assert!(!manager.is_active_for_device("device-a", TaskKind::Screenshot));
        assert!(manager.is_active_for_device("device-b", TaskKind::Screenshot));
    }

    #[test]
    fn finished_listing_and_cleanup() {
        let mut manager = TaskManager::new();
        let done = manager.register(None, TaskKind::Shell);
        manager.start(done).unwrap();
        manager.complete(done).unwrap();
        let failed = manager.register(None, TaskKind::Shell);
        manager.start(failed).unwrap();
        manager.fail(failed, "boom".into()).unwrap();
        let active = manager.register(None, TaskKind::Shell);
        manager.start(active).unwrap();

        assert_eq!(manager.finished().len(), 2);
        manager.clear_finished();
        assert!(manager.get(done).is_none());
        assert!(manager.get(failed).is_none());
        assert!(manager.get(active).is_some());
    }
}
