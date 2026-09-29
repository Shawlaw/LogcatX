use std::{
    fmt, io,
    process::{Child, ExitStatus},
};

/// Wrapper around a child process with a uniform kill/reap API.
///
/// Windows job objects decide which constructor a caller must use:
///
/// - [`ManagedChild::new`] assigns a kill-on-close job, so the child (and
///   anything it spawns) dies when this handle is dropped. Use it only for
///   children we own for their whole life, such as logcat or push/pull.
/// - [`ManagedChild::new_detached`] assigns no job. Any adb client may be
///   the one to (re)spawn the shared adb daemon, and a daemon spawned by a
///   job member inherits that job — a kill-on-close job would then kill the
///   daemon (and every wireless connection with it) the moment the client
///   is reaped. Short-lived commands therefore must stay job-free.
pub struct ManagedChild {
    child: Child,
    #[cfg(target_os = "windows")]
    _job: Option<windows_job::KillOnDropJob>,
}

impl ManagedChild {
    pub fn new(child: Child) -> Self {
        #[cfg(target_os = "windows")]
        {
            let job = match windows_job::KillOnDropJob::assign_child(&child) {
                Ok(job) => Some(job),
                Err(err) => {
                    log::warn!("Failed to attach collector process to Windows job object: {err}");
                    None
                }
            };

            Self { child, _job: job }
        }

        #[cfg(not(target_os = "windows"))]
        {
            Self { child }
        }
    }

    /// Job-free variant for short-lived commands that may spawn the shared
    /// adb daemon (see the type-level docs).
    pub fn new_detached(child: Child) -> Self {
        Self {
            child,
            #[cfg(target_os = "windows")]
            _job: None,
        }
    }

    pub fn kill(&mut self) -> io::Result<()> {
        self.child.kill()
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    pub fn wait(&mut self) -> io::Result<ExitStatus> {
        self.child.wait()
    }
}

impl fmt::Debug for ManagedChild {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ManagedChild").finish_non_exhaustive()
    }
}

#[cfg(target_os = "windows")]
mod windows_job {
    use std::{io, mem::size_of, os::windows::io::AsRawHandle, process::Child, ptr::null};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE},
        System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        },
    };

    pub struct KillOnDropJob {
        handle: HANDLE,
    }

    unsafe impl Send for KillOnDropJob {}

    impl KillOnDropJob {
        pub fn assign_child(child: &Child) -> io::Result<Self> {
            let job = unsafe { CreateJobObjectW(null(), null()) };
            if job.is_null() {
                return Err(io::Error::last_os_error());
            }

            let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

            let configured = unsafe {
                SetInformationJobObject(
                    job,
                    JobObjectExtendedLimitInformation,
                    &limits as *const _ as *const _,
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if configured == 0 {
                let err = io::Error::last_os_error();
                unsafe {
                    CloseHandle(job);
                }
                return Err(err);
            }

            let assigned =
                unsafe { AssignProcessToJobObject(job, child.as_raw_handle() as HANDLE) };
            if assigned == 0 {
                let err = io::Error::last_os_error();
                unsafe {
                    CloseHandle(job);
                }
                return Err(err);
            }

            Ok(Self { handle: job })
        }
    }

    impl Drop for KillOnDropJob {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.handle);
            }
        }
    }
}
