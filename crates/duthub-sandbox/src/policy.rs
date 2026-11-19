//! Pure policy layer: what a sandboxed command may touch, where it runs, and
//! what the host will let it say.
//!
//! This module makes no Win32 calls and holds no `unsafe`; it only inspects
//! paths with `std::fs` to reject malformed roots. Every rule here is a bound
//! the backend must enforce in the kernel; `tests/security_invariants.rs`
//! proves the backend does, and the unit tests below prove the bounds are not
//! advisory.

use std::{
    ffi::OsString,
    path::{Component, Path, PathBuf},
    time::Duration,
};

/// Longest a single command may run, whatever the policy asks for.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(600);
/// Largest `max_output_bytes` a policy may request, per stream.
pub const MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
/// Largest concurrent-process count a policy may request.
pub const MAX_PROCESS_LIMIT: u32 = 64;
/// Smallest per-process memory limit a policy may request.
pub const MIN_MEMORY_LIMIT_BYTES: u64 = 64 * 1024 * 1024;
/// Largest per-process memory limit a policy may request (8 GiB).
pub const MAX_MEMORY_LIMIT_BYTES: u64 = 8 * 1024 * 1024 * 1024;
/// Timeout used when the policy leaves it at zero.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
/// Per-process memory limit used when the policy leaves it unset.
pub const DEFAULT_MEMORY_LIMIT_BYTES: u64 = 2 * 1024 * 1024 * 1024;
/// Concurrent-process limit used when the policy leaves it unset.
pub const DEFAULT_PROCESS_LIMIT: u32 = 32;
/// Default per-stream output cap when the policy leaves it at zero.
pub const DEFAULT_OUTPUT_BYTES: usize = 64 * 1024;

const SCOPE_MAX_CHARS: usize = 64;
const DUT_ID_MAX_CHARS: usize = 256;
/// PowerShell scripts travel base64-encoded on the command line, so they are
/// bounded well below the Windows command-line limit.
pub const MAX_SCRIPT_CHARS: usize = 8_000;

/// Which kernel identity a cell's child processes run under.
///
/// The two modes exist because two callers have different requirements; they
/// are not interchangeable and the policy layer refuses to let one drift into
/// the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IsolationMode {
    /// `WRITE_RESTRICTED` token + per-DUT capability SID + Low integrity.
    ///
    /// Writes are limited to the cell and reads of the protected paths are
    /// blocked by a `NO_READ_UP` mandatory label. The host's network stack is
    /// untouched, which is what the Agent host needs to reach the loopback MCP
    /// listeners.
    RestrictedToken,
    /// AppContainer profile with zero capabilities.
    ///
    /// No network (loopback included), no read outside explicit grants, Low
    /// integrity. Used for every script the model can influence.
    AppContainer,
}

/// Network reachability granted to a command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkPolicy {
    /// No network at all, loopback included (AppContainer with no capability).
    Deny,
    /// No network isolation: the child keeps the host's networking, including
    /// loopback. Only meaningful for [`IsolationMode::RestrictedToken`],
    /// because that mode does not isolate the network at all.
    Host,
}

/// What a command may touch.
///
/// These are the bounds the backend must enforce. `deny_read` items are the
/// paths whose contents must never reach the child (host credentials, other
/// DUTs' evidence, the Agent's own install directory).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxPolicy {
    pub mode: IsolationMode,
    /// Directory trees the command may read and list (read + execute).
    pub readable: Vec<PathBuf>,
    /// Directory trees the command may read and write. A backend must deny
    /// writes elsewhere through the token, not through a path check.
    pub writable: Vec<PathBuf>,
    /// Directory trees the child must not be able to read, even if the
    /// platform would otherwise allow it.
    pub deny_read: Vec<PathBuf>,
    pub network: NetworkPolicy,
    /// Wall-clock limit for the whole process tree. Zero means
    /// [`DEFAULT_TIMEOUT`]; values above [`MAX_TIMEOUT`] are rejected.
    pub timeout: Duration,
    /// Commit limit per process. `None` means [`DEFAULT_MEMORY_LIMIT_BYTES`].
    pub memory_limit_bytes: Option<u64>,
    /// Concurrent processes in the tree. `None` means
    /// [`DEFAULT_PROCESS_LIMIT`].
    pub process_limit: Option<u32>,
    /// Per-stream (stdout, stderr) cap; anything beyond is drained and counted
    /// as truncated. Zero means [`DEFAULT_OUTPUT_BYTES`].
    pub max_output_bytes: usize,
}

impl SandboxPolicy {
    /// Read and write access to exactly one tree, no network, default limits.
    pub fn scratch(root: impl Into<PathBuf>) -> Self {
        Self {
            mode: IsolationMode::AppContainer,
            readable: Vec::new(),
            writable: vec![root.into()],
            deny_read: Vec::new(),
            network: NetworkPolicy::Deny,
            timeout: Duration::ZERO,
            memory_limit_bytes: None,
            process_limit: None,
            max_output_bytes: 0,
        }
    }

    /// The Agent host (`goose`) policy: write-isolated by a restricted token,
    /// host networking (needed for the loopback MCP listeners).
    ///
    /// `deny_read` is intentionally empty: a `WRITE_RESTRICTED` token consults
    /// its restricting SIDs for **writes only**, so a deny-read ACE cannot be
    /// enforced in this mode (verified 2026-11-25, ADR-12 §5). Anything that
    /// needs read isolation uses [`Self::script_cell`]'s AppContainer instead.
    pub fn agent_cell(cell: &CellSpec) -> Self {
        Self {
            mode: IsolationMode::RestrictedToken,
            readable: Vec::new(),
            writable: vec![cell.root().to_path_buf()],
            deny_read: Vec::new(),
            network: NetworkPolicy::Host,
            timeout: Duration::ZERO,
            memory_limit_bytes: None,
            process_limit: None,
            max_output_bytes: 0,
        }
    }

    /// The script policy: AppContainer, no network, the cell writable and the
    /// protected paths unreadable.
    pub fn script_cell(cell: &CellSpec) -> Self {
        Self {
            mode: IsolationMode::AppContainer,
            readable: Vec::new(),
            writable: vec![cell.root().to_path_buf()],
            deny_read: cell.protected_paths(),
            network: NetworkPolicy::Deny,
            timeout: Duration::ZERO,
            memory_limit_bytes: None,
            process_limit: None,
            max_output_bytes: 0,
        }
    }

    pub fn timeout_or_default(&self) -> Duration {
        if self.timeout.is_zero() {
            DEFAULT_TIMEOUT
        } else {
            self.timeout
        }
    }

    pub fn process_limit_or_default(&self) -> u32 {
        self.process_limit.unwrap_or(DEFAULT_PROCESS_LIMIT)
    }

    pub fn memory_limit_or_default(&self) -> u64 {
        self.memory_limit_bytes
            .unwrap_or(DEFAULT_MEMORY_LIMIT_BYTES)
    }

    pub fn max_output_or_default(&self) -> usize {
        if self.max_output_bytes == 0 {
            DEFAULT_OUTPUT_BYTES
        } else {
            self.max_output_bytes
        }
    }
}

/// One command to run inside the sandbox.
///
/// Built by trusted host code ([`powershell_request`], `run_agent`), never
/// passed through from an LLM: the program, arguments, working directory and
/// environment are the host's decision; untrusted content only ever reaches the
/// encoded script.
#[derive(Debug, Clone)]
pub struct ExecRequest {
    /// Absolute path of the executable. Never searched on `PATH`.
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// Must lie inside one of the policy's writable roots.
    pub cwd: PathBuf,
    /// The complete environment of the child. Nothing is inherited.
    pub environment: Vec<(OsString, OsString)>,
    pub stdin: Vec<u8>,
}

/// One long-lived command: the Agent host's ACP session process.
///
/// [`ExecRequest`] describes a command that must finish inside the policy
/// timeout with a fixed stdin payload. An ACP session is the opposite: stdin
/// stays open, stdout/stderr are read while the process runs, and there is no
/// wall-clock limit. The scheduler and pipes are the same; only the lifecycle
/// differs (see [`crate::spawn_streaming`]).
#[derive(Debug, Clone)]
pub struct SpawnRequest {
    /// Absolute path of the executable. Never searched on `PATH`.
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// Must lie inside one of the policy's writable roots.
    pub cwd: PathBuf,
    /// The complete environment of the child. Nothing is inherited.
    pub environment: Vec<(OsString, OsString)>,
}

/// Outcome of one sandboxed command.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecResult {
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// The policy timeout elapsed and the process tree was terminated.
    pub timed_out: bool,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    /// The executor read the child's token back and it was the sandbox token
    /// (restricted, or an AppContainer). A backend that cannot prove this must
    /// return [`SandboxError::NotSandboxed`] instead of a result.
    pub sandboxed: bool,
}

#[derive(Debug)]
pub enum SandboxError {
    /// No sandbox backend exists for this platform (or this build). The
    /// command must not run; there is deliberately no unsandboxed fallback.
    Unsupported(&'static str),
    /// The scope, request or policy is malformed or asks for too much.
    PolicyRejected(String),
    /// The sandbox could not be established (ACLs, AppContainer SID, Job).
    Setup(String),
    /// The sandbox was established but the process could not be started.
    Spawn(String),
    /// The process started but did not end up under the sandbox token. It was
    /// terminated while still suspended.
    NotSandboxed(String),
    /// The command ran but its output or exit status could not be collected.
    Execution(String),
}

impl std::fmt::Display for SandboxError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SandboxError::Unsupported(what) => {
                write!(formatter, "沙箱后端不可用（{what}），已拒绝执行")
            }
            SandboxError::PolicyRejected(reason) => {
                write!(formatter, "沙箱策略被拒绝: {reason}")
            }
            SandboxError::Setup(reason) => write!(formatter, "沙箱建立失败: {reason}"),
            SandboxError::Spawn(reason) => write!(formatter, "沙箱内进程启动失败: {reason}"),
            SandboxError::NotSandboxed(reason) => {
                write!(formatter, "子进程未处于沙箱令牌下，已终止: {reason}")
            }
            SandboxError::Execution(reason) => write!(formatter, "沙箱命令收尾失败: {reason}"),
        }
    }
}

impl std::error::Error for SandboxError {}

impl From<std::io::Error> for SandboxError {
    fn from(error: std::io::Error) -> Self {
        SandboxError::Setup(error.to_string())
    }
}

/// FNV-1a over a UTF-8 string: a stable 64-bit digest for bounded scope names.
fn fnv1a(text: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// AppContainer profile name for `scope`. The name determines the container
/// SID, so it must be stable for a given scope and must not collide across
/// scopes; [`validate_scope`] keeps it free of path and separator characters.
pub fn profile_name(scope: &str) -> String {
    format!("duthub.sandbox.{scope}")
}

/// Reject scopes that would make ambiguous or forgeable profile names.
pub fn validate_scope(scope: &str) -> Result<(), SandboxError> {
    let valid = !scope.is_empty()
        && scope.len() <= SCOPE_MAX_CHARS
        && scope
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if valid {
        Ok(())
    } else {
        Err(SandboxError::PolicyRejected(format!(
            "沙箱作用域非法（仅限 1-{SCOPE_MAX_CHARS} 位字母、数字、- _ .）: {scope:?}"
        )))
    }
}

/// Absolute, lexically normal (no `..`) and an existing directory.
fn is_plain_root(path: &Path) -> bool {
    path.is_absolute()
        && !path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        && path.is_dir()
}

/// Check a policy and the working directory it will be used with.
pub fn validate_policy(policy: &SandboxPolicy, cwd: &Path) -> Result<(), SandboxError> {
    if policy.timeout_or_default() > MAX_TIMEOUT {
        return Err(SandboxError::PolicyRejected(format!(
            "超时上限为 {} 秒",
            MAX_TIMEOUT.as_secs()
        )));
    }
    if policy.max_output_or_default() > MAX_OUTPUT_BYTES {
        return Err(SandboxError::PolicyRejected(format!(
            "输出上限为 {MAX_OUTPUT_BYTES} 字节"
        )));
    }
    if policy.process_limit_or_default() > MAX_PROCESS_LIMIT {
        return Err(SandboxError::PolicyRejected(format!(
            "进程数上限为 {MAX_PROCESS_LIMIT}"
        )));
    }
    if policy.memory_limit_or_default() < MIN_MEMORY_LIMIT_BYTES {
        return Err(SandboxError::PolicyRejected(format!(
            "内存下限为 {MIN_MEMORY_LIMIT_BYTES} 字节"
        )));
    }
    if policy.memory_limit_or_default() > MAX_MEMORY_LIMIT_BYTES {
        return Err(SandboxError::PolicyRejected(format!(
            "内存上限为 {MAX_MEMORY_LIMIT_BYTES} 字节"
        )));
    }
    match (policy.mode, policy.network) {
        (IsolationMode::AppContainer, NetworkPolicy::Host) => {
            return Err(SandboxError::PolicyRejected(
                "AppContainer 模式未实现网络能力，只能拒绝网络".to_owned(),
            ));
        }
        (IsolationMode::RestrictedToken, NetworkPolicy::Deny) => {
            return Err(SandboxError::PolicyRejected(
                "受限令牌不隔离网络，不能声明 Deny".to_owned(),
            ));
        }
        _ => {}
    }
    if policy.mode == IsolationMode::RestrictedToken && !policy.deny_read.is_empty() {
        // A write-restricted token cannot enforce deny-read: its restricting
        // SIDs are only consulted for writes. Refusing here keeps callers from
        // believing a path is hidden when it is not.
        return Err(SandboxError::PolicyRejected(
            "受限令牌模式不能声明 deny_read（内核只对写检查 restricting SID），需要读隔离请改用 AppContainer"
                .to_owned(),
        ));
    }
    if policy.writable.is_empty() {
        return Err(SandboxError::PolicyRejected(
            "至少需要一个可写目录".to_owned(),
        ));
    }
    for root in policy
        .readable
        .iter()
        .chain(&policy.writable)
        .chain(&policy.deny_read)
    {
        if !is_plain_root(root) {
            return Err(SandboxError::PolicyRejected(format!(
                "沙箱目录必须是已存在的绝对路径目录，且不含 `..`: {}",
                root.display()
            )));
        }
    }
    if !is_plain_root(cwd) {
        return Err(SandboxError::PolicyRejected(format!(
            "工作目录必须是已存在的绝对路径目录，且不含 `..`: {}",
            cwd.display()
        )));
    }
    let inside_writable = policy
        .writable
        .iter()
        .any(|root| cwd == root || cwd.starts_with(root));
    if !inside_writable {
        return Err(SandboxError::PolicyRejected(format!(
            "工作目录必须位于可写目录内: {}",
            cwd.display()
        )));
    }
    // A writable or readable root that also appears in `deny_read` is a
    // contradiction the backend must not be asked to resolve.
    for denied in &policy.deny_read {
        for allowed in policy.writable.iter().chain(&policy.readable) {
            if denied == allowed || allowed.starts_with(denied) {
                return Err(SandboxError::PolicyRejected(format!(
                    "拒绝读取的目录同时被授权: {}",
                    denied.display()
                )));
            }
        }
    }
    Ok(())
}

/// Where a sandboxed command's private `TEMP` and profile live: inside the
/// writable root, so anything that insists on a "user profile" writes into the
/// cell instead of being handed the real one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScratchLayout {
    root: PathBuf,
}

impl ScratchLayout {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn temp(&self) -> PathBuf {
        self.root.join("tmp")
    }

    pub fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    /// Directories the host must create before launching a command.
    pub fn required_dirs(&self) -> Vec<PathBuf> {
        let home = self.home();
        vec![
            self.temp(),
            home.clone(),
            home.join("AppData").join("Local"),
            home.join("AppData").join("Roaming"),
        ]
    }
}

/// One isolation cell: the directory tree a single DUT's Agent may use.
///
/// Layout follows the Agent host's existing convention:
/// `<work_dir>/agent/dut/<hex(dut_id)>/sandbox`, with `tmp` and `home` inside.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellSpec {
    scope: String,
    work_dir: PathBuf,
    dut_key: String,
    root: PathBuf,
}

impl CellSpec {
    /// A cell at `root` with an explicit scope (tests, embedded use).
    pub fn new(root: impl Into<PathBuf>, scope: &str) -> Result<Self, SandboxError> {
        validate_scope(scope)?;
        let root = root.into();
        if !root.is_absolute() {
            return Err(SandboxError::PolicyRejected(format!(
                "沙箱单元必须是绝对路径: {}",
                root.display()
            )));
        }
        let work_dir = work_dir_of(&root).ok_or_else(|| {
            SandboxError::PolicyRejected(format!(
                "无法从沙箱单元推导工作目录（期望 <work_dir>/agent/dut/<key>/sandbox）: {}",
                root.display()
            ))
        })?;
        let dut_key = root
            .parent()
            .and_then(Path::file_name)
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        Ok(Self {
            scope: scope.to_owned(),
            work_dir,
            dut_key,
            root,
        })
    }

    /// A cell rooted at an arbitrary absolute directory (tests, embedded use).
    ///
    /// [`CellSpec::new`] insists on the `<work_dir>/agent/dut/<key>/sandbox`
    /// layout because [`CellSpec::protected_paths`] derives the host's config
    /// and evidence paths from it. Callers that only need the writable root —
    /// the restricted-token Agent policy never uses `deny_read` — can point the
    /// cell at the directory they already own.
    pub fn at(root: impl Into<PathBuf>, scope: &str) -> Result<Self, SandboxError> {
        validate_scope(scope)?;
        let root = root.into();
        if !root.is_absolute() {
            return Err(SandboxError::PolicyRejected(format!(
                "沙箱单元必须是绝对路径: {}",
                root.display()
            )));
        }
        let dut_key = root
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        let work_dir = root
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| root.clone());
        Ok(Self {
            scope: scope.to_owned(),
            work_dir,
            dut_key,
            root,
        })
    }

    /// The per-DUT cell for `dut_id` under `work_dir`.
    ///
    /// The directory name is the hex encoding of the UTF-8 id: injective,
    /// case-insensitive-filesystem safe, and free of separators, so two DUTs can
    /// never share a cell and a DUT id cannot escape the tree.
    pub fn for_dut(work_dir: &Path, dut_id: &str) -> Result<Self, SandboxError> {
        if dut_id.trim().is_empty() || dut_id.chars().count() > DUT_ID_MAX_CHARS {
            return Err(SandboxError::PolicyRejected(format!(
                "DUT id 为空或过长（上限 {DUT_ID_MAX_CHARS} 字符）"
            )));
        }
        if !work_dir.is_absolute() {
            return Err(SandboxError::PolicyRejected(format!(
                "工作目录必须是绝对路径: {}",
                work_dir.display()
            )));
        }
        let dut_key: String = dut_id.bytes().map(|byte| format!("{byte:02x}")).collect();
        // The scope is a profile/audit name, not a path. A long DUT id (a UUID
        // is 36 chars → 72 hex) would exceed SCOPE_MAX_CHARS and be rejected by
        // validate_scope, so long keys fall back to a stable hash.
        let scope = if dut_key.len() + "dut-".len() <= SCOPE_MAX_CHARS {
            format!("dut-{dut_key}")
        } else {
            format!("dut-{:016x}", fnv1a(&dut_key))
        };
        let root = work_dir
            .join("agent")
            .join("dut")
            .join(&dut_key)
            .join("sandbox");
        Ok(Self {
            scope,
            work_dir: work_dir.to_path_buf(),
            dut_key,
            root,
        })
    }

    pub fn scope(&self) -> &str {
        &self.scope
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn work_dir(&self) -> &Path {
        &self.work_dir
    }

    /// Hex-encoded DUT directory name under `<work_dir>/agent/dut`.
    pub fn dut_key(&self) -> &str {
        &self.dut_key
    }

    pub fn layout(&self) -> ScratchLayout {
        ScratchLayout::new(&self.root)
    }

    /// Directories that must exist before a launch.
    pub fn required_dirs(&self) -> Vec<PathBuf> {
        self.layout().required_dirs()
    }

    /// Paths the cell's children must never read: the host's config directory
    /// (settings and, through the keyring, everything derived from them), the
    /// host's evidence directory, the sibling DUT cells and the user's SSH
    /// material.
    pub fn protected_paths(&self) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        let duthub_dir = host_config_dir();
        if let Some(dir) = duthub_dir {
            paths.push(dir);
        }
        let data_dir = self.work_dir.join("duthub-data");
        if data_dir.is_dir() {
            paths.push(data_dir);
        }
        let agents_file = self.work_dir.join("agents.json");
        if agents_file.is_file() {
            paths.push(agents_file);
        }
        if let Some(user_profile) = std::env::var_os("USERPROFILE") {
            let profile = PathBuf::from(user_profile);
            for rel in [".ssh", ".aws", ".kube", ".docker", ".gitconfig"] {
                let candidate = profile.join(rel);
                if candidate.exists() {
                    paths.push(candidate);
                }
            }
        }
        let dut_parent = self.work_dir.join("agent").join("dut");
        if let Ok(entries) = std::fs::read_dir(&dut_parent) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path == self.root.parent().unwrap_or(&self.root) {
                    continue;
                }
                if path.is_dir() {
                    paths.push(path);
                }
            }
        }
        paths
    }
}

/// `<exe dir>/.duthub`, the host's configuration directory (ADR: portable
/// layout). `None` when the executable path is unavailable.
fn host_config_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?.join(".duthub");
    dir.is_dir().then_some(dir)
}

/// Recover `<work_dir>` from `<work_dir>/agent/dut/<key>/sandbox`.
fn work_dir_of(root: &Path) -> Option<PathBuf> {
    let sandbox = root;
    let dut_key = sandbox.parent()?;
    let dut = dut_key.parent()?;
    let agent = dut.parent()?;
    let work_dir = agent.parent()?;
    if sandbox.file_name()? != "sandbox"
        || dut.file_name()? != "dut"
        || agent.file_name()? != "agent"
    {
        return None;
    }
    Some(work_dir.to_path_buf())
}

/// Where Windows PowerShell 5.1 lives: a fixed, absolute system path. It is
/// never resolved through `PATH`, so a planted `powershell.exe` earlier on
/// `PATH` cannot be picked up instead of the real one.
pub fn windows_powershell() -> PathBuf {
    std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .filter(|root| root.is_absolute())
        .map(|root| {
            root.join("System32")
                .join("WindowsPowerShell")
                .join("v1.0")
                .join("powershell.exe")
        })
        .unwrap_or_else(|| {
            PathBuf::from(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe")
        })
}

/// UTF-16LE base64 for PowerShell's `-EncodedCommand`.
pub fn encode_script(script: &str) -> String {
    use base64::Engine as _;
    let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// Preamble prepended to every script: UTF-8 output (the default OEM code
/// page would mangle non-ASCII output) and no telemetry.
fn preamble() -> &'static str {
    concat!(
        "$OutputEncoding = [System.Text.UTF8Encoding]::new($false)\n",
        "try { [Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false) } catch {}\n",
        "$env:POWERSHELL_TELEMETRY_OPTOUT = '1'\n",
        "$env:DOTNET_CLI_TELEMETRY_OPTOUT = '1'\n",
        // A lowbox can access its cell without being able to resolve the volume's PSDrive.
        "try { Set-Location -LiteralPath $env:DUTHUB_SANDBOX_CWD -ErrorAction Stop } catch {\n",
        "    New-PSDrive -Name DuthubCell -PSProvider FileSystem -Root $env:DUTHUB_SANDBOX_CWD -ErrorAction Stop | Out-Null\n",
        "    Set-Location -LiteralPath 'DuthubCell:\\' -ErrorAction Stop\n",
        "}\n",
    )
}

/// Windows PowerShell request for `script`, run with `cwd` as the working
/// directory.
///
/// The script is base64-encoded UTF-16LE (`-EncodedCommand`), so it reaches
/// PowerShell byte-exact regardless of the machine's code pages and never
/// appears in a shell command string. The environment is built from scratch by
/// [`powershell_environment`]: a few system facts, the cell's private paths,
/// and nothing from the host's environment that could carry a credential.
pub fn powershell_request(
    script: &str,
    cwd: &Path,
    scratch: &ScratchLayout,
) -> Result<ExecRequest, SandboxError> {
    let denial = script_denial_reason(script);
    if let Some((pattern, reason)) = denial {
        return Err(SandboxError::PolicyRejected(format!(
            "脚本包含被禁止的破坏性操作（{reason}，匹配 `{pattern}`）"
        )));
    }
    if script.chars().count() > MAX_SCRIPT_CHARS {
        return Err(SandboxError::PolicyRejected(format!(
            "PowerShell 脚本过长（上限 {MAX_SCRIPT_CHARS} 字符）"
        )));
    }
    if script.contains('\0') {
        return Err(SandboxError::PolicyRejected(
            "PowerShell 脚本含有 NUL 字符".to_owned(),
        ));
    }
    let home = scratch.home();
    let temp = scratch.temp();
    let mut environment = powershell_environment(&home, &temp);
    environment.push(("DUTHUB_SANDBOX_CWD".into(), cwd.as_os_str().to_owned()));
    Ok(ExecRequest {
        program: windows_powershell(),
        args: vec![
            "-NoLogo".into(),
            "-NoProfile".into(),
            "-NonInteractive".into(),
            "-ExecutionPolicy".into(),
            "RemoteSigned".into(),
            "-EncodedCommand".into(),
            encode_script(&format!("{}{script}", preamble())).into(),
        ],
        cwd: cwd.to_path_buf(),
        environment,
        stdin: Vec::new(),
    })
}

/// The complete environment of a sandboxed command: a minimal `PATH` made of
/// system directories only, the cell's private `TEMP`/profile, and three
/// hardware facts. No variable of the host process is copied through except
/// those three, which carry no user or credential data.
pub fn powershell_environment(home: &Path, temp: &Path) -> Vec<(OsString, OsString)> {
    let system_root = std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .filter(|root| root.is_absolute())
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let system32 = system_root.join("System32");
    let path = std::env::join_paths([
        system32.clone(),
        system_root.clone(),
        system32.join("WindowsPowerShell").join("v1.0"),
        system32.join("Wbem"),
    ])
    .unwrap_or_default();
    let local = home.join("AppData").join("Local");
    let roaming = home.join("AppData").join("Roaming");
    let mut environment: Vec<(OsString, OsString)> = vec![
        ("SystemRoot", system_root.clone().into_os_string()),
        ("windir", system_root.into_os_string()),
        ("SystemDrive", drive_of(&system32).into()),
        ("PATH", path),
        ("PATHEXT", ".COM;.EXE;.BAT;.CMD".into()),
        ("COMSPEC", system32.join("cmd.exe").into_os_string()),
        ("TEMP", temp.as_os_str().to_owned()),
        ("TMP", temp.as_os_str().to_owned()),
        ("USERPROFILE", home.as_os_str().to_owned()),
        ("LOCALAPPDATA", local.as_os_str().to_owned()),
        ("APPDATA", roaming.as_os_str().to_owned()),
        ("POWERSHELL_TELEMETRY_OPTOUT", "1".into()),
        ("DOTNET_CLI_TELEMETRY_OPTOUT", "1".into()),
        ("DOTNET_NOLOGO", "1".into()),
    ]
    .into_iter()
    .map(|(key, value): (&str, OsString)| (OsString::from(key), value))
    .collect();
    for key in ["NUMBER_OF_PROCESSORS", "PROCESSOR_ARCHITECTURE", "OS"] {
        if let Some(value) = std::env::var_os(key) {
            environment.push((OsString::from(key), value));
        }
    }
    if let Some((drive, rest)) = split_drive(home) {
        environment.push(("HOMEDRIVE".into(), drive.into()));
        environment.push(("HOMEPATH".into(), rest.into_os_string()));
    }
    environment
}

/// `C:` for `C:\Windows\System32`; `C:` for anything without a drive-letter
/// prefix (relative paths, UNC shares), because the value is only used as the
/// `SystemDrive` environment variable.
fn drive_of(path: &Path) -> String {
    match path.components().next() {
        Some(Component::Prefix(prefix)) if matches!(prefix.kind(), std::path::Prefix::Disk(_)) => {
            prefix
                .as_os_str()
                .to_string_lossy()
                .trim_end_matches('\\')
                .to_owned()
        }
        _ => "C:".to_owned(),
    }
}

/// Split `C:\Users\x` into (`C:\`, `\Users\x`). Returns `None` for paths that
/// are not drive-absolute. The second element keeps its leading separator, the
/// way `HOMEPATH` is conventionally written.
fn split_drive(path: &Path) -> Option<(String, PathBuf)> {
    let mut components = path.components();
    let prefix = match components.next()? {
        Component::Prefix(prefix) => prefix.as_os_str().to_string_lossy().into_owned(),
        _ => return None,
    };
    if !matches!(components.clone().next(), Some(Component::RootDir)) {
        return None;
    }
    let drive = if prefix.ends_with('\\') {
        prefix
    } else {
        format!("{prefix}\\")
    };
    Some((drive, components.as_path().to_path_buf()))
}

/// One hostile command the script layer refuses before spawning anything.
///
/// This list is **defence in depth, not the boundary**: obfuscation defeats
/// substring matching, and the kernel boundary (no network, no writes outside
/// the cell, no reads of host state) is what actually contains a script. The
/// entries below exist because these operations are destructive even *inside*
/// a correctly sandboxed machine (they reach the host through device I/O,
/// firmware or security-centre APIs the token does not gate).
struct ScriptRestriction {
    pattern: &'static str,
    reason: &'static str,
}

const SCRIPT_RESTRICTIONS: &[ScriptRestriction] = &[
    ScriptRestriction {
        pattern: "format-volume",
        reason: "格式化卷",
    },
    ScriptRestriction {
        pattern: "clear-disk",
        reason: "清除磁盘",
    },
    ScriptRestriction {
        pattern: "initialize-disk",
        reason: "初始化磁盘",
    },
    ScriptRestriction {
        pattern: "diskpart",
        reason: "磁盘分区工具",
    },
    ScriptRestriction {
        pattern: "bcdedit",
        reason: "修改启动配置",
    },
    ScriptRestriction {
        pattern: "shutdown",
        reason: "关机/重启",
    },
    ScriptRestriction {
        pattern: "stop-computer",
        reason: "关机/重启",
    },
    ScriptRestriction {
        pattern: "restart-computer",
        reason: "关机/重启",
    },
    ScriptRestriction {
        pattern: "set-mppreference",
        reason: "修改防病毒策略",
    },
    ScriptRestriction {
        pattern: "add-mppreference",
        reason: "修改防病毒策略",
    },
    ScriptRestriction {
        pattern: "remove-mppreference",
        reason: "修改防病毒策略",
    },
    ScriptRestriction {
        pattern: "new-localuser",
        reason: "创建本机账户",
    },
    ScriptRestriction {
        pattern: "add-localgroupmember",
        reason: "修改本机组成员",
    },
    ScriptRestriction {
        pattern: "set-executionpolicy",
        reason: "修改执行策略",
    },
    ScriptRestriction {
        pattern: "takeown",
        reason: "抢占文件所有权",
    },
    ScriptRestriction {
        pattern: "enable-windowsoptionalfeature",
        reason: "修改系统功能",
    },
    ScriptRestriction {
        pattern: "disable-windowsoptionalfeature",
        reason: "修改系统功能",
    },
];

/// System roots whose recursive deletion is always refused.
const SYSTEM_PATHS_FOR_RECURSIVE_DELETE: &[&str] = &[
    "c:\\windows",
    "c:\\program files",
    "c:\\program files (x86)",
    "$env:systemroot",
    "$env:windir",
    "$env:programfiles",
    "c:\\users\\public",
];

/// Why `script` must not run, if it names a known-destructive operation.
///
/// Matching is case-insensitive substring matching with a small structural
/// rule for `Remove-Item -Recurse` on system roots. It returns the pattern that
/// matched so the audit record can say why.
pub fn script_denial_reason(script: &str) -> Option<(&'static str, &'static str)> {
    let lowered = script.to_lowercase();
    for restriction in SCRIPT_RESTRICTIONS {
        if lowered.contains(restriction.pattern) {
            return Some((restriction.pattern, restriction.reason));
        }
    }
    if lowered.contains("remove-item")
        && lowered.contains("-recurse")
        && SYSTEM_PATHS_FOR_RECURSIVE_DELETE
            .iter()
            .any(|prefix| lowered.contains(prefix))
    {
        return Some(("remove-item -recurse <system path>", "递归删除系统目录"));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("duthub-sbx-{tag}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn scope_is_restricted_to_safe_profile_names() {
        assert!(validate_scope("dut-7f3a").is_ok());
        assert!(validate_scope("A_b.0").is_ok());
        for bad in [
            "",
            "  ",
            "dut/a",
            "dut\\a",
            "dut:a",
            "中文",
            &"x".repeat(65),
        ] {
            assert!(validate_scope(bad).is_err(), "{bad:?}");
        }
        assert_eq!(profile_name("dut-1"), "duthub.sandbox.dut-1");
    }

    #[test]
    fn policy_bounds_are_enforced() {
        let root = temp_dir("policy");
        let cell_dir = root.join("cell");
        std::fs::create_dir_all(cell_dir.join("sub")).unwrap();
        let ok = SandboxPolicy::scratch(&cell_dir);
        assert!(validate_policy(&ok, &cell_dir).is_ok());
        assert!(
            validate_policy(&ok, &cell_dir.join("sub")).is_ok(),
            "cwd may be deeper"
        );
        assert!(
            validate_policy(&ok, &root).is_err(),
            "the parent of the cell is not writable"
        );
        let mut p = ok.clone();
        p.timeout = MAX_TIMEOUT + Duration::from_secs(1);
        assert!(validate_policy(&p, &cell_dir).is_err());
        p.timeout = Duration::ZERO;
        p.max_output_bytes = MAX_OUTPUT_BYTES + 1;
        assert!(validate_policy(&p, &cell_dir).is_err());
        p.max_output_bytes = 0;
        p.process_limit = Some(MAX_PROCESS_LIMIT + 1);
        assert!(validate_policy(&p, &cell_dir).is_err());
        p.process_limit = None;
        p.memory_limit_bytes = Some(MIN_MEMORY_LIMIT_BYTES - 1);
        assert!(validate_policy(&p, &cell_dir).is_err());
        p.memory_limit_bytes = Some(MAX_MEMORY_LIMIT_BYTES + 1);
        assert!(validate_policy(&p, &cell_dir).is_err());
        p.memory_limit_bytes = None;
        p.writable.clear();
        assert!(validate_policy(&p, &cell_dir).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn network_and_mode_must_agree() {
        let root = temp_dir("netmode");
        let cell_dir = root.join("cell");
        std::fs::create_dir_all(&cell_dir).unwrap();
        let mut policy = SandboxPolicy::scratch(&cell_dir);
        policy.mode = IsolationMode::AppContainer;
        policy.network = NetworkPolicy::Host;
        assert!(matches!(
            validate_policy(&policy, &cell_dir),
            Err(SandboxError::PolicyRejected(_))
        ));
        policy.mode = IsolationMode::RestrictedToken;
        policy.network = NetworkPolicy::Deny;
        assert!(matches!(
            validate_policy(&policy, &cell_dir),
            Err(SandboxError::PolicyRejected(_))
        ));
        policy.network = NetworkPolicy::Host;
        assert!(validate_policy(&policy, &cell_dir).is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn deny_read_must_not_overlap_grants() {
        let root = temp_dir("overlap");
        let cell_dir = root.join("cell");
        let other = root.join("other");
        std::fs::create_dir_all(&cell_dir).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let mut policy = SandboxPolicy::scratch(&cell_dir);
        policy.deny_read = vec![cell_dir.clone()];
        assert!(matches!(
            validate_policy(&policy, &cell_dir),
            Err(SandboxError::PolicyRejected(_))
        ));
        // Denying a parent of the writable root is also a contradiction: the
        // child could not reach its own cell.
        policy.deny_read = vec![root.clone()];
        assert!(matches!(
            validate_policy(&policy, &cell_dir),
            Err(SandboxError::PolicyRejected(_))
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn scratch_layout_stays_inside_its_root() {
        let layout = ScratchLayout::new(Path::new(r"D:\work\agent\dut\1\sandbox"));
        assert_eq!(layout.temp(), Path::new(r"D:\work\agent\dut\1\sandbox\tmp"));
        assert_eq!(
            layout.home(),
            Path::new(r"D:\work\agent\dut\1\sandbox\home")
        );
        for dir in layout.required_dirs() {
            assert!(dir.starts_with(layout.root()));
        }
        assert_eq!(layout.required_dirs().len(), 4);
    }

    #[test]
    fn cell_layout_matches_the_agent_host_convention() {
        let work = temp_dir("cell");
        let cell = CellSpec::for_dut(&work, "dut-A").unwrap();
        assert_eq!(cell.work_dir(), work.as_path());
        assert_eq!(
            cell.root(),
            work.join("agent")
                .join("dut")
                .join("6475742d41")
                .join("sandbox")
        );
        assert_eq!(cell.scope(), "dut-6475742d41");
        assert_eq!(cell.dut_key(), "6475742d41");
        let rebuilt = CellSpec::new(cell.root(), cell.scope()).unwrap();
        assert_eq!(rebuilt.work_dir(), work.as_path());
        // Distinct ids never share a directory, and separators cannot escape.
        let other = CellSpec::for_dut(&work, "dut-B").unwrap();
        assert_ne!(cell.root(), other.root());
        assert!(
            CellSpec::for_dut(&work, "..\\..\\x")
                .unwrap()
                .root()
                .starts_with(&work)
        );
        assert!(CellSpec::for_dut(&work, "  ").is_err());
        let _ = std::fs::remove_dir_all(&work);
    }

    #[test]
    fn script_restrictions_refuse_destructive_commands() {
        for script in [
            "Format-Volume -DriveLetter C",
            "diskpart /s part.txt",
            "bcdedit /set {default} safeboot minimal",
            "shutdown /r /t 0",
            "Stop-Computer -Force",
            "Set-MpPreference -DisableRealtimeMonitoring $true",
            "New-LocalUser -Name x",
            "Remove-Item -Recurse -Force C:\\Windows\\System32",
        ] {
            assert!(
                script_denial_reason(script).is_some(),
                "must be refused: {script}"
            );
        }
        for script in [
            "Get-Date",
            "Get-ChildItem | Select-Object -First 3",
            "Write-Output 'hello'",
            "Remove-Item -Recurse .\\build",
        ] {
            assert!(
                script_denial_reason(script).is_none(),
                "must be allowed: {script}"
            );
        }
    }

    #[test]
    fn encoded_command_carries_preamble_then_script_verbatim() {
        let scratch = ScratchLayout::new(Path::new(r"D:\work\cell"));
        let script = "Get-ChildItem Env: | ForEach-Object Name\n\u{4F60}\u{597D} 'q' \\ \"dq\" $x";
        let request = powershell_request(script, Path::new(r"D:\work\cell"), &scratch).unwrap();
        let decoded = decoded(&request.args);
        assert!(decoded.starts_with(preamble()));
        assert!(decoded.ends_with(script));
        let flags: Vec<String> = request
            .args
            .iter()
            .map(|arg| arg.to_string_lossy().to_string())
            .collect();
        assert_eq!(
            &flags[..6],
            [
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "RemoteSigned",
                "-EncodedCommand",
            ]
        );
        assert!(request.stdin.is_empty());
        assert_eq!(request.cwd, Path::new(r"D:\work\cell"));
    }

    fn decoded(args: &[OsString]) -> String {
        use base64::Engine as _;
        let encoded = args.last().unwrap().to_string_lossy();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded.as_ref())
            .unwrap();
        let units: Vec<u16> = bytes
            .chunks(2)
            .filter_map(|pair| <[u8; 2]>::try_from(pair).ok())
            .map(u16::from_le_bytes)
            .collect();
        String::from_utf16(&units).unwrap()
    }

    #[test]
    fn oversized_or_nul_script_is_rejected() {
        let scratch = ScratchLayout::new(Path::new(r"D:\work\cell"));
        let cwd = Path::new(r"D:\work\cell");
        assert!(powershell_request("x", cwd, &scratch).is_ok());
        assert!(powershell_request(&"x".repeat(MAX_SCRIPT_CHARS + 1), cwd, &scratch).is_err());
        assert!(powershell_request("a\0b", cwd, &scratch).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn powershell_request_targets_the_system_interpreter_by_absolute_path() {
        let cell = temp_dir("ps");
        let scratch = ScratchLayout::new(&cell);
        let request = powershell_request("Get-Date", &cell, &scratch).unwrap();
        assert!(request.program.is_absolute());
        assert!(
            request
                .program
                .ends_with(r"System32\WindowsPowerShell\v1.0\powershell.exe"),
            "{}",
            request.program.display()
        );
        assert!(request.program.is_file(), "{}", request.program.display());
        assert_eq!(request.cwd, cell);
        let _ = std::fs::remove_dir_all(&cell);
    }

    #[cfg(windows)]
    #[test]
    fn powershell_environment_is_allowlisted() {
        let home = Path::new(r"D:\work\cell\home");
        let temp = Path::new(r"D:\work\cell\tmp");
        let environment = powershell_environment(home, temp);
        let value = |name: &str| {
            environment
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
        };
        assert_eq!(value("TEMP"), Some(temp.as_os_str().to_owned()));
        assert_eq!(value("USERPROFILE"), Some(home.as_os_str().to_owned()));
        assert_eq!(value("HOMEDRIVE"), Some("D:\\".into()));
        assert_eq!(value("HOMEPATH"), Some(r"\work\cell\home".into()));
        assert!(
            value("PATH")
                .unwrap()
                .to_string_lossy()
                .contains("System32")
        );
        for name in [
            "USERDOMAIN",
            "GOOSE_PROVIDER",
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "DUTHUB_TOKEN",
            "SSH_AUTH_SOCK",
        ] {
            assert!(value(name).is_none(), "{name} 不得出现在沙箱环境中");
        }
    }

    #[cfg(windows)]
    #[test]
    fn drive_helpers_handle_prefixes() {
        assert_eq!(drive_of(Path::new(r"C:\Windows")), "C:");
        assert_eq!(drive_of(Path::new(r"\\srv\share\a")), "C:");
        assert_eq!(
            split_drive(Path::new(r"D:\work\cell\home")),
            Some(("D:\\".to_owned(), PathBuf::from(r"\work\cell\home")))
        );
        assert_eq!(split_drive(Path::new(r"relative\path")), None);
    }
}
