//! Windows Job Object 薄封装：把 goose 进程树关进内核 Job（W5）。
//!
//! 动机（`docs/reviews/AGENT_IMPLEMENTATION_REPORT.md` §5.1/§5.3）：仅 kill 直接子进程或
//! `taskkill /T /F` 在硬崩溃/Drop 顺序异常时仍可能留下 goose 派生的孙进程。
//!
//! 为什么不用 `process-wrap 10.0.1`：它内部创建 Job 且**不暴露句柄**，也没有
//! `ActiveProcessLimit` / `ProcessMemoryLimit` 的 API（只支持
//! `KILL_ON_JOB_CLOSE`，且 wrapper 以 `kill_on_drop = false` 创建），无法满足
//! 本模块的限制要求；这里只借鉴它验证过的「CREATE_SUSPENDED → assign →
//! 枚举线程 ResumeThread」步骤，用 `windows 0.62.2` 写薄封装。
//!
//! 契约：
//! - [`spawn`] 返回前子进程**必定**已加入 Job 并 resume；任一步失败都会
//!   `TerminateProcess` 挂起的子进程并返回错误，绝不放出未进 Job 的进程。
//! - [`Job::terminate`] 对应 `TerminateJobObject`；[`Job`] 的 `Drop` 关闭句柄，
//!   `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` 在最后一个句柄关闭时兜底杀树。
//! - 全部是同步 Win32 调用，只允许在工作线程使用（UI 线程不做阻塞 I/O）。
//!
//! 注意：若 duthub 自身被父进程放进 Job（部分 CI/启动器），子进程会继承该
//! Job；Windows 8+ 允许把已是 Job 成员的进程再分配到新 Job（嵌套 Job）。
//! 仍然失败时不降级 resume，而是终止挂起的子进程并返回错误（fail closed）。
//!
//! Application containment: [`contain_current_process`] puts the whole duthub
//! process tree into a job whose only limit is `KILL_ON_JOB_CLOSE`, so a crash
//! or a forced kill (Task Manager "End task") still reaps descendants such as
//! ffmpeg/ffplay and releases the capture device. The handle is leaked on
//! purpose: the kernel closes it when the process dies, which is exactly the
//! kill trigger. Goose keeps its own job ([`Job::create`]) and that job nests
//! under the containment job (Windows 8+ supports job hierarchies); if the
//! containment assignment fails, startup is fail-open — warn and continue,
//! because graceful exits are still covered by `Capture::Drop`.

use std::process::{Child, Command};

/// Job 内同时存活的进程上限。goose 本身 + 工具子进程（shell/grep 等）远小于
/// 该值；超限由内核拒绝创建，防止失控 agent 无限 fork（约 100–200 行封装的
/// 主要目的之一）。
pub const JOB_ACTIVE_PROCESS_LIMIT: u32 = 16;
/// 单进程提交内存上限（4 GiB）。`ProcessMemoryLimit` 按进程计，不是 Job 总量；
/// 超限时内核终止该进程，Host 会看到非 0 退出码并转 Failed。
pub const JOB_PROCESS_MEMORY_LIMIT: usize = 4 * 1024 * 1024 * 1024;
/// Job 终止时的退出码（与 taskkill 的失败口径一致：只要非 0 即异常，但用户
/// 主动停止时 Host 不把退出码当失败）。
pub const JOB_KILL_EXIT_CODE: u32 = 1;

/// 启动失败时的收尾：终止（可能仍挂起的）直接子进程，并清掉 Job 内可能
/// 已存在的其它进程。直接子进程可能尚未加入 Job（assign 失败），所以
/// `child.kill()` 必须无条件执行，不能只依赖 `TerminateJobObject`。
pub fn abort_spawn(child: &mut Child, job: Option<&Job>) {
    let _ = child.kill();
    if let Some(job) = job {
        let _ = job.terminate(JOB_KILL_EXIT_CODE);
    }
    let _ = child.wait();
}

#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::io;
    use std::mem::size_of;
    use std::os::windows::io::AsRawHandle;
    use std::os::windows::process::CommandExt;

    use windows::Win32::Foundation::{CloseHandle, ERROR_NO_MORE_FILES, HANDLE};
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
    };
    use windows::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, IsProcessInJob,
        JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
        TerminateJobObject,
    };
    use windows::Win32::System::Threading::{
        CREATE_NO_WINDOW, CREATE_SUSPENDED, GetCurrentProcess, OpenThread, ResumeThread,
        THREAD_SUSPEND_RESUME,
    };
    use windows::core::{BOOL, HRESULT};

    use super::{Child, Command, JOB_ACTIVE_PROCESS_LIMIT, JOB_PROCESS_MEMORY_LIMIT};

    /// 拥有一个 Job 句柄；`Drop` 关闭句柄（KILL_ON_JOB_CLOSE 兜底杀树）。
    #[derive(Debug)]
    pub struct Job {
        handle: HANDLE,
    }

    // SAFETY: Job 只持有内核对象句柄（值语义的 HANDLE），不指向进程内内存；
    // 句柄唯一所有权由 Job 自己持有，跨线程移动/共享不产生别名。
    unsafe impl Send for Job {}
    // SAFETY: 同上。共享访问只做 TerminateJobObject/查询，均为线程安全的
    // 内核调用；关闭句柄只发生在唯一所有者的 Drop 中。
    unsafe impl Sync for Job {}

    /// `QueryInformationJobObject` 实测到的生效限制（测试/诊断用）。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct JobLimits {
        pub limit_flags: u32,
        pub active_process_limit: u32,
        pub process_memory_limit: usize,
    }

    impl Job {
        /// 创建 Job 并写入限制：
        /// - `KILL_ON_JOB_CLOSE`：句柄关闭时终止所有关联进程；
        /// - `ActiveProcessLimit = 16`：防止无限 fork；
        /// - `ProcessMemoryLimit = 4 GiB`：单个 goose/工具进程内存上限。
        ///
        /// 不设 `JobMemoryLimit`（Job 总量）：goose 的 MCP 子进程数量很小，
        /// 进程级上限已足够，且避免正常峰值被 Job 总量误杀。
        pub fn create() -> io::Result<Self> {
            // SAFETY: 无属性、无名字地创建匿名 Job；返回的句柄由 Job 独占并 close。
            let handle = unsafe { CreateJobObjectW(None, None) }.map_err(io::Error::other)?;
            let job = Self { handle };

            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
                | JOB_OBJECT_LIMIT_ACTIVE_PROCESS
                | JOB_OBJECT_LIMIT_PROCESS_MEMORY;
            info.BasicLimitInformation.ActiveProcessLimit = JOB_ACTIVE_PROCESS_LIMIT;
            info.ProcessMemoryLimit = JOB_PROCESS_MEMORY_LIMIT;

            // SAFETY: info 是合法的 JOBOBJECT_EXTENDED_LIMIT_INFORMATION，
            // 长度按 size_of 传入；句柄来自 CreateJobObjectW 且仍有效。
            unsafe {
                SetInformationJobObject(
                    job.handle,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const c_void,
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            }
            .map_err(io::Error::other)?;
            Ok(job)
        }

        /// Create a job whose only limit is `KILL_ON_JOB_CLOSE`.
        ///
        /// Used for application-wide containment: it must not cap the app's
        /// process or memory budget, because the desktop spawns ffmpeg, ffplay,
        /// agent hosts and shell helpers concurrently. Closing the last handle
        /// terminates every member; nothing is limited while the handle lives.
        pub fn create_containment() -> io::Result<Self> {
            // SAFETY: anonymous job object; the returned handle is owned by `Job`.
            let handle = unsafe { CreateJobObjectW(None, None) }.map_err(io::Error::other)?;
            let job = Self { handle };

            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;

            // SAFETY: `info` is a valid JOBOBJECT_EXTENDED_LIMIT_INFORMATION and
            // its exact size is passed; `job.handle` is a live job handle.
            unsafe {
                SetInformationJobObject(
                    job.handle,
                    JobObjectExtendedLimitInformation,
                    &info as *const _ as *const c_void,
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            }
            .map_err(io::Error::other)?;
            Ok(job)
        }

        /// Assign an already-running child process to this job.
        ///
        /// The kernel state is queried back (`IsProcessInJob`) instead of
        /// trusting the call, the same way [`assign_and_resume`] guards spawn.
        pub fn assign_child(&self, child: &Child) -> io::Result<()> {
            let process = HANDLE(child.as_raw_handle());
            // SAFETY: `process` is owned by `child` and `self.handle` by `self`;
            // both handles stay valid for the duration of the call.
            unsafe { AssignProcessToJobObject(self.handle, process) }.map_err(|error| {
                io::Error::other(format!("AssignProcessToJobObject 失败: {error}"))
            })?;

            let mut in_job = BOOL(0);
            // SAFETY: `in_job` is a valid BOOL out-parameter; handles still valid.
            unsafe { IsProcessInJob(process, Some(self.handle), &mut in_job) }
                .map_err(|error| io::Error::other(format!("IsProcessInJob 失败: {error}")))?;
            if !in_job.as_bool() {
                return Err(io::Error::other("子进程未加入 Job"));
            }
            Ok(())
        }

        /// Terminate Job 内全部进程（`TerminateJobObject`）。不关闭句柄。
        pub fn terminate(&self, exit_code: u32) -> io::Result<()> {
            // SAFETY: 句柄由 self 独占且有效；TerminateJobObject 是线程安全的内核调用。
            unsafe { TerminateJobObject(self.handle, exit_code) }.map_err(io::Error::other)
        }

        /// 查询内核中实际生效的限制（供测试/诊断；失败说明 Job 句柄已失效）。
        pub fn query_limits(&self) -> io::Result<JobLimits> {
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            // SAFETY: info 可写且长度匹配；句柄由 self 独占且有效。
            unsafe {
                QueryInformationJobObject(
                    Some(self.handle),
                    JobObjectExtendedLimitInformation,
                    &mut info as *mut _ as *mut c_void,
                    size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
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

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: 句柄由 self 唯一拥有，关闭后不再使用。
            // 最后一个 Job 句柄关闭时，KILL_ON_JOB_CLOSE 终止残留进程。
            unsafe {
                let _ = CloseHandle(self.handle);
            }
        }
    }

    /// 启动子进程并保证它在返回前已加入 Job：
    /// `CREATE_SUSPENDED` → `AssignProcessToJobObject` → `ResumeThread`。
    ///
    /// 任一步失败：`TerminateProcess` 挂起的子进程、终止并关闭 Job，返回错误。
    pub fn spawn(command: &mut Command) -> io::Result<(Child, Option<Job>)> {
        let job = Job::create()?;
        // std 的 creation_flags 是覆盖式；这里同时带上原来的 CREATE_NO_WINDOW。
        command.creation_flags(CREATE_NO_WINDOW.0 | CREATE_SUSPENDED.0);
        let mut child = command.spawn()?;
        if let Err(error) = assign_and_resume(&child, &job) {
            super::abort_spawn(&mut child, Some(&job));
            return Err(error);
        }
        Ok((child, Some(job)))
    }

    /// Put the current process into a containment job (see
    /// [`Job::create_containment`]) and leak the handle until process exit.
    ///
    /// Every descendant automatically joins the job, so a crash or a forced
    /// kill (which closes our handles) terminates ffmpeg/ffplay and releases
    /// the capture device. The job is a child of any job that already contains
    /// us (Windows 8+ job hierarchy) — the goose job then nests under this one.
    /// The handle is not inheritable (`CreateJobObjectW` without attributes),
    /// so descendants cannot keep the kill trigger alive.
    ///
    /// Callers treat an error as fail-open: warn and keep starting, since a
    /// graceful exit is still covered by `Capture::Drop`.
    pub fn contain_current_process() -> io::Result<()> {
        let job = Job::create_containment()?;
        // SAFETY: `GetCurrentProcess` returns a pseudo-handle for the calling
        // process with full access; it must not be closed. The job handle is
        // live and owned by `job`.
        unsafe { AssignProcessToJobObject(job.handle, GetCurrentProcess()) }
            .map_err(|error| io::Error::other(format!("AssignProcessToJobObject 失败: {error}")))?;
        // Leak on purpose: dropping would close the handle immediately and kill
        // the process. The kernel closes it on termination, arming the kill.
        std::mem::forget(job);
        Ok(())
    }

    /// 加入 Job 后 resume 主线程；任何一步失败都返回错误，由调用方杀进程。
    fn assign_and_resume(child: &Child, job: &Job) -> io::Result<()> {
        // SAFETY: 子进程句柄由 Child 持有且有效；Job 句柄由 job 独占。
        unsafe { AssignProcessToJobObject(job.handle, HANDLE(child.as_raw_handle())) }
            .map_err(|error| io::Error::other(format!("AssignProcessToJobObject 失败: {error}")))?;

        // 显式复核：绝不 resume 一个未进 Job 的进程。
        let mut in_job = BOOL(0);
        // SAFETY: 同上；in_job 是合法的 BOOL 输出。
        unsafe { IsProcessInJob(HANDLE(child.as_raw_handle()), Some(job.handle), &mut in_job) }
            .map_err(|error| io::Error::other(format!("IsProcessInJob 失败: {error}")))?;
        if !in_job.as_bool() {
            return Err(io::Error::other("进程未加入 Job，拒绝 ResumeThread"));
        }

        resume_primary_thread(child.id())
            .map_err(|error| io::Error::other(format!("ResumeThread 失败: {error}")))
    }

    /// `CREATE_SUSPENDED` 创建的进程只有一个线程（std 已关闭主线程句柄），
    /// 通过 Toolhelp 快照找到它并 ResumeThread；找不到任何可 resume 的线程
    /// 视为失败（上层会杀进程）。
    fn resume_primary_thread(pid: u32) -> io::Result<()> {
        // SAFETY: 只读的系统快照；句柄由 Snapshot 守卫关闭。
        let snapshot =
            unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) }.map_err(io::Error::other)?;
        let snapshot = SnapshotHandle(snapshot);

        let mut entry = THREADENTRY32 {
            dwSize: size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        // SAFETY: entry.dwSize 已按结构大小初始化，快照句柄有效。
        unsafe { Thread32First(snapshot.0, &mut entry) }.map_err(io::Error::other)?;

        let mut resumed = false;
        loop {
            if entry.th32OwnerProcessID == pid {
                // SAFETY: 线程 ID 来自系统快照；访问权限只申请 SUSPEND_RESUME。
                let thread =
                    unsafe { OpenThread(THREAD_SUSPEND_RESUME, false, entry.th32ThreadID) }
                        .map_err(io::Error::other)?;
                let thread = SnapshotHandle(thread);
                // SAFETY: 线程句柄有效；ResumeThread 返回上一次挂起计数。
                let previous = unsafe { ResumeThread(thread.0) };
                if previous == u32::MAX {
                    return Err(io::Error::last_os_error());
                }
                if previous > 0 {
                    resumed = true;
                }
            }
            // SAFETY: 快照与 entry 均有效；枚举结束返回 ERROR_NO_MORE_FILES。
            match unsafe { Thread32Next(snapshot.0, &mut entry) } {
                Ok(()) => {}
                Err(error) if error.code() == HRESULT::from_win32(ERROR_NO_MORE_FILES.0) => break,
                Err(error) => return Err(io::Error::other(error)),
            }
        }
        if resumed {
            Ok(())
        } else {
            Err(io::Error::other("未找到主线程，进程保持挂起"))
        }
    }

    /// 局部 RAII：关闭 Toolhelp/线程句柄。
    struct SnapshotHandle(HANDLE);

    impl Drop for SnapshotHandle {
        fn drop(&mut self) {
            // SAFETY: 句柄由本守卫唯一拥有。
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use std::io;

    use super::{Child, Command};

    /// 非 Windows 平台没有 Job Object；恒为 `None`，由 `kill_process_tree` 回退。
    #[derive(Debug)]
    pub enum Job {}

    impl Job {
        /// Job Objects are a Windows kernel feature; a containment job cannot
        /// be created on other platforms.
        pub fn create_containment() -> io::Result<Self> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Job Objects are Windows-only",
            ))
        }

        pub fn assign_child(&self, _child: &Child) -> io::Result<()> {
            match *self {}
        }

        pub fn terminate(&self, _exit_code: u32) -> io::Result<()> {
            match *self {}
        }

        pub fn query_limits(&self) -> io::Result<JobLimits> {
            match *self {}
        }
    }

    /// 与 Windows 版本同名的限制快照（非 Windows 永远不会返回）。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct JobLimits {
        pub limit_flags: u32,
        pub active_process_limit: u32,
        pub process_memory_limit: usize,
    }

    pub fn spawn(command: &mut Command) -> io::Result<(Child, Option<Job>)> {
        Ok((command.spawn()?, None))
    }

    /// No-op: process containment relies on Job Objects, which only exist on
    /// Windows; elsewhere the OS has no equivalent handle-close trigger.
    pub fn contain_current_process() -> io::Result<()> {
        Ok(())
    }
}

pub use imp::{Job, JobLimits, contain_current_process, spawn};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limits_constants_are_documented_bounds() {
        assert_eq!(JOB_ACTIVE_PROCESS_LIMIT, 16);
        assert_eq!(JOB_PROCESS_MEMORY_LIMIT, 4 * 1024 * 1024 * 1024);
        assert_ne!(JOB_KILL_EXIT_CODE, 0);
    }

    #[cfg(windows)]
    #[test]
    fn created_job_reports_configured_limits() {
        let job = Job::create().expect("CreateJobObjectW 失败");
        let limits = job.query_limits().expect("QueryInformationJobObject 失败");
        assert_eq!(limits.active_process_limit, JOB_ACTIVE_PROCESS_LIMIT);
        assert_eq!(limits.process_memory_limit, JOB_PROCESS_MEMORY_LIMIT);
        // JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE = 0x2000
        // JOB_OBJECT_LIMIT_ACTIVE_PROCESS   = 0x0008
        // JOB_OBJECT_LIMIT_PROCESS_MEMORY   = 0x0100
        assert_eq!(limits.limit_flags & 0x2000, 0x2000, "{limits:?}");
        assert_eq!(limits.limit_flags & 0x0008, 0x0008, "{limits:?}");
        assert_eq!(limits.limit_flags & 0x0100, 0x0100, "{limits:?}");
    }

    /// Long-running child (`ping` loops for ~29 s) used by the containment
    /// tests. The test process itself is never assigned to a job: closing the
    /// handle would kill `cargo test`.
    #[cfg(windows)]
    fn spawn_ping() -> std::process::Child {
        use std::os::windows::process::CommandExt;
        use std::process::{Command, Stdio};

        use windows::Win32::System::Threading::CREATE_NO_WINDOW;

        Command::new("cmd")
            .args(["/c", "ping", "-n", "30", "127.0.0.1"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW.0)
            .spawn()
            .expect("spawn cmd /c ping 失败")
    }

    /// Wait for the child to exit; kill it and fail the test once `timeout`
    /// elapses, so a hung child can never leak past the test run.
    #[cfg(windows)]
    fn wait_for_exit_or_kill(
        child: &mut std::process::Child,
        timeout: std::time::Duration,
        what: &str,
    ) {
        use std::time::Instant;

        let deadline = Instant::now() + timeout;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("{what}：{timeout:?} 内子进程仍在运行");
                }
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("try_wait 失败: {error}");
                }
            }
        }
    }

    /// The containment job must not throttle the whole application: the kernel
    /// has to report `KILL_ON_JOB_CLOSE` as the only limit.
    #[cfg(windows)]
    #[test]
    fn containment_job_only_kills_on_close() {
        let job = Job::create_containment().expect("CreateJobObjectW 失败");
        let limits = job.query_limits().expect("QueryInformationJobObject 失败");
        // JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE = 0x2000, and nothing else.
        assert_eq!(limits.limit_flags, 0x2000, "{limits:?}");
        assert_eq!(limits.active_process_limit, 0, "{limits:?}");
        assert_eq!(limits.process_memory_limit, 0, "{limits:?}");
    }

    /// Closing the last handle (no `TerminateJobObject`) is what a crash or a
    /// forced kill does, and it must reap the assigned child.
    #[cfg(windows)]
    #[test]
    fn closing_containment_handle_kills_assigned_child() {
        use std::time::Duration;

        let job = Job::create_containment().expect("CreateJobObjectW 失败");
        let mut child = spawn_ping();
        if let Err(error) = job.assign_child(&child) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("assign_child 失败: {error}");
        }
        // Close the only handle: KILL_ON_JOB_CLOSE terminates the whole job.
        drop(job);
        wait_for_exit_or_kill(&mut child, Duration::from_secs(10), "关闭 Job 句柄后");
    }

    /// goose keeps its own job and that job has to nest under the containment
    /// job (Windows 8+); the kernel accepts a second assignment for a process
    /// that is already a job member as long as a valid hierarchy is formed.
    #[cfg(windows)]
    #[test]
    fn process_already_in_a_job_can_join_a_nested_job() {
        use std::time::Duration;

        let containment = Job::create_containment().expect("CreateJobObjectW 失败");
        let mut child = spawn_ping();
        let inner = Job::create().expect("CreateJobObjectW 失败");
        if let Err(error) = containment.assign_child(&child) {
            let _ = child.kill();
            let _ = child.wait();
            panic!("control job assign 失败: {error}");
        }
        match inner.assign_child(&child) {
            Ok(()) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("嵌套 Job 分配失败（Windows 8+ 应支持）: {error}");
            }
        }
        // Terminating the parent job kills the child job and its processes.
        drop(containment);
        wait_for_exit_or_kill(&mut child, Duration::from_secs(10), "关闭父 Job 句柄后");
        drop(inner);
    }
}
