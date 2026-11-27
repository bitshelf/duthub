//! Pure policy and command-authoring layer for MXC sandboxed execution.
//!
//! Contains no OS FFI or `unsafe` code: validates scopes, path invariants,
//! timeout/output bounds, network/UI rules, and constructs PowerShell or
//! arbitrary executable requests for [`crate::runner`].

use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

/// Longest wall-clock duration a single one-shot command may request.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(600);
/// Largest per-stream output capture limit a policy may request (16 MiB).
pub const MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
/// Default wall-clock timeout for one-shot commands when `timeout` is zero.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
/// Default per-stream output capture cap when `max_output_bytes` is zero.
pub const DEFAULT_OUTPUT_BYTES: usize = 64 * 1024;

const SCOPE_MAX_CHARS: usize = 64;
const DUT_ID_MAX_CHARS: usize = 256;

/// PowerShell scripts travel base64-encoded on the command line, so length is
/// capped well below the Windows command-line limit.
pub const MAX_SCRIPT_CHARS: usize = 8_000;

/// Declarative network posture mapped directly to `mxc_sdk::v1::NetworkPolicy`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum NetworkPolicy {
    /// Complete network denial (egress deny, ingress deny, host loopback deny).
    #[default]
    Deny,
    /// Loopback only: outbound and bidirectional host loopback (`127.0.0.1` /
    /// `::1`) allowed, all non-loopback egress and ingress denied.
    LoopbackOnly,
    /// Host loopback plus outbound egress (required for Agents that talk to
    /// both local MCP listeners and remote LLM APIs). Inbound non-loopback
    /// connections remain denied. When `proxy_url` is set, direct egress is
    /// denied and traffic is routed through the configured local proxy.
    LoopbackAndOutbound { proxy_url: Option<String> },
}

/// UI, clipboard, and input-injection isolation policy mapped to MXC `UiPolicy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "1:1 mirror of mxc-sdk UiPolicy capability flags"
)]
pub struct UiPolicy {
    /// When `true`, blocks Win32k system calls (`UiPolicy.disable = true`).
    /// Keep `false` for Windows runtimes (such as PowerShell / `WinHttp` /
    /// GUI-linked tools) that fail DLL init when Win32k is disabled; clipboard,
    /// input injection, desktop, and system-settings isolation remain enforced.
    pub block_win32k: bool,
    pub allow_clipboard_read: bool,
    pub allow_clipboard_write: bool,
    pub allow_input_injection: bool,
}

impl UiPolicy {
    /// Strictest UI isolation including blocking Win32k syscalls.
    #[must_use]
    pub const fn strict() -> Self {
        Self {
            block_win32k: true,
            allow_clipboard_read: false,
            allow_clipboard_write: false,
            allow_input_injection: false,
        }
    }
}

/// Optional Windows PSEC Learning Mode denial-capture configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DenialCaptureConfig {
    /// Optional target path for the JSON denials document (parent must exist).
    /// When `None`, MXC manages a per-run temporary file.
    pub output_path: Option<PathBuf>,
    /// Keep the sealed ETL trace alongside the JSON denials report.
    pub retain_etl: bool,
}

/// Declarative sandbox bounds enforced by `mxc-sdk`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxPolicy {
    /// Directory trees the process may read and execute.
    pub readable: Vec<PathBuf>,
    /// Directory trees the process may read and write. May be empty for
    /// strictly read-only containers (in which case `cwd` must be inside
    /// `readable`).
    pub writable: Vec<PathBuf>,
    /// Directory or file trees explicitly denied for both reads and writes
    /// (enforced natively by Windows PSEC `deniedPaths`, Bubblewrap, Seatbelt).
    pub deny_read: Vec<PathBuf>,
    /// Directory trees that may only be enumerated without file-content reads
    /// (Windows PSEC 1.1 `enumeratePaths`).
    pub enumerate: Vec<PathBuf>,
    pub network: NetworkPolicy,
    pub ui: UiPolicy,
    /// Wall-clock timeout for one-shot commands. Zero means [`DEFAULT_TIMEOUT`];
    /// values above [`MAX_TIMEOUT`] are rejected. Ignored by streaming spawns.
    pub timeout: Duration,
    /// Per-stream stdout/stderr capture limit. Zero means [`DEFAULT_OUTPUT_BYTES`].
    pub max_output_bytes: usize,
    /// Optional native denial-capture recording (Windows PSEC / macOS Seatbelt).
    pub capture_denials: Option<DenialCaptureConfig>,
}

impl SandboxPolicy {
    /// Read and write access to a single directory tree, no network, UI isolated.
    pub fn scratch(root: impl Into<PathBuf>) -> Self {
        Self {
            readable: Vec::new(),
            writable: vec![root.into()],
            deny_read: Vec::new(),
            enumerate: Vec::new(),
            network: NetworkPolicy::Deny,
            ui: UiPolicy::default(),
            timeout: Duration::ZERO,
            max_output_bytes: 0,
            capture_denials: None,
        }
    }

    /// Policy for an Agent host process (`goose`, CLI profiles): cell writable,
    /// host secrets and sibling DUTs denied, loopback + outbound network allowed.
    #[must_use]
    pub fn agent_cell(cell: &CellSpec, allowed_exe_dir: Option<&Path>) -> Self {
        Self {
            readable: Vec::new(),
            writable: vec![cell.root().to_path_buf()],
            deny_read: cell.protected_paths_excluding(allowed_exe_dir),
            enumerate: Vec::new(),
            network: NetworkPolicy::LoopbackAndOutbound { proxy_url: None },
            ui: UiPolicy::default(),
            timeout: Duration::ZERO,
            max_output_bytes: 0,
            capture_denials: None,
        }
    }

    /// Policy for untrusted script execution: cell writable, protected paths
    /// denied, zero network (loopback included), UI isolated.
    #[must_use]
    pub fn script_cell(cell: &CellSpec) -> Self {
        Self {
            readable: Vec::new(),
            writable: vec![cell.root().to_path_buf()],
            deny_read: cell.protected_paths(),
            enumerate: Vec::new(),
            network: NetworkPolicy::Deny,
            ui: UiPolicy::default(),
            timeout: Duration::ZERO,
            max_output_bytes: 0,
            capture_denials: None,
        }
    }

    #[must_use]
    pub fn timeout_or_default(&self) -> Duration {
        if self.timeout.is_zero() {
            DEFAULT_TIMEOUT
        } else {
            self.timeout
        }
    }

    #[must_use]
    pub fn max_output_or_default(&self) -> usize {
        if self.max_output_bytes == 0 {
            DEFAULT_OUTPUT_BYTES
        } else {
            self.max_output_bytes
        }
    }
}

/// One short-lived command to run to completion inside an MXC container.
#[derive(Debug, Clone)]
pub struct ExecRequest {
    /// Absolute path of the executable (never searched on `PATH`).
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// Working directory inside one of the policy's writable or readable roots.
    pub cwd: PathBuf,
    /// Explicit environment of the child (nothing is inherited from host).
    pub environment: Vec<(OsString, OsString)>,
    pub stdin: Vec<u8>,
}

/// One long-lived command (such as an ACP or streaming CLI session).
#[derive(Debug, Clone)]
pub struct SpawnRequest {
    /// Absolute path of the executable (never searched on `PATH`).
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// Working directory inside one of the policy's writable or readable roots.
    pub cwd: PathBuf,
    /// Explicit environment of the child (nothing is inherited from host).
    pub environment: Vec<(OsString, OsString)>,
}

/// Outcome of one sandboxed one-shot command.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecResult {
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// The policy timeout elapsed and the process tree was terminated.
    pub timed_out: bool,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    /// Policy or operational warnings reported by `mxc-sdk`.
    pub warnings: Vec<String>,
    /// Denial-capture deliverable metadata when `capture_denials` was enabled.
    pub denial_capture: Option<crate::runner::DenialCaptureSummary>,
}

/// Fail-closed sandbox error variants (English technical messages per crate policy).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SandboxError {
    /// No supported MXC containment backend is available on this host.
    #[error("sandbox backend unavailable: {0}")]
    Unsupported(String),
    /// The scope, paths, request, or policy violated validation bounds.
    #[error("sandbox policy rejected: {0}")]
    PolicyRejected(String),
    /// Container directory or environment setup failed before launch.
    #[error("sandbox setup failed: {0}")]
    Setup(String),
    /// The MXC engine failed to start the containerized process.
    #[error("sandbox spawn failed: {0}")]
    Spawn(String),
    /// Waiting for or collecting output from the running container failed.
    #[error("sandbox execution failed: {0}")]
    Execution(String),
}

impl From<std::io::Error> for SandboxError {
    fn from(error: std::io::Error) -> Self {
        SandboxError::Setup(error.to_string())
    }
}

/// FNV-1a 64-bit digest for stable, bounded scope identifiers.
fn fnv1a(text: &str) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// Validate a logical container scope identifier (`1..=64` ASCII `[A-Za-z0-9._-]`).
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
            "invalid sandbox scope (expected 1-{SCOPE_MAX_CHARS} ASCII alnum/-/_/.): {scope:?}"
        )))
    }
}

/// Strip Windows `\\?\` verbatim prefixes so path comparisons and MXC policy
/// entries use consistent plain paths.
pub(crate) fn strip_verbatim(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{unc}"));
        }
        if let Some(plain) = text.strip_prefix(r"\\?\") {
            return PathBuf::from(plain);
        }
    }
    path.to_path_buf()
}

/// Case-insensitive component-wise containment on Windows, exact on Unix.
pub(crate) fn path_within(candidate: &Path, root: &Path) -> bool {
    let candidate = strip_verbatim(candidate);
    let root = strip_verbatim(root);
    let mut cand = candidate.components();
    for expected in root.components() {
        let Some(actual) = cand.next() else {
            return false;
        };
        #[cfg(windows)]
        let matched = actual
            .as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(&expected.as_os_str().to_string_lossy());
        #[cfg(not(windows))]
        let matched = actual == expected;
        if !matched {
            return false;
        }
    }
    true
}

/// Absolute, lexically normal (no `..`), and existing directory.
fn is_plain_dir(path: &Path) -> bool {
    path.is_absolute()
        && !path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        && path.is_dir()
}

/// Absolute, lexically normal (no `..`), and existing file or directory.
fn is_plain_existing(path: &Path) -> bool {
    path.is_absolute()
        && !path
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        && path.exists()
}

/// Validate a [`SandboxPolicy`] and working directory before touching MXC.
pub fn validate_policy(policy: &SandboxPolicy, cwd: &Path) -> Result<(), SandboxError> {
    if policy.timeout_or_default() > MAX_TIMEOUT {
        return Err(SandboxError::PolicyRejected(format!(
            "timeout exceeds maximum of {} seconds",
            MAX_TIMEOUT.as_secs()
        )));
    }
    if policy.max_output_or_default() > MAX_OUTPUT_BYTES {
        return Err(SandboxError::PolicyRejected(format!(
            "max_output_bytes exceeds maximum of {MAX_OUTPUT_BYTES} bytes"
        )));
    }
    if policy.writable.is_empty() && policy.readable.is_empty() {
        return Err(SandboxError::PolicyRejected(
            "policy must grant at least one writable or readable directory".to_owned(),
        ));
    }
    for root in policy
        .readable
        .iter()
        .chain(&policy.writable)
        .chain(&policy.enumerate)
    {
        if !is_plain_dir(root) {
            return Err(SandboxError::PolicyRejected(format!(
                "sandbox grant must be an existing absolute directory without `..`: {}",
                root.display()
            )));
        }
    }
    for denied in &policy.deny_read {
        if !is_plain_existing(denied) {
            return Err(SandboxError::PolicyRejected(format!(
                "denied path must be an existing absolute path without `..`: {}",
                denied.display()
            )));
        }
    }
    if !is_plain_dir(cwd) {
        return Err(SandboxError::PolicyRejected(format!(
            "working directory must be an existing absolute directory without `..`: {}",
            cwd.display()
        )));
    }
    let cwd_granted = policy
        .writable
        .iter()
        .chain(&policy.readable)
        .any(|root| path_within(cwd, root));
    if !cwd_granted {
        return Err(SandboxError::PolicyRejected(format!(
            "working directory must lie inside a writable or readable root: {}",
            cwd.display()
        )));
    }
    // A denied path must never be an ancestor of (or identical to) a granted
    // root, because PSEC deny rules override allow rules and would make the
    // container unusable or contradictory.
    for denied in &policy.deny_read {
        for allowed in policy.writable.iter().chain(&policy.readable) {
            if path_within(allowed, denied) {
                return Err(SandboxError::PolicyRejected(format!(
                    "denied path {} conflicts with granted path {}",
                    denied.display(),
                    allowed.display()
                )));
            }
        }
    }
    if let NetworkPolicy::LoopbackAndOutbound {
        proxy_url: Some(proxy),
    } = &policy.network
        && proxy.trim().is_empty()
    {
        return Err(SandboxError::PolicyRejected(
            "network proxy URL must not be blank".to_owned(),
        ));
    }
    if let Some(capture) = &policy.capture_denials
        && let Some(output) = &capture.output_path
    {
        let valid_parent = output.is_absolute() && output.parent().is_some_and(Path::is_dir);
        if !valid_parent {
            return Err(SandboxError::PolicyRejected(format!(
                "capture_denials output_path must be absolute with an existing parent directory: {}",
                output.display()
            )));
        }
    }
    Ok(())
}

/// Private `tmp` and `home` directories inside a cell's writable root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScratchLayout {
    root: PathBuf,
}

impl ScratchLayout {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn temp(&self) -> PathBuf {
        self.root.join("tmp")
    }

    #[must_use]
    pub fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    /// Directories that must exist before launching a command.
    #[must_use]
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

/// One per-DUT isolation cell rooted at `<work_dir>/agent/dut/<hex(dut_id)>/sandbox`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellSpec {
    scope: String,
    work_dir: PathBuf,
    dut_key: String,
    root: PathBuf,
}

impl CellSpec {
    /// Construct a cell from a canonical `<work_dir>/agent/dut/<key>/sandbox` path.
    pub fn new(root: impl Into<PathBuf>, scope: &str) -> Result<Self, SandboxError> {
        validate_scope(scope)?;
        let root = strip_verbatim(&root.into());
        if !root.is_absolute() {
            return Err(SandboxError::PolicyRejected(format!(
                "sandbox cell root must be absolute: {}",
                root.display()
            )));
        }
        let work_dir = work_dir_of(&root).ok_or_else(|| {
            SandboxError::PolicyRejected(format!(
                "cannot derive work_dir from cell root (expected <work_dir>/agent/dut/<key>/sandbox): {}",
                root.display()
            ))
        })?;
        let dut_key = root
            .parent()
            .and_then(Path::file_name)
            .map_or_default(|name| name.to_string_lossy().into_owned());
        Ok(Self {
            scope: scope.to_owned(),
            work_dir,
            dut_key,
            root,
        })
    }

    /// Construct a cell rooted at an arbitrary absolute directory (tests/embedded use).
    pub fn at(root: impl Into<PathBuf>, scope: &str) -> Result<Self, SandboxError> {
        validate_scope(scope)?;
        let root = strip_verbatim(&root.into());
        if !root.is_absolute() {
            return Err(SandboxError::PolicyRejected(format!(
                "sandbox cell root must be absolute: {}",
                root.display()
            )));
        }
        let dut_key = root
            .file_name()
            .map_or_default(|name| name.to_string_lossy().into_owned());
        let work_dir = root
            .parent()
            .map_or_else(|| root.clone(), Path::to_path_buf);
        Ok(Self {
            scope: scope.to_owned(),
            work_dir,
            dut_key,
            root,
        })
    }

    /// Canonical per-DUT cell under `work_dir`.
    pub fn for_dut(work_dir: &Path, dut_id: &str) -> Result<Self, SandboxError> {
        if dut_id.trim().is_empty() || dut_id.chars().count() > DUT_ID_MAX_CHARS {
            return Err(SandboxError::PolicyRejected(format!(
                "DUT id is empty or exceeds {DUT_ID_MAX_CHARS} characters"
            )));
        }
        let work_dir = strip_verbatim(work_dir);
        if !work_dir.is_absolute() {
            return Err(SandboxError::PolicyRejected(format!(
                "work_dir must be absolute: {}",
                work_dir.display()
            )));
        }
        let dut_key =
            dut_id
                .bytes()
                .fold(String::with_capacity(dut_id.len() * 2), |mut out, byte| {
                    use std::fmt::Write as _;
                    let _ = write!(out, "{byte:02x}");
                    out
                });
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
            work_dir,
            dut_key,
            root,
        })
    }

    #[must_use]
    pub fn scope(&self) -> &str {
        &self.scope
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn work_dir(&self) -> &Path {
        &self.work_dir
    }

    #[must_use]
    pub fn dut_key(&self) -> &str {
        &self.dut_key
    }

    #[must_use]
    pub fn layout(&self) -> ScratchLayout {
        ScratchLayout::new(&self.root)
    }

    #[must_use]
    pub fn required_dirs(&self) -> Vec<PathBuf> {
        self.layout().required_dirs()
    }

    /// Host secrets, configuration, evidence, and sibling DUT directories that
    /// this cell must never read.
    #[must_use]
    pub fn protected_paths(&self) -> Vec<PathBuf> {
        self.protected_paths_excluding(None)
    }

    /// Like [`Self::protected_paths`], but when `allowed_exe_dir` lies inside
    /// `<exe dir>/.duthub/tools/<tool>`, excludes that tool directory from
    /// denial while still denying all sibling config entries in `.duthub/`
    /// (such as `settings.json` and `duts.json`).
    pub fn protected_paths_excluding(&self, allowed_exe_dir: Option<&Path>) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        let allowed_exe_dir = allowed_exe_dir.map(strip_verbatim);

        if let Some(duthub_dir) = host_config_dir() {
            let duthub_dir = strip_verbatim(&duthub_dir);
            if let Some(exe_dir) = allowed_exe_dir.as_deref()
                && path_within(exe_dir, &duthub_dir)
            {
                // Deny every direct entry inside `.duthub/` that does not contain
                // `exe_dir`, so `settings.json` / `duts.json` remain blocked even
                // while `.duthub/tools/<tool>` is granted read-only for launch.
                if let Ok(entries) = std::fs::read_dir(&duthub_dir) {
                    for entry in entries.flatten() {
                        let path = strip_verbatim(&entry.path());
                        if !path_within(exe_dir, &path) {
                            paths.push(path);
                        }
                    }
                }
            } else {
                paths.push(duthub_dir);
            }
        }

        let data_dir = self.work_dir.join("duthub-data");
        if data_dir.is_dir() && !is_excluded_ancestor(&data_dir, allowed_exe_dir.as_deref()) {
            paths.push(data_dir);
        }
        let agents_file = self.work_dir.join("agents.json");
        if agents_file.is_file() {
            paths.push(agents_file);
        }
        if let Some(user_profile) =
            std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))
        {
            let profile = PathBuf::from(user_profile);
            for rel in [".ssh", ".aws", ".kube", ".docker", ".gitconfig", ".gnupg"] {
                let candidate = profile.join(rel);
                if candidate.exists()
                    && !is_excluded_ancestor(&candidate, allowed_exe_dir.as_deref())
                {
                    paths.push(candidate);
                }
            }
        }
        let dut_parent = self.work_dir.join("agent").join("dut");
        let own_dut_dir = self.root.parent().unwrap_or(&self.root);
        if let Ok(entries) = std::fs::read_dir(&dut_parent) {
            for entry in entries.flatten() {
                let path = strip_verbatim(&entry.path());
                if path == own_dut_dir {
                    continue;
                }
                if path.is_dir() && !is_excluded_ancestor(&path, allowed_exe_dir.as_deref()) {
                    paths.push(path);
                }
            }
        }
        paths
    }
}

fn is_excluded_ancestor(candidate: &Path, allowed: Option<&Path>) -> bool {
    allowed.is_some_and(|allowed| path_within(allowed, candidate))
}

fn host_config_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?.join(".duthub");
    dir.is_dir().then_some(dir)
}

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

/// Format `(program, args)` into a single command-line string for MXC.
///
/// On Windows, arguments are quoted per Microsoft C runtime / `CommandLineToArgvW`
/// rules. On Unix, arguments are single-quote escaped for POSIX `sh`.
pub fn format_command_line(program: &Path, args: &[OsString]) -> Result<String, SandboxError> {
    let program_str = program.to_str().ok_or_else(|| {
        SandboxError::PolicyRejected("executable path is not valid UTF-8".to_owned())
    })?;
    if program_str.is_empty() || program_str.contains('\0') {
        return Err(SandboxError::PolicyRejected(
            "executable path is empty or contains NUL".to_owned(),
        ));
    }
    let mut parts = Vec::with_capacity(1 + args.len());
    parts.push(quote_arg(program_str));
    for arg in args {
        let text = arg.to_str().ok_or_else(|| {
            SandboxError::PolicyRejected("command argument is not valid UTF-8".to_owned())
        })?;
        if text.contains('\0') {
            return Err(SandboxError::PolicyRejected(
                "command argument contains NUL".to_owned(),
            ));
        }
        parts.push(quote_arg(text));
    }
    Ok(parts.join(" "))
}

#[cfg(windows)]
fn quote_arg(arg: &str) -> String {
    if !arg.is_empty()
        && !arg
            .chars()
            .any(|c| matches!(c, ' ' | '\t' | '\n' | '\x0b' | '"'))
    {
        return arg.to_owned();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for ch in arg.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                for _ in 0..=(backslashes * 2) {
                    out.push('\\');
                }
                out.push('"');
                backslashes = 0;
            }
            _ => {
                for _ in 0..backslashes {
                    out.push('\\');
                }
                backslashes = 0;
                out.push(ch);
            }
        }
    }
    for _ in 0..(backslashes * 2) {
        out.push('\\');
    }
    out.push('"');
    out
}

#[cfg(not(windows))]
fn quote_arg(arg: &str) -> String {
    if !arg.is_empty()
        && arg.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'-' | b'_' | b':' | b'=')
        })
    {
        return arg.to_owned();
    }
    format!("'{}'", arg.replace('\'', "'\\''"))
}

/// Fixed absolute path to Windows PowerShell 5.1 (never resolved via `PATH`).
pub fn windows_powershell() -> PathBuf {
    std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .filter(|root| root.is_absolute())
        .map_or_else(
            || PathBuf::from(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"),
            |root| {
                root.join("System32")
                    .join("WindowsPowerShell")
                    .join("v1.0")
                    .join("powershell.exe")
            },
        )
}

/// UTF-16LE base64 payload for PowerShell `-EncodedCommand`.
pub fn encode_script(script: &str) -> String {
    use base64::Engine as _;
    let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn preamble() -> &'static str {
    concat!(
        "$OutputEncoding = [System.Text.UTF8Encoding]::new($false)\n",
        "try { [Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false) } catch {}\n",
        "$env:POWERSHELL_TELEMETRY_OPTOUT = '1'\n",
        "$env:DOTNET_CLI_TELEMETRY_OPTOUT = '1'\n",
        "try { Set-Location -LiteralPath $env:DUTHUB_SANDBOX_CWD -ErrorAction Stop } catch {\n",
        "    New-PSDrive -Name DuthubCell -PSProvider FileSystem -Root $env:DUTHUB_SANDBOX_CWD -ErrorAction Stop | Out-Null\n",
        "    Set-Location -LiteralPath 'DuthubCell:\\' -ErrorAction Stop\n",
        "}\n",
    )
}

/// Build an allow-listed Windows PowerShell execution request for `script`.
pub fn powershell_request(
    script: &str,
    cwd: &Path,
    scratch: &ScratchLayout,
) -> Result<ExecRequest, SandboxError> {
    if let Some((pattern, reason)) = script_denial_reason(script) {
        return Err(SandboxError::PolicyRejected(format!(
            "script contains prohibited destructive operation ({reason}, matched `{pattern}`)"
        )));
    }
    if script.chars().count() > MAX_SCRIPT_CHARS {
        return Err(SandboxError::PolicyRejected(format!(
            "PowerShell script exceeds limit of {MAX_SCRIPT_CHARS} characters"
        )));
    }
    if script.contains('\0') {
        return Err(SandboxError::PolicyRejected(
            "PowerShell script contains NUL byte".to_owned(),
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

/// Minimal allow-listed environment for Windows sandboxed commands.
///
/// Always includes `SystemRoot` and `LOCALAPPDATA` (required by Windows PSEC)
/// with user/temp paths anchored inside the sandbox cell.
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

struct ScriptRestriction {
    pattern: &'static str,
    reason: &'static str,
}

const SCRIPT_RESTRICTIONS: &[ScriptRestriction] = &[
    ScriptRestriction {
        pattern: "format-volume",
        reason: "volume format",
    },
    ScriptRestriction {
        pattern: "clear-disk",
        reason: "disk wipe",
    },
    ScriptRestriction {
        pattern: "initialize-disk",
        reason: "disk initialization",
    },
    ScriptRestriction {
        pattern: "diskpart",
        reason: "disk partitioning tool",
    },
    ScriptRestriction {
        pattern: "bcdedit",
        reason: "boot configuration edit",
    },
    ScriptRestriction {
        pattern: "shutdown",
        reason: "system shutdown or reboot",
    },
    ScriptRestriction {
        pattern: "stop-computer",
        reason: "system shutdown or reboot",
    },
    ScriptRestriction {
        pattern: "restart-computer",
        reason: "system shutdown or reboot",
    },
    ScriptRestriction {
        pattern: "set-mppreference",
        reason: "antivirus policy modification",
    },
    ScriptRestriction {
        pattern: "add-mppreference",
        reason: "antivirus policy modification",
    },
    ScriptRestriction {
        pattern: "remove-mppreference",
        reason: "antivirus policy modification",
    },
    ScriptRestriction {
        pattern: "new-localuser",
        reason: "local account creation",
    },
    ScriptRestriction {
        pattern: "add-localgroupmember",
        reason: "local group modification",
    },
    ScriptRestriction {
        pattern: "set-executionpolicy",
        reason: "execution policy modification",
    },
    ScriptRestriction {
        pattern: "takeown",
        reason: "file ownership takeover",
    },
    ScriptRestriction {
        pattern: "enable-windowsoptionalfeature",
        reason: "OS optional feature modification",
    },
    ScriptRestriction {
        pattern: "disable-windowsoptionalfeature",
        reason: "OS optional feature modification",
    },
];

const SYSTEM_PATHS_FOR_RECURSIVE_DELETE: &[&str] = &[
    "c:\\windows",
    "c:\\program files",
    "c:\\program files (x86)",
    "$env:systemroot",
    "$env:windir",
    "$env:programfiles",
    "c:\\users\\public",
];

/// Defense-in-depth denylist for destructive host commands checked before spawn.
#[must_use]
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
        return Some((
            "remove-item -recurse <system path>",
            "recursive delete of system directory",
        ));
    }
    None
}

/// Validate an environment variable name before forwarding to MXC.
pub(crate) fn valid_environment_key(key: &OsStr) -> bool {
    let Some(text) = key.to_str() else {
        return false;
    };
    !text.is_empty() && !text.contains(['=', '\0'])
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
    fn scope_validation_enforces_safe_charset() {
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
    }

    #[test]
    fn policy_bounds_and_readonly_mode_are_validated() {
        let root = temp_dir("policy");
        let cell_dir = root.join("cell");
        std::fs::create_dir_all(cell_dir.join("sub")).unwrap();
        let ok = SandboxPolicy::scratch(&cell_dir);
        assert!(validate_policy(&ok, &cell_dir).is_ok());
        assert!(validate_policy(&ok, &cell_dir.join("sub")).is_ok());
        assert!(validate_policy(&ok, &root).is_err());

        // Read-only policy (no writable roots) is valid when cwd is in readable.
        let mut ro = ok.clone();
        ro.writable.clear();
        assert!(validate_policy(&ro, &cell_dir).is_err());
        ro.readable.push(cell_dir.clone());
        assert!(validate_policy(&ro, &cell_dir).is_ok());

        let mut bad = ok.clone();
        bad.timeout = MAX_TIMEOUT + Duration::from_secs(1);
        assert!(validate_policy(&bad, &cell_dir).is_err());
        bad.timeout = Duration::ZERO;
        bad.max_output_bytes = MAX_OUTPUT_BYTES + 1;
        assert!(validate_policy(&bad, &cell_dir).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn deny_read_cannot_contradict_granted_roots() {
        let root = temp_dir("overlap");
        let cell_dir = root.join("cell");
        std::fs::create_dir_all(&cell_dir).unwrap();
        let mut policy = SandboxPolicy::scratch(&cell_dir);
        policy.deny_read = vec![cell_dir.clone()];
        assert!(matches!(
            validate_policy(&policy, &cell_dir),
            Err(SandboxError::PolicyRejected(_))
        ));
        policy.deny_read = vec![root.clone()];
        assert!(matches!(
            validate_policy(&policy, &cell_dir),
            Err(SandboxError::PolicyRejected(_))
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cell_layout_matches_agent_host_convention() {
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
        let rebuilt = CellSpec::new(cell.root(), cell.scope()).unwrap();
        assert_eq!(rebuilt.work_dir(), work.as_path());
        assert!(CellSpec::for_dut(&work, "  ").is_err());
        let _ = std::fs::remove_dir_all(&work);
    }

    #[test]
    fn format_command_line_quotes_spaces_and_embedded_quotes() {
        let cmd = format_command_line(
            Path::new(r"C:\Program Files\My App\tool.exe"),
            &[
                OsString::from("--flag"),
                OsString::from("hello world"),
                OsString::from(r#"a"b\c\"#),
                OsString::from(""),
            ],
        )
        .unwrap();
        #[cfg(windows)]
        assert_eq!(
            cmd,
            r#""C:\Program Files\My App\tool.exe" --flag "hello world" "a\"b\c\\" """#
        );
        assert!(format_command_line(Path::new("tool.exe"), &[OsString::from("bad\0arg")]).is_err());
    }

    #[test]
    fn script_restrictions_refuse_destructive_commands() {
        for script in [
            "Format-Volume -DriveLetter C",
            "diskpart /s part.txt",
            "bcdedit /set {default} safeboot minimal",
            "shutdown /r /t 0",
            "Remove-Item -Recurse -Force C:\\Windows\\System32",
        ] {
            assert!(script_denial_reason(script).is_some(), "{script}");
        }
        assert!(script_denial_reason("Get-Date").is_none());
        assert!(script_denial_reason("Remove-Item -Recurse .\\build").is_none());
    }

    #[cfg(windows)]
    #[test]
    fn powershell_environment_contains_required_psec_vars_and_no_host_secrets() {
        let home = Path::new(r"D:\work\cell\home");
        let temp = Path::new(r"D:\work\cell\tmp");
        let env = powershell_environment(home, temp);
        let value = |name: &str| {
            env.iter()
                .find(|(k, _)| k.to_string_lossy().eq_ignore_ascii_case(name))
                .map(|(_, v)| v.clone())
        };
        assert!(value("SYSTEMROOT").is_some());
        assert_eq!(
            value("LOCALAPPDATA"),
            Some(home.join("AppData").join("Local").into_os_string())
        );
        assert_eq!(value("TEMP"), Some(temp.as_os_str().to_owned()));
        assert_eq!(value("USERPROFILE"), Some(home.as_os_str().to_owned()));
        for secret in [
            "GOOSE_PROVIDER",
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "DUTHUB_TOKEN",
        ] {
            assert!(value(secret).is_none(), "{secret} must not leak");
        }
    }
}
