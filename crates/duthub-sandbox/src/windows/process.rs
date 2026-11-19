//! Process creation, Job Object limits and output collection.
//!
//! One launch is: job limits → pipes → attribute list (handle list, job list,
//! and for scripts the zero-capability AppContainer) → `CreateProcess*` with
//! `CREATE_SUSPENDED` → **token read-back** → `ResumeThread` → bounded read of
//! stdout/stderr → wait with timeout → `TerminateJobObject` (the whole tree) →
//! exit code. If any step before `ResumeThread` fails, the child is terminated
//! while still suspended and no unsandboxed fallback is attempted.

use std::ffi::{OsStr, OsString, c_void};
use std::io::{Read, Write};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::FromRawHandle;
use std::path::Path;
use std::sync::mpsc;

use windows_sys::Win32::Foundation::{
    GetLastError, HANDLE, HANDLE_FLAG_INHERIT, STILL_ACTIVE, SetHandleInformation, WAIT_OBJECT_0,
    WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::{
    GetTokenInformation, IsTokenRestricted, SECURITY_ATTRIBUTES, SECURITY_CAPABILITIES,
    TOKEN_QUERY, TokenAppContainerSid, TokenHasRestrictions, TokenIsAppContainer,
};
use windows_sys::Win32::System::JobObjects::{
    CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    TerminateJobObject,
};
use windows_sys::Win32::System::Pipes::CreatePipe;
use windows_sys::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW,
    CreateProcessW, EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess,
    InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST, OpenProcessToken,
    PROCESS_INFORMATION, ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess,
    UpdateProcThreadAttribute, WaitForSingleObject,
};

use super::{CellIdentity, OwnedHandle, OwnedSid, error_text, path_wide, token};
use crate::policy::{CellSpec, ExecRequest, ExecResult, SandboxError, SandboxPolicy, SpawnRequest};
use crate::status::Capability;
use crate::stream::{SandboxLimits, SandboxedProcess};

const PROC_THREAD_ATTRIBUTE_HANDLE_LIST: usize = 0x0002_0002;
const PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES: usize = 0x0002_0009;
const PROC_THREAD_ATTRIBUTE_JOB_LIST: usize = 0x0002_000D;
/// How long to keep reading pipes after the process tree has been terminated.
const DRAIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Run one command under the cell's restricted token (the Agent host path).
pub(crate) fn launch_restricted(
    cell: &CellSpec,
    policy: &SandboxPolicy,
    request: &ExecRequest,
    identity: &CellIdentity,
) -> Result<ExecResult, SandboxError> {
    let token = restricted_token(identity)?;
    run(
        Some(token.0),
        None,
        Expectation::Restricted {
            capability: identity.primary(),
            user: identity.account(),
        },
        cell,
        policy,
        request,
    )
}

/// Run one command inside the cell's zero-capability AppContainer (scripts).
///
/// With a provisioned identity the container is created *from the sandbox
/// account's logon token*, so the child's user SID is the sandbox account and
/// reads are separated from the host user. Without one the host token is used
/// (write/network isolation only).
pub(crate) fn launch_container(
    cell: &CellSpec,
    policy: &SandboxPolicy,
    request: &ExecRequest,
    identity: &CellIdentity,
) -> Result<ExecResult, SandboxError> {
    let base = container_base_token(identity)?;
    run(
        base.as_ref().map(|handle| handle.0),
        Some(identity.primary()),
        Expectation::Container {
            container: identity.primary(),
            user: identity.account(),
        },
        cell,
        policy,
        request,
    )
}

/// Spawn a long-lived process under the cell's restricted token.
pub(crate) fn spawn_streaming_restricted(
    cell: &CellSpec,
    policy: &SandboxPolicy,
    request: &SpawnRequest,
    identity: &CellIdentity,
) -> Result<SandboxedProcess, SandboxError> {
    let token = restricted_token(identity)?;
    spawn_streaming(
        Some(token.0),
        None,
        Expectation::Restricted {
            capability: identity.primary(),
            user: identity.account(),
        },
        cell,
        policy,
        request,
    )
}

/// Spawn a long-lived process inside the cell's AppContainer.
pub(crate) fn spawn_streaming_container(
    cell: &CellSpec,
    policy: &SandboxPolicy,
    request: &SpawnRequest,
    identity: &CellIdentity,
) -> Result<SandboxedProcess, SandboxError> {
    let base = container_base_token(identity)?;
    spawn_streaming(
        base.as_ref().map(|handle| handle.0),
        Some(identity.primary()),
        Expectation::Container {
            container: identity.primary(),
            user: identity.account(),
        },
        cell,
        policy,
        request,
    )
}

/// The restricted token for the Agent host path.
fn restricted_token(identity: &CellIdentity) -> Result<OwnedHandle, SandboxError> {
    match identity.identity() {
        // A dedicated account makes the child a different principal, which is
        // what separates its reads from the host's files (ADR-12).
        Some(credentials) => {
            token::restricted_for_identity(credentials, identity.primary()).map(|(token, _)| token)
        }
        None => token::restricted_for_current_user(identity.primary()),
    }
}

/// The base token an AppContainer is derived from: the sandbox account's logon
/// token when provisioned, otherwise none (the host token).
fn container_base_token(identity: &CellIdentity) -> Result<Option<OwnedHandle>, SandboxError> {
    match identity.identity() {
        Some(credentials) => Ok(Some(token::logon_identity(credentials)?.0)),
        None => Ok(None),
    }
}

/// Start one sandboxed process and keep its pipes open.
///
/// The token read-back happens before `ResumeThread`, exactly as in [`run`]; a
/// failure terminates the suspended child and returns an error, so no
/// unsandboxed process ever becomes observable.
fn spawn_streaming(
    token_handle: Option<HANDLE>,
    container: Option<&OwnedSid>,
    expectation: Expectation<'_>,
    cell: &CellSpec,
    policy: &SandboxPolicy,
    request: &SpawnRequest,
) -> Result<SandboxedProcess, SandboxError> {
    let started = start(StartSpec {
        token_handle,
        container,
        expectation,
        cell,
        policy,
        program: &request.program,
        args: &request.args,
        cwd: &request.cwd,
        environment: &request.environment,
    })?;
    Ok(SandboxedProcess::new(
        started.pid,
        into_file(started.stdin),
        into_file(started.stdout),
        into_file(started.stderr),
        ProcessHandle {
            process: started.process,
            job: started.job,
        },
    ))
}

fn into_file(handle: OwnedHandle) -> std::fs::File {
    // SAFETY: the handle is owned by this function and is a pipe end; the File
    // becomes its unique owner.
    unsafe { std::fs::File::from_raw_handle(handle.into_raw()) }
}

/// Handles and exit information for a launched (and resumed) child.
struct StartedProcess {
    pid: u32,
    process: OwnedHandle,
    job: OwnedHandle,
    /// Parent ends, still owned as handles.
    stdin: OwnedHandle,
    stdout: OwnedHandle,
    stderr: OwnedHandle,
}

/// Everything one launch needs; grouped so the call sites stay readable.
struct StartSpec<'a> {
    token_handle: Option<HANDLE>,
    container: Option<&'a OwnedSid>,
    expectation: Expectation<'a>,
    cell: &'a CellSpec,
    policy: &'a SandboxPolicy,
    program: &'a Path,
    args: &'a [OsString],
    cwd: &'a Path,
    environment: &'a [(OsString, OsString)],
}

/// Create the child suspended, prove its token, resume it, and hand the parent
/// ends back. Shared by one-shot [`run`] and [`spawn_streaming`].
fn start(spec: StartSpec<'_>) -> Result<StartedProcess, SandboxError> {
    let StartSpec {
        token_handle,
        container,
        expectation,
        cell,
        policy,
        program: request_program,
        args: request_args,
        cwd: request_cwd,
        environment: request_environment,
    } = spec;
    if !request_cwd.starts_with(cell.root()) {
        return Err(SandboxError::PolicyRejected(format!(
            "工作目录不在沙箱单元内: {}",
            request_cwd.display()
        )));
    }
    // Validate `request_cwd` without traversing any reparse points and confirm
    // its canonical target still sits inside the canonical cell root.
    let _pinned_cwd = super::acl::PinnedPath::open(request_cwd).map_err(|_| {
        SandboxError::PolicyRejected(format!(
            "工作目录不可用或包含重解析点（junction/符号链接）: {}",
            request_cwd.display()
        ))
    })?;
    let canonical_root = std::fs::canonicalize(cell.root()).map_err(|error| {
        SandboxError::Setup(format!(
            "无法规范化沙箱单元根目录 {}: {error}",
            cell.root().display()
        ))
    })?;
    let canonical_cwd = std::fs::canonicalize(request_cwd).map_err(|error| {
        SandboxError::PolicyRejected(format!(
            "无法规范化工作目录 {}: {error}",
            request_cwd.display()
        ))
    })?;
    if !canonical_cwd.starts_with(&canonical_root) {
        return Err(SandboxError::PolicyRejected(format!(
            "工作目录规范化后超出沙箱单元: {}",
            request_cwd.display()
        )));
    }
    let job = create_job(policy)?;
    let (parent_stdin, child_stdin) = pipe(false)?;
    let (parent_stdout, child_stdout) = pipe(true)?;
    let (parent_stderr, child_stderr) = pipe(true)?;
    let child_stdin_handle = child_stdin.into_raw();
    let child_stdout_handle = child_stdout.into_raw();
    let child_stderr_handle = child_stderr.into_raw();

    let environment = environment_block(request_environment)?;
    let mut command_line = command_line(request_program, request_args);
    let cwd = path_wide(request_cwd);
    let program = path_wide(request_program);

    let mut attributes = Attributes::new(if container.is_some() { 3 } else { 2 }, &job, container)?;
    attributes.set_handle_list(&[child_stdin_handle, child_stdout_handle, child_stderr_handle])?;

    let mut startup: STARTUPINFOEXW = unsafe {
        // SAFETY: STARTUPINFOEXW is plain-old-data; zero is the documented
        // "defaults" value, after which cb and lpAttributeList are set.
        std::mem::zeroed()
    };
    startup.StartupInfo.cb = std::mem::size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = child_stdin_handle;
    startup.StartupInfo.hStdOutput = child_stdout_handle;
    startup.StartupInfo.hStdError = child_stderr_handle;
    startup.lpAttributeList = attributes.as_mut_ptr();

    let mut process: PROCESS_INFORMATION = unsafe {
        // SAFETY: plain-old-data output structure, zeroed before the call.
        std::mem::zeroed()
    };
    let flags = CREATE_SUSPENDED
        | CREATE_UNICODE_ENVIRONMENT
        | EXTENDED_STARTUPINFO_PRESENT
        | CREATE_NO_WINDOW;
    // SAFETY: every pointer passed to the API points at a live, correctly sized
    // value for the duration of the call: NUL-terminated program/cwd strings, a
    // mutable command line, a Unicode environment block, a STARTUPINFOEXW whose
    // attribute list references the live `attributes`/`job`, and the token
    // handle (or null for the AppContainer path).
    let created = unsafe {
        match token_handle {
            Some(token_handle) => CreateProcessAsUserW(
                token_handle,
                program.as_ptr(),
                command_line.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                flags,
                environment.as_ptr().cast(),
                cwd.as_ptr(),
                &startup.StartupInfo,
                &mut process,
            ),
            None => CreateProcessW(
                program.as_ptr(),
                command_line.as_mut_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                1,
                flags,
                environment.as_ptr().cast(),
                cwd.as_ptr(),
                &startup.StartupInfo,
                &mut process,
            ),
        }
    };
    if created == 0 {
        return Err(SandboxError::Spawn(format!(
            "CreateProcess 失败（{}）: {}",
            request_program.display(),
            error_text(last_error())
        )));
    }
    let process_handle = OwnedHandle(process.hProcess);
    let thread_handle = OwnedHandle(process.hThread);

    // The child must prove the token before it runs a single instruction.
    if let Err(reason) = expectation.verify(process_handle.0) {
        // SAFETY: the process handle is live and the process is suspended.
        unsafe { TerminateProcess(process_handle.0, 1) };
        return Err(SandboxError::NotSandboxed(reason));
    }

    // SAFETY: the thread handle is live and the child was created suspended, so
    // this resumes it exactly once.
    if unsafe { ResumeThread(thread_handle.0) } == u32::MAX {
        // SAFETY: the child is still suspended: terminate rather than leak it.
        unsafe { TerminateProcess(process_handle.0, 1) };
        return Err(SandboxError::Spawn(format!(
            "ResumeThread 失败: {}",
            error_text(last_error())
        )));
    }

    drop(thread_handle);
    drop(attributes);
    // The child has its own copies of the stdio handles; close ours so the
    // pipes see EOF when the child (and its tree) exit.
    drop(OwnedHandle(child_stdin_handle));
    drop(OwnedHandle(child_stdout_handle));
    drop(OwnedHandle(child_stderr_handle));

    Ok(StartedProcess {
        pid: process.dwProcessId,
        process: process_handle,
        job,
        stdin: parent_stdin,
        stdout: parent_stdout,
        stderr: parent_stderr,
    })
}

fn run(
    token_handle: Option<HANDLE>,
    container: Option<&OwnedSid>,
    expectation: Expectation<'_>,
    cell: &CellSpec,
    policy: &SandboxPolicy,
    request: &ExecRequest,
) -> Result<ExecResult, SandboxError> {
    let StartedProcess {
        process: process_handle,
        job,
        stdin: parent_stdin,
        stdout: parent_stdout,
        stderr: parent_stderr,
        ..
    } = start(StartSpec {
        token_handle,
        container,
        expectation,
        cell,
        policy,
        program: &request.program,
        args: &request.args,
        cwd: &request.cwd,
        environment: &request.environment,
    })?;
    let parent_stdin_handle = parent_stdin.into_raw();
    let parent_stdout_handle = parent_stdout.into_raw();
    let parent_stderr_handle = parent_stderr.into_raw();

    // Feed stdin, then close the write end so the child sees EOF.
    // SAFETY: the handle is exclusively owned by this function from here on.
    let mut stdin = unsafe { std::fs::File::from_raw_handle(parent_stdin_handle) };
    if !request.stdin.is_empty()
        && let Err(error) = stdin.write_all(&request.stdin)
    {
        return Err(SandboxError::Execution(format!(
            "写入子进程 stdin 失败: {error}"
        )));
    }
    drop(stdin);

    let stdout = read_stream(parent_stdout_handle, policy.max_output_or_default());
    let stderr = read_stream(parent_stderr_handle, policy.max_output_or_default());

    let timeout_ms = policy
        .timeout_or_default()
        .as_millis()
        .min(u128::from(u32::MAX)) as u32;
    // SAFETY: the process handle is live and owned here.
    let wait = unsafe { WaitForSingleObject(process_handle.0, timeout_ms) };
    let timed_out = wait == WAIT_TIMEOUT;
    if wait != WAIT_OBJECT_0 && !timed_out {
        return Err(SandboxError::Execution(format!(
            "等待子进程失败: {}",
            error_text(last_error())
        )));
    }
    // Always kill the remaining tree: a command's children die with the command,
    // and it is what closes the pipe write ends when a grandchild inherits them.
    // SAFETY: the job handle is owned here.
    unsafe { TerminateJobObject(job.0, 1) };

    let mut exit_code = 0u32;
    // SAFETY: the process handle is live; the call only writes the exit code.
    unsafe { GetExitCodeProcess(process_handle.0, &mut exit_code) };

    let (stdout, stdout_truncated) = drain(stdout, "stdout")?;
    let (stderr, stderr_truncated) = drain(stderr, "stderr")?;

    Ok(ExecResult {
        exit_code: Some(exit_code as i32),
        stdout,
        stderr,
        timed_out,
        stdout_truncated,
        stderr_truncated,
        sandboxed: true,
    })
}

// ---------------------------------------------------------------------------
// Long-lived process handle
// ---------------------------------------------------------------------------

/// Ownership of a running sandboxed process plus the Job that bounds its tree.
///
/// Every method maps one kernel call; the caller (the crate's
/// [`SandboxedProcess`] wrapper) owns the pipes. `Drop` terminates the tree so
/// a host that forgets to call `terminate` cannot leak grandchildren.
pub(crate) struct ProcessHandle {
    process: OwnedHandle,
    job: OwnedHandle,
}

// SAFETY: the handle owns two kernel object handles (value semantics), not
// pointers into process memory; each handle has exactly one owner. Moving it
// across threads is what lets the host keep the process in a worker struct.
unsafe impl Send for ProcessHandle {}
// SAFETY: shared access only performs QueryInformationJobObject, GetExitCode-
// Process and Terminate calls, all thread-safe kernel operations; closing the
// handles happens in the unique owner's Drop.
unsafe impl Sync for ProcessHandle {}

impl ProcessHandle {
    pub(crate) fn try_wait(&self) -> std::io::Result<Option<i32>> {
        let mut code = 0u32;
        // SAFETY: `self.process` is a live process handle; `code` receives the
        // exit code.
        if unsafe { GetExitCodeProcess(self.process.0, &mut code) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        if code == STILL_ACTIVE as u32 {
            Ok(None)
        } else {
            Ok(Some(code as i32))
        }
    }

    pub(crate) fn wait_timeout(
        &self,
        timeout: std::time::Duration,
    ) -> std::io::Result<Option<i32>> {
        let timeout_ms = timeout.as_millis().min(u128::from(u32::MAX - 1)) as u32;
        // SAFETY: `self.process` is a live process handle.
        match unsafe { WaitForSingleObject(self.process.0, timeout_ms) } {
            WAIT_OBJECT_0 => self.try_wait(),
            WAIT_TIMEOUT => Ok(None),
            _ => Err(std::io::Error::last_os_error()),
        }
    }

    pub(crate) fn terminate(&self) -> std::io::Result<()> {
        // SAFETY: both handles are live and owned by `self`.
        if unsafe { TerminateJobObject(self.job.0, 1) } != 0 {
            return Ok(());
        }
        // The Job call can fail (closed handle, denied access) while the
        // process is still alive; terminating it directly is the last resort.
        // SAFETY: the process handle is live.
        if unsafe { TerminateProcess(self.process.0, 1) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    pub(crate) fn limits(&self) -> std::io::Result<SandboxLimits> {
        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        // SAFETY: `info` is a writable structure of the exact size passed and
        // the job handle is live.
        let ok = unsafe {
            QueryInformationJobObject(
                self.job.0,
                JobObjectExtendedLimitInformation,
                &mut info as *mut _ as *mut c_void,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(SandboxLimits {
            active_process_limit: info.BasicLimitInformation.ActiveProcessLimit,
            process_memory_limit: info.ProcessMemoryLimit as u64,
            kill_on_close: info.BasicLimitInformation.LimitFlags
                & JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
                != 0,
        })
    }
}

impl Drop for ProcessHandle {
    fn drop(&mut self) {
        // KILL_ON_JOB_CLOSE arms when the Job handle closes; the explicit call
        // makes the common path synchronous. Errors are ignored on drop.
        let _ = self.terminate();
    }
}

// ---------------------------------------------------------------------------
// Job Object
// ---------------------------------------------------------------------------

fn create_job(policy: &SandboxPolicy) -> Result<OwnedHandle, SandboxError> {
    // SAFETY: default attributes and an unnamed job; the returned handle is
    // owned by `OwnedHandle`.
    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return Err(SandboxError::Setup(format!(
            "CreateJobObjectW 失败: {}",
            error_text(last_error())
        )));
    }
    let job = OwnedHandle(job);
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe {
        // SAFETY: plain-old-data; zeroed before the fields are set.
        std::mem::zeroed()
    };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_ACTIVE_PROCESS
        | JOB_OBJECT_LIMIT_PROCESS_MEMORY
        | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    limits.BasicLimitInformation.ActiveProcessLimit = policy.process_limit_or_default();
    limits.ProcessMemoryLimit = policy.memory_limit_or_default() as usize;
    // SAFETY: the structure is fully initialized and the size matches its type.
    let set = unsafe {
        SetInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if set == 0 {
        return Err(SandboxError::Setup(format!(
            "SetInformationJobObject 失败: {}",
            error_text(last_error())
        )));
    }
    let mut readback: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe {
        // SAFETY: plain-old-data output.
        std::mem::zeroed()
    };
    let mut returned = 0u32;
    // SAFETY: the output buffer is the size passed and stays alive for the call.
    let queried = unsafe {
        QueryInformationJobObject(
            job.0,
            JobObjectExtendedLimitInformation,
            &mut readback as *mut _ as *mut c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            &mut returned,
        )
    };
    if queried == 0
        || readback.BasicLimitInformation.ActiveProcessLimit
            != limits.BasicLimitInformation.ActiveProcessLimit
        || readback.ProcessMemoryLimit != limits.ProcessMemoryLimit
        || readback.BasicLimitInformation.LimitFlags != limits.BasicLimitInformation.LimitFlags
    {
        return Err(SandboxError::Setup(
            "Job Object 限额写入后回读不一致，拒绝启动".to_owned(),
        ));
    }
    Ok(job)
}

/// Probe: create a Job with limits and read them back.
pub(crate) fn probe_job_limits() -> Capability {
    let policy = SandboxPolicy::scratch(std::env::temp_dir());
    match create_job(&policy) {
        Ok(_) => Capability::Verified,
        Err(_) => Capability::Unverified,
    }
}

// ---------------------------------------------------------------------------
// Pipes and output
// ---------------------------------------------------------------------------

/// Create one pipe and return `(parent_end, child_end)`.
///
/// `parent_is_read_end` selects the flow: for stdout/stderr the parent reads
/// and the child writes; for stdin the parent writes and the child reads. Only
/// the child end is inheritable, so the handle list passed to `CreateProcess`
/// contains exactly the three ends the child may use.
fn pipe(parent_is_read_end: bool) -> Result<(OwnedHandle, OwnedHandle), SandboxError> {
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: 1,
    };
    let mut read: HANDLE = std::ptr::null_mut();
    let mut write: HANDLE = std::ptr::null_mut();
    // SAFETY: `attributes` asks for inheritable handles; both outputs are
    // written by the call and wrapped in owned handles.
    let ok = unsafe { CreatePipe(&mut read, &mut write, &attributes, 0) };
    if ok == 0 {
        return Err(SandboxError::Setup(format!(
            "CreatePipe 失败: {}",
            error_text(last_error())
        )));
    }
    let parent = if parent_is_read_end { read } else { write };
    // The parent-side end must not leak into the child.
    // SAFETY: `parent` is a live handle owned by this function.
    unsafe { SetHandleInformation(parent, HANDLE_FLAG_INHERIT, 0) };
    Ok(if parent_is_read_end {
        (OwnedHandle(read), OwnedHandle(write))
    } else {
        (OwnedHandle(write), OwnedHandle(read))
    })
}

/// Reader thread for one pipe: bounded buffer plus a truncation flag.
fn read_stream(handle: HANDLE, cap: usize) -> mpsc::Receiver<(Vec<u8>, bool)> {
    let (sender, receiver) = mpsc::channel();
    let raw = handle as usize;
    std::thread::spawn(move || {
        // SAFETY: the handle is exclusively owned by this thread from here on.
        let mut file = unsafe { std::fs::File::from_raw_handle(raw as *mut c_void) };
        let mut output = Vec::new();
        let mut truncated = false;
        let mut buffer = [0u8; 8192];
        loop {
            match file.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => {
                    if output.len() < cap {
                        let take = (cap - output.len()).min(read);
                        output.extend_from_slice(&buffer[..take]);
                        if take < read {
                            truncated = true;
                        }
                    } else {
                        truncated = true;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = sender.send((output, truncated));
    });
    receiver
}

fn drain(
    receiver: mpsc::Receiver<(Vec<u8>, bool)>,
    stream: &str,
) -> Result<(Vec<u8>, bool), SandboxError> {
    receiver.recv_timeout(DRAIN_TIMEOUT).map_err(|_| {
        SandboxError::Execution(format!("{stream} 读取超时（管道未在进程树退出后关闭）"))
    })
}

// ---------------------------------------------------------------------------
// Token read-back
// ---------------------------------------------------------------------------

/// `GetTokenInformation` into a sized buffer (query, allocate, read).
fn token_info(
    token: HANDLE,
    class: windows_sys::Win32::Security::TOKEN_INFORMATION_CLASS,
) -> Result<Vec<u8>, String> {
    let mut needed = 0u32;
    // SAFETY: size query with a null buffer.
    unsafe { GetTokenInformation(token, class, std::ptr::null_mut(), 0, &mut needed) };
    if needed == 0 {
        return Err(format!(
            "令牌信息 {class} 长度查询失败: {}",
            error_text(last_error())
        ));
    }
    let mut buffer = vec![0u8; needed as usize];
    let mut returned = 0u32;
    // SAFETY: the buffer is exactly the size the kernel reported.
    let ok = unsafe {
        GetTokenInformation(
            token,
            class,
            buffer.as_mut_ptr().cast(),
            buffer.len() as u32,
            &mut returned,
        )
    };
    if ok == 0 {
        return Err(format!(
            "令牌信息 {class} 读取失败: {}",
            error_text(last_error())
        ));
    }
    buffer.truncate(returned as usize);
    Ok(buffer)
}

/// What the child's token must prove before it is resumed.
pub(crate) enum Expectation<'a> {
    /// A restricted token: restricted, carrying the cell's capability SID.
    Restricted {
        capability: &'a OwnedSid,
        /// The sandbox account's SID when the dedicated identity is in use.
        user: Option<&'a OwnedSid>,
    },
    /// A lowbox token for the cell's AppContainer.
    Container {
        container: &'a OwnedSid,
        user: Option<&'a OwnedSid>,
    },
}

impl Expectation<'_> {
    fn verify(&self, process: HANDLE) -> Result<(), String> {
        let mut token: HANDLE = std::ptr::null_mut();
        // SAFETY: `process` is the live suspended child; `token` receives an
        // owned handle with TOKEN_QUERY.
        let ok = unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) };
        if ok == 0 || token.is_null() {
            return Err(format!("无法打开子进程令牌: {}", error_text(last_error())));
        }
        let token = OwnedHandle(token);
        match self {
            Expectation::Restricted { capability, .. } => {
                // SAFETY: `IsTokenRestricted` only queries the handle.
                if unsafe { IsTokenRestricted(token.0) } == 0 {
                    return Err("IsTokenRestricted 判定子进程令牌不是受限令牌".to_owned());
                }
                let buffer = token_info(token.0, TokenHasRestrictions)?;
                if buffer.first().copied().unwrap_or(0) == 0 {
                    return Err("子进程令牌没有限制标记".to_owned());
                }
                match token::restricted_sids_contain(token.0, capability) {
                    Ok(true) => {}
                    Ok(false) => {
                        return Err("子进程令牌的限制 SID 不含本单元的 capability SID".to_owned());
                    }
                    Err(error) => return Err(format!("无法枚举子进程限制 SID: {error}")),
                }
            }
            Expectation::Container { container, .. } => {
                let buffer = token_info(token.0, TokenIsAppContainer)?;
                let is_container = buffer
                    .get(..4)
                    .map(|bytes| u32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
                    .unwrap_or(0);
                if is_container == 0 {
                    return Err("子进程令牌不是 AppContainer".to_owned());
                }
                let info = token_info(token.0, TokenAppContainerSid)?;
                let pointer_bytes: [u8; std::mem::size_of::<usize>()] = info
                    .get(..std::mem::size_of::<usize>())
                    .and_then(|bytes| bytes.try_into().ok())
                    .ok_or_else(|| "AppContainer SID 信息不完整".to_owned())?;
                let sid = usize::from_ne_bytes(pointer_bytes) as *mut c_void;
                if sid.is_null() {
                    return Err("子进程 AppContainer SID 为空".to_owned());
                }
                // SAFETY: both SIDs are valid for the comparison.
                if unsafe { windows_sys::Win32::Security::EqualSid(sid, container.as_ptr()) } == 0 {
                    return Err("子进程 AppContainer SID 与本单元不一致".to_owned());
                }
            }
        }
        if let Some(expected) = self.user() {
            let actual = token::user_sid_of(token.0)
                .map_err(|error| format!("failed to verify child process user SID: {error}"))?;
            if actual.as_string() != expected.as_string() {
                return Err(format!(
                    "child process running as {}, expected sandbox account {}",
                    actual.as_string(),
                    expected.as_string()
                ));
            }
        }
        Ok(())
    }

    fn user(&self) -> Option<&OwnedSid> {
        match self {
            Expectation::Restricted { user, .. } | Expectation::Container { user, .. } => *user,
        }
    }
}

// ---------------------------------------------------------------------------
// Attribute list
// ---------------------------------------------------------------------------

struct Attributes {
    /// Word-aligned storage: `InitializeProcThreadAttributeList` writes
    /// pointer-sized fields into it.
    buffer: Vec<usize>,
    handles: Vec<HANDLE>,
    jobs: Vec<HANDLE>,
    capabilities: Box<SECURITY_CAPABILITIES>,
}

impl Attributes {
    fn new(
        count: u32,
        job: &OwnedHandle,
        container: Option<&OwnedSid>,
    ) -> Result<Self, SandboxError> {
        let mut size = 0usize;
        // SAFETY: a size query with a null list is the documented first call;
        // it fills `size`.
        unsafe { InitializeProcThreadAttributeList(std::ptr::null_mut(), count, 0, &mut size) };
        if size == 0 {
            return Err(SandboxError::Setup(format!(
                "InitializeProcThreadAttributeList 查询大小失败: {}",
                error_text(last_error())
            )));
        }
        let mut buffer = vec![0usize; size.div_ceil(std::mem::size_of::<usize>())];
        // SAFETY: the buffer is at least `size` bytes, the value the API asked
        // for; `size` is passed back as the available size.
        let ok = unsafe {
            InitializeProcThreadAttributeList(
                buffer.as_mut_ptr().cast::<c_void>(),
                count,
                0,
                &mut size,
            )
        };
        if ok == 0 {
            return Err(SandboxError::Setup(format!(
                "InitializeProcThreadAttributeList 失败: {}",
                error_text(last_error())
            )));
        }
        // SAFETY: `SECURITY_CAPABILITIES` is plain-old-data; zeroing initializes
        // null SID/capability pointers before optional fields are populated below.
        let mut capabilities: Box<SECURITY_CAPABILITIES> = Box::new(unsafe { std::mem::zeroed() });
        if let Some(sid) = container {
            capabilities.AppContainerSid = sid.as_ptr();
            capabilities.Capabilities = std::ptr::null_mut();
            capabilities.CapabilityCount = 0;
        }
        let mut attributes = Self {
            buffer,
            handles: Vec::new(),
            jobs: vec![job.0],
            capabilities,
        };
        if container.is_some() {
            attributes.set_capabilities()?;
        }
        attributes.set_job()?;
        Ok(attributes)
    }

    fn as_mut_ptr(&mut self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.buffer.as_mut_ptr().cast()
    }

    fn set_handle_list(&mut self, handles: &[HANDLE]) -> Result<(), SandboxError> {
        self.handles = handles.to_vec();
        let value = self.handles.as_mut_ptr().cast();
        let size = std::mem::size_of_val(self.handles.as_slice());
        self.update(PROC_THREAD_ATTRIBUTE_HANDLE_LIST, value, size)
    }

    fn set_job(&mut self) -> Result<(), SandboxError> {
        // `jobs` is a field so its address stays valid until the attribute list
        // is deleted (the API stores the pointer, not a copy).
        let value = self.jobs.as_mut_ptr().cast();
        let size = std::mem::size_of_val(self.jobs.as_slice());
        self.update(PROC_THREAD_ATTRIBUTE_JOB_LIST, value, size)
    }

    fn set_capabilities(&mut self) -> Result<(), SandboxError> {
        let value = (&mut *self.capabilities) as *mut SECURITY_CAPABILITIES as *const c_void;
        self.update(
            PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES,
            value,
            std::mem::size_of::<SECURITY_CAPABILITIES>(),
        )
    }

    fn update(
        &mut self,
        attribute: usize,
        value: *const c_void,
        size: usize,
    ) -> Result<(), SandboxError> {
        // SAFETY: the list was initialized above; `value` points at a field of
        // `self` that outlives the list; `size` matches the attribute's type.
        let ok = unsafe {
            UpdateProcThreadAttribute(
                self.as_mut_ptr(),
                0,
                attribute,
                value,
                size,
                std::ptr::null_mut(),
                std::ptr::null(),
            )
        };
        if ok == 0 {
            return Err(SandboxError::Setup(format!(
                "UpdateProcThreadAttribute 失败: {}",
                error_text(last_error())
            )));
        }
        Ok(())
    }
}

impl Drop for Attributes {
    fn drop(&mut self) {
        // SAFETY: the pointer came from InitializeProcThreadAttributeList and is
        // deleted exactly once.
        unsafe {
            windows_sys::Win32::System::Threading::DeleteProcThreadAttributeList(self.as_mut_ptr())
        };
    }
}

// ---------------------------------------------------------------------------
// Command line and environment
// ---------------------------------------------------------------------------

fn command_line(program: &Path, args: &[OsString]) -> Vec<u16> {
    // Always quote the program: a path such as `C:\Program Files\...` must not
    // be split by the loader, and quoting an unquoted-safe path is harmless.
    let mut text = format!("\"{}\"", program.to_string_lossy());
    for arg in args {
        text.push(' ');
        text.push_str(&quote_arg(&arg.to_string_lossy()));
    }
    OsStr::new(&text).encode_wide().chain(Some(0)).collect()
}

/// Quote one argument per Microsoft's C runtime parsing rules.
fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains([' ', '\t', '"']) {
        return arg.to_owned();
    }
    let mut quoted = String::from("\"");
    let mut backslashes = 0usize;
    for ch in arg.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                quoted.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            }
            _ => {
                quoted.extend(std::iter::repeat_n('\\', backslashes));
                backslashes = 0;
                quoted.push(ch);
            }
        }
    }
    quoted.extend(std::iter::repeat_n('\\', backslashes * 2));
    quoted.push('"');
    quoted
}

fn environment_block(environment: &[(OsString, OsString)]) -> Result<Vec<u16>, SandboxError> {
    let mut entries: Vec<(String, String)> = Vec::with_capacity(environment.len());
    for (key, value) in environment {
        let key = key
            .to_str()
            .ok_or_else(|| SandboxError::PolicyRejected("环境变量名不是 UTF-8".to_owned()))?;
        if key.is_empty() || key.contains(['=', '\0']) {
            return Err(SandboxError::PolicyRejected(format!(
                "环境变量名非法: {key:?}"
            )));
        }
        let value = value.to_str().ok_or_else(|| {
            SandboxError::PolicyRejected(format!("环境变量 {key} 的值不是 UTF-8"))
        })?;
        if value.contains('\0') {
            return Err(SandboxError::PolicyRejected(format!(
                "环境变量 {key} 的值含 NUL"
            )));
        }
        entries.push((key.to_owned(), value.to_owned()));
    }
    // Windows expects a case-insensitive order so that duplicate spellings of a
    // key are adjacent. The sort is stable, so equal keys keep the caller's
    // order and the first spelling wins below.
    entries.sort_by_key(|(key, _)| key.to_uppercase());
    let mut block = Vec::new();
    let mut previous: Option<String> = None;
    for (key, value) in entries {
        if previous
            .as_deref()
            .is_some_and(|previous| key.eq_ignore_ascii_case(previous))
        {
            continue;
        }
        block.extend(OsStr::new(&format!("{key}={value}")).encode_wide());
        block.push(0);
        previous = Some(key);
    }
    block.push(0);
    Ok(block)
}

fn last_error() -> u32 {
    // SAFETY: reads the thread-local last-error value.
    unsafe { GetLastError() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments_are_quoted_per_the_c_runtime_rules() {
        assert_eq!(quote_arg("plain"), "plain");
        assert_eq!(quote_arg(""), "\"\"");
        assert_eq!(quote_arg("a b"), "\"a b\"");
        assert_eq!(quote_arg("a\\\"b"), "\"a\\\\\\\"b\"");
        assert_eq!(quote_arg("trailing\\"), "trailing\\");
        assert_eq!(quote_arg("a b\\"), "\"a b\\\\\"");
        let line = command_line(
            Path::new(r"C:\Windows\System32\cmd.exe"),
            &[OsString::from("/c"), OsString::from("echo hi")],
        );
        let text = String::from_utf16(&line[..line.len() - 1]).unwrap();
        assert_eq!(text, r#""C:\Windows\System32\cmd.exe" /c "echo hi""#);
    }

    #[test]
    fn environment_blocks_are_sorted_and_deduplicated() {
        let block = environment_block(&[
            (OsString::from("Path"), OsString::from("a")),
            (OsString::from("TEMP"), OsString::from("b")),
            (OsString::from("PATH"), OsString::from("c")),
        ])
        .unwrap();
        assert_eq!(block.last(), Some(&0));
        assert_eq!(block[block.len() - 2], 0);
        let text = String::from_utf16(&block[..block.len() - 2]).unwrap();
        assert_eq!(text, "Path=a\0TEMP=b");
        assert!(environment_block(&[(OsString::from("A=B"), OsString::from("x"))]).is_err());
        assert!(environment_block(&[(OsString::from("A"), OsString::from("x\0y"))]).is_err());
    }
}
