//! Application-wide Windows Job Object containment.
//!
//! [`contain_current_process`] puts the entire `duthub` process tree into an
//! anonymous Job whose only limit is `KILL_ON_JOB_CLOSE`. When the desktop app
//! crashes or is force-killed (Task Manager "End task"), the kernel closes the
//! leaked handle and reaps every descendant (`ffmpeg`, `ffplay`, capture
//! helpers, shell children), releasing hardware devices deterministically.
//!
//! Per-Agent container isolation and tree termination are handled natively by
//! `mxc-sdk` inside `duthub-sandbox`; Windows 8+ nested Job Objects allow MXC
//! container jobs to nest cleanly beneath this application containment job.

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::io;
    use std::mem::size_of;
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    #[cfg(test)]
    use std::process::Child;

    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
        SetInformationJobObject,
    };
    #[cfg(test)]
    use windows::Win32::System::JobObjects::{IsProcessInJob, QueryInformationJobObject};
    use windows::Win32::System::Threading::GetCurrentProcess;
    #[cfg(test)]
    use windows::core::BOOL;

    /// Owns an anonymous Windows Job handle; closing the last handle triggers
    /// `KILL_ON_JOB_CLOSE`.
    #[derive(Debug)]
    pub struct Job {
        handle: OwnedHandle,
    }

    /// Limits queried back from the kernel (tests/diagnostics).
    #[cfg(test)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct JobLimits {
        pub limit_flags: u32,
        pub active_process_limit: u32,
        pub process_memory_limit: usize,
    }

    impl Job {
        fn raw_handle(&self) -> HANDLE {
            HANDLE(self.handle.as_raw_handle())
        }

        /// Create a Job whose only limit is `KILL_ON_JOB_CLOSE`.
        ///
        /// Deliberately omits process/memory caps so desktop child processes
        /// (`ffmpeg`, `ffplay`, Agent hosts, shells) are never throttled.
        pub fn create_containment() -> io::Result<Self> {
            const INFO_SIZE: u32 = {
                assert!(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() <= u32::MAX as usize);
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() is statically asserted <= u32::MAX"
                )]
                {
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32
                }
            };
            // SAFETY: Anonymous non-inheritable Job Object; owned by `Job`.
            let handle = unsafe { CreateJobObjectW(None, None) }.map_err(io::Error::other)?;
            // SAFETY: CreateJobObjectW succeeded and transferred a valid handle
            // to this caller. OwnedHandle takes sole responsibility for closing it.
            let handle = unsafe { OwnedHandle::from_raw_handle(handle.0) };
            let job = Self { handle };

            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

            // SAFETY: `info` is a properly aligned, initialized
            // `JOBOBJECT_EXTENDED_LIMIT_INFORMATION` matching `INFO_SIZE`,
            // `job.handle` is a live owned Job Object handle, and the kernel
            // only reads `info` for the duration of the synchronous call.
            unsafe {
                SetInformationJobObject(
                    job.raw_handle(),
                    JobObjectExtendedLimitInformation,
                    std::ptr::from_ref(&info).cast::<c_void>(),
                    INFO_SIZE,
                )
            }
            .map_err(io::Error::other)?;
            Ok(job)
        }

        #[cfg(test)]
        pub fn assign_child(&self, child: &Child) -> io::Result<()> {
            let process = HANDLE(child.as_raw_handle());
            // SAFETY: Both handles stay valid for the duration of the call.
            unsafe { AssignProcessToJobObject(self.raw_handle(), process) }.map_err(|error| {
                io::Error::other(format!("AssignProcessToJobObject failed: {error}"))
            })?;
            let mut in_job = BOOL(0);
            // SAFETY: `in_job` is a valid out-parameter; handles remain valid.
            unsafe {
                IsProcessInJob(
                    process,
                    Some(self.raw_handle()),
                    std::ptr::from_mut(&mut in_job),
                )
            }
            .map_err(|error| io::Error::other(format!("IsProcessInJob failed: {error}")))?;
            if !in_job.as_bool() {
                return Err(io::Error::other("child process did not join job"));
            }
            Ok(())
        }

        #[cfg(test)]
        pub fn query_limits(&self) -> io::Result<JobLimits> {
            const INFO_SIZE: u32 = {
                assert!(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() <= u32::MAX as usize);
                #[expect(
                    clippy::cast_possible_truncation,
                    reason = "size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() is statically asserted <= u32::MAX"
                )]
                {
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32
                }
            };
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            // SAFETY: `info` is uniquely borrowed, initialized, and sized
            // accurately by `INFO_SIZE`; `self.handle` is a live Job Object
            // handle owned by `&self` for the duration of the call.
            unsafe {
                QueryInformationJobObject(
                    Some(self.raw_handle()),
                    JobObjectExtendedLimitInformation,
                    std::ptr::from_mut(&mut info).cast::<c_void>(),
                    INFO_SIZE,
                    None,
                )
            }
            .map_err(io::Error::other)?;
            Ok(JobLimits {
                limit_flags: info.BasicLimitInformation.LimitFlags.0,
                active_process_limit: info.BasicLimitInformation.ActiveProcessLimit,
                process_memory_limit: info.ProcessMemoryLimit,
            })
        }
    }

    /// Put the current process into an application-wide `KILL_ON_JOB_CLOSE` job
    /// and intentionally leak the handle until process exit.
    ///
    /// Callers treat failure as fail-open (warn and continue), because normal
    /// exits still clean up capture children via `Drop`.
    pub fn contain_current_process() -> io::Result<()> {
        let job = Job::create_containment()?;
        // SAFETY: `GetCurrentProcess` returns a pseudo-handle valid for the
        // calling process; `job.handle` is owned and live.
        unsafe { AssignProcessToJobObject(job.raw_handle(), GetCurrentProcess()) }.map_err(
            |error| io::Error::other(format!("AssignProcessToJobObject failed: {error}")),
        )?;
        // Leak intentionally so the kernel closes the handle on process death.
        std::mem::forget(job);
        Ok(())
    }
}

#[cfg(not(windows))]
mod imp {
    use std::io;

    /// No-op off Windows: handle-close job containment is Windows-specific.
    pub fn contain_current_process() -> io::Result<()> {
        Ok(())
    }
}

pub use imp::contain_current_process;

#[cfg(all(test, windows))]
mod tests {
    use std::os::windows::process::CommandExt;
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    use windows::Win32::System::Threading::CREATE_NO_WINDOW;

    use super::imp::Job;

    fn spawn_ping() -> Child {
        Command::new("cmd")
            .args(["/c", "ping", "-n", "30", "127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW.0)
            .spawn()
            .expect("spawn cmd /c ping")
    }

    fn wait_for_exit_or_kill(child: &mut Child, timeout: Duration, what: &str) {
        let deadline = Instant::now() + timeout;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("{what}: child still alive after {timeout:?}");
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("try_wait failed: {error}");
                }
            }
        }
    }

    #[test]
    fn containment_job_only_kills_on_close() {
        let job = Job::create_containment().expect("CreateJobObjectW");
        let limits = job.query_limits().expect("QueryInformationJobObject");
        // JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE = 0x2000, and nothing else.
        assert_eq!(limits.limit_flags, 0x2000, "{limits:?}");
        assert_eq!(limits.active_process_limit, 0, "{limits:?}");
        assert_eq!(limits.process_memory_limit, 0, "{limits:?}");
    }

    #[test]
    fn closing_containment_handle_kills_assigned_child() {
        let job = Job::create_containment().expect("CreateJobObjectW");
        let mut child = spawn_ping();
        if let Err(error) = job.assign_child(&child) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("assign_child failed: {error}");
        }
        drop(job);
        wait_for_exit_or_kill(
            &mut child,
            Duration::from_secs(10),
            "after closing job handle",
        );
    }
}
