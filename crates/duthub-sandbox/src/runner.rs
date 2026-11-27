//! In-process MXC execution engine adapter (`mxc_sdk::v1`).
//!
//! Translates [`SandboxPolicy`] + [`ExecRequest`]/[`SpawnRequest`] into typed
//! `mxc_sdk::v1::ContainerRequest` values and manages both one-shot captured
//! execution and long-lived streaming processes with deterministic tree kill.

use std::collections::HashSet;
use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use mxc_sdk::v1::configs::{
    CaptureDenials, CaptureDenialsMode, ProcessContainerConfig, ProcessContainerFilesystem,
};
use mxc_sdk::v1::policy::{
    ClipboardPolicy, Containment, FilesystemPolicy, NetworkAction, NetworkEgressPolicy,
    NetworkIngressPolicy, NetworkPeerPolicy, NetworkPolicy as MxcNetworkPolicy, NetworkRulePolicy,
    NetworkRuntimeConfig, UiPolicy as MxcUiPolicy,
};
use mxc_sdk::v1::{
    self, ContainerRequest, Error as MxcError, ErrorCode, MxcProcess, SpawnOptions, WaitResult,
};

use crate::policy::{
    CellSpec, DenialCaptureConfig, ExecRequest, ExecResult, NetworkPolicy, SandboxError,
    SandboxPolicy, SpawnRequest, UiPolicy, format_command_line, path_within,
    powershell_environment, powershell_request, strip_verbatim, valid_environment_key,
    validate_policy, validate_scope,
};

/// Polling cadence used by [`SandboxedProcess::wait_timeout`].
const WAIT_POLL_INTERVAL: Duration = Duration::from_millis(10);
/// Bounded wait during `Drop` after tree termination.
const DROP_REAP_TIMEOUT: Duration = Duration::from_secs(2);

/// Re-export of MXC's read-unblocking stream closer.
pub type StreamCloser = v1::StreamCloser;

/// Summary of native denial capture returned after process teardown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DenialCaptureSummary {
    pub json_path: PathBuf,
    pub total_denials: usize,
    pub etl_path: Option<PathBuf>,
}

/// Long-lived containerized process whose stdio pipes belong to the caller.
///
/// Dropping the handle terminates the entire container process tree and
/// releases the MXC security environment.
pub struct SandboxedProcess {
    pid: u32,
    stdin: Option<Box<dyn Write + Send>>,
    stdout: Option<Box<dyn Read + Send>>,
    stderr: Option<Box<dyn Read + Send>>,
    stdout_closer: Option<StreamCloser>,
    stderr_closer: Option<StreamCloser>,
    inner: Mutex<MxcProcess>,
}

impl SandboxedProcess {
    fn from_mxc(mut process: MxcProcess) -> Self {
        let pid = process.id();
        let stdout_closer = process.stdout_closer();
        let stderr_closer = process.stderr_closer();
        let stdin = process.take_stdin();
        let stdout = process.take_stdout();
        let stderr = process.take_stderr();
        Self {
            pid,
            stdin,
            stdout,
            stderr,
            stdout_closer,
            stderr_closer,
            inner: Mutex::new(process),
        }
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// Take the child's stdin pipe. Dropping it signals EOF to the child.
    pub fn take_stdin(&mut self) -> Option<Box<dyn Write + Send>> {
        self.stdin.take()
    }

    /// Take the child's stdout pipe.
    pub fn take_stdout(&mut self) -> Option<Box<dyn Read + Send>> {
        self.stdout.take()
    }

    /// Take the child's stderr pipe.
    pub fn take_stderr(&mut self) -> Option<Box<dyn Read + Send>> {
        self.stderr.take()
    }

    /// Closer that unblocks a parked read on stdout without killing the child.
    pub fn stdout_closer(&self) -> Option<&StreamCloser> {
        self.stdout_closer.as_ref()
    }

    /// Closer that unblocks a parked read on stderr without killing the child.
    pub fn stderr_closer(&self) -> Option<&StreamCloser> {
        self.stderr_closer.as_ref()
    }

    /// Non-blocking exit status check.
    pub fn try_wait(&self) -> io::Result<Option<i32>> {
        self.lock_inner().try_wait()
    }

    /// Wait up to `timeout` for the process to exit.
    pub fn wait_timeout(&self, timeout: Duration) -> io::Result<Option<i32>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(code) = self.try_wait()? {
                return Ok(Some(code));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(WAIT_POLL_INTERVAL);
        }
    }

    /// Terminate the entire container process tree.
    pub fn terminate(&self) -> io::Result<()> {
        self.lock_inner().kill()
    }

    /// Policy and operational warnings reported by MXC for this container.
    pub fn warnings(&self) -> Vec<String> {
        self.lock_inner().warnings()
    }

    /// Denial-capture summary available after process exit and teardown.
    pub fn denial_capture(&self) -> Option<DenialCaptureSummary> {
        self.lock_inner()
            .output_metadata()
            .and_then(|meta| meta.capture_denials.as_ref())
            .map(summary_from_mxc)
    }

    fn lock_inner(&self) -> std::sync::MutexGuard<'_, MxcProcess> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl std::fmt::Debug for SandboxedProcess {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SandboxedProcess")
            .field("pid", &self.pid)
            .finish_non_exhaustive()
    }
}

impl Drop for SandboxedProcess {
    fn drop(&mut self) {
        self.stdin.take();
        if let Some(closer) = self.stdout_closer.take() {
            closer.close();
        }
        if let Some(closer) = self.stderr_closer.take() {
            closer.close();
        }
        let mut inner = self.lock_inner();
        let _ = inner.kill();
        let deadline = Instant::now() + DROP_REAP_TIMEOUT;
        while Instant::now() < deadline {
            match inner.try_wait() {
                Ok(Some(_)) | Err(_) => break,
                Ok(None) => std::thread::sleep(WAIT_POLL_INTERVAL),
            }
        }
    }
}

/// Create the cell's directory layout and verify that none of the paths are
/// symlinks or NTFS reparse points.
pub fn prepare_cell(cell: &CellSpec) -> Result<(), SandboxError> {
    validate_scope(cell.scope())?;
    std::fs::create_dir_all(cell.root()).map_err(|error| {
        SandboxError::Setup(format!(
            "failed to create sandbox cell {}: {error}",
            cell.root().display()
        ))
    })?;
    for dir in cell.required_dirs() {
        std::fs::create_dir_all(&dir).map_err(|error| {
            SandboxError::Setup(format!(
                "failed to create sandbox directory {}: {error}",
                dir.display()
            ))
        })?;
    }
    for dir in std::iter::once(cell.root().to_path_buf()).chain(cell.required_dirs()) {
        ensure_plain_directory(&dir)?;
    }
    Ok(())
}

/// Probe request compatibility against the host MXC backend without launching.
pub fn probe_request(
    cell: &CellSpec,
    policy: &SandboxPolicy,
    request: &SpawnRequest,
) -> Result<(), SandboxError> {
    validate_scope(cell.scope())?;
    validate_executable(&request.program)?;
    validate_policy(policy, &request.cwd)?;
    prepare_cell(cell)?;
    let mxc_req = build_mxc_request(
        cell,
        policy,
        &request.program,
        &request.args,
        &request.cwd,
        &request.environment,
        None,
    )?;

    #[cfg(target_os = "windows")]
    {
        let output = v1::probe(Some(&mxc_req)).map_err(|err| map_mxc_error(&err))?;
        if let Some(error) = output.error {
            return Err(SandboxError::Unsupported(error));
        }
        if output.tier.is_none() {
            return Err(SandboxError::Unsupported(
                "Windows ProcessContainer could not service this request".to_owned(),
            ));
        }
        Ok(())
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = mxc_req;
        let host = crate::status();
        if host.is_enforced() {
            Ok(())
        } else {
            Err(SandboxError::Unsupported(host.summary()))
        }
    }
}

/// Run a single host-built command to completion inside an MXC container.
pub fn run(
    cell: &CellSpec,
    policy: &SandboxPolicy,
    request: &ExecRequest,
) -> Result<ExecResult, SandboxError> {
    validate_scope(cell.scope())?;
    validate_executable(&request.program)?;
    validate_policy(policy, &request.cwd)?;
    prepare_cell(cell)?;

    let timeout = policy.timeout_or_default();
    let timeout_ms = u32::try_from(timeout.as_millis()).unwrap_or(u32::MAX);
    let max_output = policy.max_output_or_default();
    let mxc_req = build_mxc_request(
        cell,
        policy,
        &request.program,
        &request.args,
        &request.cwd,
        &request.environment,
        Some(timeout_ms),
    )?;

    let mut process =
        v1::spawn(mxc_req, SpawnOptions::default()).map_err(|err| map_mxc_error(&err))?;
    let stdout_closer = process.stdout_closer();
    let stderr_closer = process.stderr_closer();

    let stdin_pipe = process.take_stdin();
    let stdout_pipe = process.take_stdout();
    let stderr_pipe = process.take_stderr();

    let stdin_payload = request.stdin.clone();
    let stdin_thread = stdin_pipe.map(|mut stdin| {
        std::thread::spawn(move || {
            if !stdin_payload.is_empty() {
                let _ = stdin.write_all(&stdin_payload);
                let _ = stdin.flush();
            }
            // Drop closes stdin so the child sees EOF.
        })
    });

    let stdout_thread = std::thread::spawn(move || read_bounded(stdout_pipe, max_output));
    let stderr_thread = std::thread::spawn(move || read_bounded(stderr_pipe, max_output));

    let wait_outcome = process.wait();
    let warnings = process.warnings();
    let denial_capture = process
        .output_metadata()
        .and_then(|meta| meta.capture_denials.as_ref())
        .map(summary_from_mxc);
    drop(process);

    // Cancel parked reads when the process timed out or failed to wait cleanly.
    if !matches!(wait_outcome, Ok(WaitResult::Exited(_))) {
        if let Some(closer) = stdout_closer {
            closer.close();
        }
        if let Some(closer) = stderr_closer {
            closer.close();
        }
    }

    if let Some(handle) = stdin_thread {
        let _ = handle.join();
    }
    let (stdout, stdout_truncated) = stdout_thread
        .join()
        .unwrap_or_else(|_| Err(io::Error::other("stdout capture thread panicked")))
        .map_err(|error| SandboxError::Execution(format!("stdout read failed: {error}")))?;
    let (stderr, stderr_truncated) = stderr_thread
        .join()
        .unwrap_or_else(|_| Err(io::Error::other("stderr capture thread panicked")))
        .map_err(|error| SandboxError::Execution(format!("stderr read failed: {error}")))?;

    let (exit_code, timed_out) = match wait_outcome {
        Ok(WaitResult::Exited(code)) => (Some(code), false),
        Ok(WaitResult::TimedOut) => (None, true),
        Err(error) => {
            return Err(SandboxError::Execution(format!(
                "waiting for sandboxed process failed: {error}"
            )));
        }
    };

    Ok(ExecResult {
        exit_code,
        stdout,
        stderr,
        timed_out,
        stdout_truncated,
        stderr_truncated,
        warnings,
        denial_capture,
    })
}

/// Start a long-lived containerized process with live bidirectional stdio pipes.
///
/// Unlike [`run`], no wall-clock timeout is set; the caller controls the
/// lifecycle via [`SandboxedProcess::terminate`] or `Drop`.
pub fn spawn_streaming(
    cell: &CellSpec,
    policy: &SandboxPolicy,
    request: &SpawnRequest,
) -> Result<SandboxedProcess, SandboxError> {
    validate_scope(cell.scope())?;
    validate_executable(&request.program)?;
    validate_policy(policy, &request.cwd)?;
    prepare_cell(cell)?;

    let mxc_req = build_mxc_request(
        cell,
        policy,
        &request.program,
        &request.args,
        &request.cwd,
        &request.environment,
        None,
    )?;
    let process = v1::spawn(mxc_req, SpawnOptions::default()).map_err(|err| map_mxc_error(&err))?;
    Ok(SandboxedProcess::from_mxc(process))
}

/// Run a PowerShell script inside the cell's MXC container.
pub fn run_script(
    cell: &CellSpec,
    policy: &SandboxPolicy,
    script: &str,
) -> Result<ExecResult, SandboxError> {
    let layout = cell.layout();
    let request = powershell_request(script, cell.root(), &layout)?;
    run(cell, policy, &request)
}

/// Run a one-shot Agent host command inside the cell's MXC container.
pub fn run_agent(
    cell: &CellSpec,
    policy: &SandboxPolicy,
    program: &Path,
    args: &[OsString],
    extra_environment: &[(OsString, OsString)],
) -> Result<ExecResult, SandboxError> {
    validate_executable(program)?;
    let layout = cell.layout();
    let home = layout.home();
    let temp = layout.temp();
    let mut environment = powershell_environment(&home, &temp);
    for (key, value) in extra_environment {
        if !valid_environment_key(key) {
            continue;
        }
        environment.retain(|(existing, _)| {
            !existing
                .to_string_lossy()
                .eq_ignore_ascii_case(&key.to_string_lossy())
        });
        environment.push((key.clone(), value.clone()));
    }
    let request = ExecRequest {
        program: program.to_path_buf(),
        args: args.to_vec(),
        cwd: cell.root().to_path_buf(),
        environment,
        stdin: Vec::new(),
    };
    run(cell, policy, &request)
}

fn validate_executable(program: &Path) -> Result<(), SandboxError> {
    if !program.is_absolute() || !program.is_file() {
        return Err(SandboxError::PolicyRejected(format!(
            "executable must be an existing absolute file path: {}",
            program.display()
        )));
    }
    Ok(())
}

fn ensure_plain_directory(path: &Path) -> Result<(), SandboxError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        SandboxError::Setup(format!(
            "sandbox directory unavailable {}: {error}",
            path.display()
        ))
    })?;
    if !metadata.is_dir() || is_reparse_point(&metadata) {
        return Err(SandboxError::Setup(format!(
            "sandbox path is not a plain directory (symlinks/junctions refused): {}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(windows)]
fn is_reparse_point(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn is_reparse_point(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

fn build_mxc_request(
    cell: &CellSpec,
    policy: &SandboxPolicy,
    program: &Path,
    args: &[OsString],
    cwd: &Path,
    environment: &[(OsString, OsString)],
    timeout_ms: Option<u32>,
) -> Result<ContainerRequest, SandboxError> {
    let command = format_command_line(program, args)?;
    let env_entries = convert_environment(environment)?;

    let mut readwrite_paths = dedup_paths(&policy.writable);
    let mut readonly_paths = dedup_paths(&policy.readable);
    let denied_paths = dedup_paths(&policy.deny_read);
    let enumerate_paths = dedup_paths(&policy.enumerate);

    // Ensure the executable's own directory is readable inside deny-by-default
    // containers unless it is already under system roots or an existing grant.
    if let Some(exe_dir) = program.parent().map(strip_verbatim)
        && exe_dir.is_dir()
        && !is_windows_system_dir(&exe_dir)
        && !policy
            .writable
            .iter()
            .chain(&policy.readable)
            .any(|root| path_within(&exe_dir, root))
        && !policy
            .deny_read
            .iter()
            .any(|denied| path_within(&exe_dir, denied))
    {
        push_unique_path(&mut readonly_paths, &exe_dir);
    }

    // Make sure the cell scratch layout is writable when cell root is writable.
    if policy
        .writable
        .iter()
        .any(|root| path_within(cell.root(), root))
    {
        push_unique_path(&mut readwrite_paths, cell.root());
    }

    let filesystem = FilesystemPolicy {
        readwrite_paths,
        readonly_paths,
        denied_paths,
        clear_policy_on_exit: Some(true),
    };

    let network = build_mxc_network(&policy.network);
    let ui = build_mxc_ui(policy.ui);
    let containment = build_mxc_containment(&enumerate_paths, policy.capture_denials.as_ref());

    Ok(ContainerRequest {
        command,
        filesystem: Some(filesystem),
        network: Some(network),
        ui: Some(ui),
        timeout_ms,
        containment,
        container_name: Some(format!("duthub-{}", cell.scope())),
        working_directory: Some(path_to_string(&strip_verbatim(cwd))),
        environment: Some(env_entries),
        inherit_default_environment: Some(false),
    })
}

fn build_mxc_network(policy: &NetworkPolicy) -> MxcNetworkPolicy {
    match policy {
        NetworkPolicy::Deny => MxcNetworkPolicy {
            egress: Some(NetworkEgressPolicy {
                default: Some(NetworkAction::Deny),
                allow: None,
                deny: None,
            }),
            ingress: Some(NetworkIngressPolicy {
                default: Some(NetworkAction::Deny),
                host_loopback: Some(NetworkAction::Deny),
            }),
            runtime_config: None,
        },
        NetworkPolicy::LoopbackOnly => MxcNetworkPolicy {
            egress: Some(NetworkEgressPolicy {
                default: Some(NetworkAction::Deny),
                allow: Some(vec![NetworkRulePolicy {
                    to: Some(vec![
                        NetworkPeerPolicy::new("127.0.0.1/32"),
                        NetworkPeerPolicy::new("::1/128"),
                    ]),
                    ports: None,
                }]),
                deny: None,
            }),
            ingress: Some(NetworkIngressPolicy {
                default: Some(NetworkAction::Deny),
                host_loopback: Some(NetworkAction::Allow),
            }),
            runtime_config: None,
        },
        NetworkPolicy::LoopbackAndOutbound { proxy_url } => {
            if let Some(url) = proxy_url {
                // MXC requires deny-by-default egress when a runtime proxy is set.
                MxcNetworkPolicy {
                    egress: Some(NetworkEgressPolicy {
                        default: Some(NetworkAction::Deny),
                        allow: None,
                        deny: None,
                    }),
                    ingress: Some(NetworkIngressPolicy {
                        default: Some(NetworkAction::Deny),
                        host_loopback: Some(NetworkAction::Allow),
                    }),
                    runtime_config: Some(NetworkRuntimeConfig {
                        network_proxy: Some(url.clone()),
                    }),
                }
            } else {
                MxcNetworkPolicy {
                    egress: Some(NetworkEgressPolicy {
                        default: Some(NetworkAction::Allow),
                        allow: None,
                        deny: None,
                    }),
                    ingress: Some(NetworkIngressPolicy {
                        default: Some(NetworkAction::Deny),
                        host_loopback: Some(NetworkAction::Allow),
                    }),
                    runtime_config: None,
                }
            }
        }
    }
}

fn build_mxc_ui(ui: UiPolicy) -> MxcUiPolicy {
    let clipboard = match (ui.allow_clipboard_read, ui.allow_clipboard_write) {
        (false, false) => ClipboardPolicy::None,
        (true, false) => ClipboardPolicy::Read,
        (false, true) => ClipboardPolicy::Write,
        (true, true) => ClipboardPolicy::All,
    };
    MxcUiPolicy {
        disable: ui.block_win32k,
        clipboard,
        allow_input_injection: ui.allow_input_injection,
    }
}

fn build_mxc_containment(
    enumerate_paths: &[String],
    capture_denials: Option<&DenialCaptureConfig>,
) -> Containment {
    #[cfg(target_os = "windows")]
    {
        let mut config = ProcessContainerConfig::default();
        if !enumerate_paths.is_empty() {
            let mut fs = ProcessContainerFilesystem::default();
            fs.enumerate_paths = enumerate_paths.to_vec();
            config.filesystem = Some(fs);
        }
        if let Some(capture) = capture_denials {
            let mut mxc_capture = CaptureDenials::default();
            mxc_capture.mode = CaptureDenialsMode::Block;
            mxc_capture.output_path = capture
                .output_path
                .as_deref()
                .map(|path| path_to_string(&strip_verbatim(path)));
            mxc_capture.retain_etl = capture.retain_etl;
            config.capture_denials = Some(mxc_capture);
        }
        Containment::ProcessContainer(config)
    }

    #[cfg(not(target_os = "windows"))]
    {
        let _ = (enumerate_paths, capture_denials);
        Containment::Process
    }
}

fn convert_environment(
    environment: &[(OsString, OsString)],
) -> Result<Vec<(String, String)>, SandboxError> {
    let mut out = Vec::with_capacity(environment.len());
    for (key, value) in environment {
        let key_str = key.to_str().ok_or_else(|| {
            SandboxError::PolicyRejected("environment key is not valid UTF-8".to_owned())
        })?;
        let val_str = value.to_str().ok_or_else(|| {
            SandboxError::PolicyRejected(format!(
                "environment value for {key_str:?} is not valid UTF-8"
            ))
        })?;
        if key_str.is_empty() || key_str.contains(['=', '\0']) || val_str.contains('\0') {
            return Err(SandboxError::PolicyRejected(format!(
                "invalid environment entry for key {key_str:?}"
            )));
        }
        out.push((key_str.to_owned(), val_str.to_owned()));
    }
    Ok(out)
}

fn is_windows_system_dir(path: &Path) -> bool {
    #[cfg(windows)]
    {
        let win_dir = std::env::var_os("SystemRoot")
            .or_else(|| std::env::var_os("windir"))
            .map_or_else(|| PathBuf::from(r"C:\Windows"), PathBuf::from);
        path_within(path, &win_dir)
    }
    #[cfg(not(windows))]
    {
        let _ = path;
        false
    }
}

fn dedup_paths(paths: &[PathBuf]) -> Vec<String> {
    let mut out = Vec::new();
    for path in paths {
        push_unique_path(&mut out, &strip_verbatim(path));
    }
    out
}

fn push_unique_path(list: &mut Vec<String>, path: &Path) {
    let text = path_to_string(path);
    let key = normalize_key(&text);
    let mut seen: HashSet<String> = list.iter().map(|s| normalize_key(s)).collect();
    if seen.insert(key) {
        list.push(text);
    }
}

fn normalize_key(path: &str) -> String {
    #[cfg(windows)]
    {
        path.to_ascii_lowercase()
    }
    #[cfg(not(windows))]
    {
        path.to_owned()
    }
}

fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn read_bounded(stream: Option<Box<dyn Read + Send>>, limit: usize) -> io::Result<(Vec<u8>, bool)> {
    let Some(mut stream) = stream else {
        return Ok((Vec::new(), false));
    };
    let mut collected = Vec::new();
    let mut truncated = false;
    let mut chunk = [0u8; 8192];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if collected.len() < limit {
                    let take = (limit - collected.len()).min(n);
                    collected.extend_from_slice(&chunk[..take]);
                    if take < n {
                        truncated = true;
                    }
                } else {
                    truncated = true;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => break,
            Err(error) => return Err(error),
        }
    }
    Ok((collected, truncated))
}

fn summary_from_mxc(capture: &v1::CaptureDenialsResult) -> DenialCaptureSummary {
    DenialCaptureSummary {
        json_path: PathBuf::from(&capture.output_path),
        total_denials: capture.total_denials,
        etl_path: capture.etl_path.as_ref().map(PathBuf::from),
    }
}

fn map_mxc_error(error: &MxcError) -> SandboxError {
    let message = error.to_string();
    match error.code {
        ErrorCode::BackendUnavailable | ErrorCode::UnsupportedContainment => {
            SandboxError::Unsupported(message)
        }
        ErrorCode::MalformedRequest | ErrorCode::PolicyValidation => {
            SandboxError::PolicyRejected(message)
        }
        ErrorCode::BackendError => SandboxError::Spawn(message),
        _ => SandboxError::Execution(message),
    }
}
