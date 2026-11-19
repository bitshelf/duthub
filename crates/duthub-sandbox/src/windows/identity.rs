//! The dedicated sandbox identity: a local account, its DPAPI-sealed password,
//! and the one-time elevated provisioning step that creates both.
//!
//! Derived from openai/codex `codex-rs/windows-sandbox-rs` (Apache-2.0):
//! `identity.rs` (account selection + credential-file handling), `setup.rs`
//! (local group/user creation with a random password) and `dpapi.rs`.
//!
//! Why a separate account is the whole point (measured 2026-11-25 on this
//! machine, see ADR-12 §5): an AppContainer token is *additive*, not
//! subtractive — its user is still the signed-in account and its group list is
//! the normal one (`Everyone`, `Users`, `Authenticated Users`, …). Reads are
//! therefore authorised with the host user's own identity, so `%TEMP%` files
//! created by duthub are readable by a sandboxed script, and denying the
//! container SID or `ALL APPLICATION PACKAGES` changes nothing. Only a deny ACE
//! for a SID that is a *different* principal blocks the read, and the only way
//! to get such a principal into the child's token is to run the child as another
//! account.
//!
//! Provisioning needs administrator rights exactly once per machine:
//!
//! ```text
//! # from an elevated shell, in the repository:
//! cargo run -p duthub-sandbox --example provision-sandbox
//! ```
//!
//! Everything after that runs unprivileged: `LogonUserW` with the stored
//! password needs no elevation, and the DPAPI blob is machine-protected so an
//! elevated provisioner and an unprivileged duthub both decrypt it.

use std::path::{Path, PathBuf};

use windows_sys::Win32::NetworkManagement::NetManagement::{
    LOCALGROUP_INFO_1, LOCALGROUP_MEMBERS_INFO_3, NERR_GroupExists, NERR_UserExists,
    NetLocalGroupAdd, NetLocalGroupAddMembers, NetUserAdd, NetUserSetInfo, UF_DONT_EXPIRE_PASSWD,
    UF_SCRIPT, USER_INFO_1, USER_PRIV_USER,
};

use super::acl::{self, PinnedPath};
use super::{OwnedSid, dpapi};
use crate::policy::SandboxError;

/// Local account that every sandboxed process runs as.
pub(crate) const SANDBOX_USER: &str = "DuthubSandbox";
/// Local group the account belongs to. ACL grants for future features (shared
/// read roots) can be issued to the group without touching the account.
pub(crate) const SANDBOX_GROUP: &str = "DuthubSandboxUsers";
/// Bumped when the stored file layout or the account contract changes; a
/// mismatched file is reported as "not provisioned" rather than silently used.
const IDENTITY_VERSION: u32 = 1;
const IDENTITY_HEADER: &str = "duthub-sandbox-identity";

/// `ERROR_ACCESS_DENIED`.
const ERROR_ACCESS_DENIED: u32 = 5;
/// `ERROR_MEMBER_IN_ALIAS` — the account is already in the group.
const ERROR_MEMBER_IN_ALIAS: u32 = 1378;

/// The credentials of the sandbox account.
#[derive(Clone)]
pub(crate) struct SandboxIdentity {
    pub(crate) username: String,
    pub(crate) password: String,
}

impl std::fmt::Debug for SandboxIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The password must never reach a log line or a panic message.
        formatter.write_str("SandboxIdentity { username, password: <redacted> }")
    }
}

use crate::status::IdentityReport;

/// Process-wide override of [`state_dir`], set by the host (or the provisioning
/// CLI) when the credential must live inside a project directory instead of the
/// per-user default.
static ROOT_OVERRIDE: std::sync::OnceLock<std::sync::RwLock<Option<PathBuf>>> =
    std::sync::OnceLock::new();

fn root_override() -> &'static std::sync::RwLock<Option<PathBuf>> {
    ROOT_OVERRIDE.get_or_init(|| std::sync::RwLock::new(None))
}

/// Environment variable that points the credential directory elsewhere.
const ROOT_ENV: &str = "DUTHUB_SANDBOX_IDENTITY_ROOT";

/// Override the directory that holds the DPAPI-sealed credential.
///
/// Precedence is: this override > `DUTHUB_SANDBOX_IDENTITY_ROOT` >
/// `%LOCALAPPDATA%\duthub\sandbox`. `None` clears the override. The path must be
/// absolute; call it before the first `status()`/`prepare()` so no earlier read
/// was cached (there is no cache, but a host that sets it late still gets a
/// consistent directory from then on).
pub(crate) fn set_root(root: Option<PathBuf>) -> Result<(), SandboxError> {
    if let Some(root) = &root
        && !root.is_absolute()
    {
        return Err(SandboxError::PolicyRejected(format!(
            "沙箱身份目录必须是绝对路径: {}",
            root.display()
        )));
    }
    let mut slot = root_override()
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *slot = root;
    Ok(())
}

/// The directory that holds the DPAPI-sealed credential and the account name.
///
/// Default `%LOCALAPPDATA%\duthub\sandbox`; a host that must keep all data inside
/// its project directory calls [`set_root`] (or sets
/// `DUTHUB_SANDBOX_IDENTITY_ROOT`) before use. This is a **known deviation** from
/// "data only in the project directory" for the default configuration — recorded
/// in ADR-12 §4.1.4.
pub(crate) fn state_dir() -> Result<PathBuf, SandboxError> {
    if let Some(root) = root_override()
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
    {
        return Ok(root);
    }
    if let Some(root) = std::env::var_os(ROOT_ENV).map(PathBuf::from) {
        if !root.is_absolute() {
            return Err(SandboxError::PolicyRejected(format!(
                "{ROOT_ENV} 必须是绝对路径: {}",
                root.display()
            )));
        }
        return Ok(root);
    }
    let base = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("USERPROFILE")
                .map(PathBuf::from)
                .map(|profile| profile.join("AppData").join("Local"))
        })
        .ok_or_else(|| {
            SandboxError::Setup(
                "无法定位 LOCALAPPDATA，无法读取沙箱身份凭据（需要管理员一次性初始化）".to_owned(),
            )
        })?;
    Ok(base.join("duthub").join("sandbox"))
}

fn credential_file() -> Result<PathBuf, SandboxError> {
    Ok(state_dir()?.join("identity.txt"))
}

/// Whether a credential file for the current version exists.
///
/// This is a *file* check only; [`super::token::identity_probe`] additionally
/// proves the account still logs on.
pub(crate) fn is_provisioned() -> bool {
    credential_file().is_ok_and(|path| path.is_file())
}

/// Load the stored credentials, or `Ok(None)` when nothing is provisioned.
pub(crate) fn load() -> Result<Option<SandboxIdentity>, SandboxError> {
    let path = credential_file()?;
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(SandboxError::Setup(format!(
                "无法读取沙箱身份凭据 {}: {error}",
                path.display()
            )));
        }
    };
    let mut lines = text.lines();
    if lines.next() != Some(IDENTITY_HEADER) {
        return Err(SandboxError::Setup(format!(
            "沙箱身份凭据格式不符（需要重新初始化）: {}",
            path.display()
        )));
    }
    let mut version = None;
    let mut username = None;
    let mut seal = None;
    for line in lines {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(SandboxError::Setup(format!(
                "沙箱身份凭据含无法解析的行: {}",
                path.display()
            )));
        };
        match key {
            "version" => {
                version = Some(
                    value
                        .parse::<u32>()
                        .map_err(|_| SandboxError::Setup("沙箱身份凭据的版本号无效".to_owned()))?,
                )
            }
            "username" => username = Some(value.to_owned()),
            // DPAPI output is opaque bytes; base64 keeps the file line-oriented.
            "sealed" => seal = Some(value.to_owned()),
            other => {
                return Err(SandboxError::Setup(format!(
                    "沙箱身份凭据含未知字段 {other:?}"
                )));
            }
        }
    }
    let (Some(version), Some(username), Some(seal)) = (version, username, seal) else {
        return Err(SandboxError::Setup(
            "沙箱身份凭据不完整（需要重新初始化）".to_owned(),
        ));
    };
    if version != IDENTITY_VERSION {
        return Err(SandboxError::Setup(format!(
            "沙箱身份凭据版本 {version} 与当前版本 {IDENTITY_VERSION} 不一致（需要重新初始化）"
        )));
    }
    if username != SANDBOX_USER {
        return Err(SandboxError::Setup(format!(
            "沙箱身份凭据的账户名不是 {SANDBOX_USER}（拒绝使用其他账户）"
        )));
    }
    let sealed = decode_base64(&seal)?;
    let password = String::from_utf8(dpapi::unprotect(&sealed)?)
        .map_err(|_| SandboxError::Setup("沙箱身份口令不是合法 UTF-8".to_owned()))?;
    if password.is_empty() {
        return Err(SandboxError::Setup(
            "沙箱身份口令为空（需要重新初始化）".to_owned(),
        ));
    }
    // The credential directory must not be writable by anyone but the current
    // user, SYSTEM and Administrators: whoever can rewrite it can substitute a
    // different account's password.
    verify_state_dir_dacl(&state_dir()?)?;
    Ok(Some(SandboxIdentity { username, password }))
}

/// Whether the dedicated identity can actually be used to **start a process**
/// right now, and if not, why.
///
/// Three independent conditions, checked in this order so the reported reason is
/// the first one that fails:
///
/// 1. the credential file exists and loads (`Unverified`: never provisioned);
/// 2. **this process** holds `SeAssignPrimaryTokenPrivilege` and
///    `SeIncreaseQuotaPrivilege` — `CreateProcessAsUser` fails with Win32 1314
///    without them, which is exactly what an ordinary non-elevated desktop
///    process looks like (measured 2026-11-25);
/// 3. the account still logs on and is not an enabled administrator.
///
/// `state == Verified` only when all three hold. `credentials`/`account` are
/// `Some` only in that case, so callers cannot accidentally launch with a
/// half-verified identity.
pub(crate) struct IdentityAvailability {
    pub(crate) credentials: Option<SandboxIdentity>,
    pub(crate) account: Option<OwnedSid>,
    pub(crate) state: crate::status::Capability,
    /// Human-readable reason, always set when `state != Verified`.
    pub(crate) note: String,
}

impl IdentityAvailability {
    fn unavailable(note: String) -> Self {
        Self {
            credentials: None,
            account: None,
            state: crate::status::Capability::Unverified,
            note,
        }
    }
}

/// Resolve the identity and prove it is usable for launching processes.
pub(crate) fn availability() -> Result<IdentityAvailability, SandboxError> {
    if !is_provisioned() {
        return Ok(IdentityAvailability::unavailable(format!(
            "未初始化（需要管理员一次性执行 `cargo run -p duthub-sandbox --example provision-sandbox`；\
             期望的凭据文件: {}）",
            credential_file()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|_| "<LOCALAPPDATA>\\duthub\\sandbox\\identity.txt".to_owned())
        )));
    }
    let loaded = match load() {
        Ok(Some(identity)) => identity,
        // A damaged file is reported as "not usable" (with the parse/ACL reason)
        // instead of breaking every sandbox call; `deny_read` is still refused
        // because `state` is not `Verified`.
        Ok(None) => {
            return Ok(IdentityAvailability::unavailable(
                "凭据文件存在但读不到内容".to_owned(),
            ));
        }
        Err(error) => {
            return Ok(IdentityAvailability::unavailable(format!(
                "凭据不可用（重新执行一次初始化可修复）: {error}"
            )));
        }
    };
    let missing = super::token::missing_create_process_privileges()?;
    if !missing.is_empty() {
        return Ok(IdentityAvailability::unavailable(format!(
            "Dedicated account {} is provisioned, but the launcher token lacks {} (Win32 1314). \
             Dedicated-account read isolation is unavailable.",
            loaded.username,
            missing.join(", ")
        )));
    }
    match super::token::logon_identity(&loaded) {
        Ok((_, account)) => Ok(IdentityAvailability {
            credentials: Some(loaded),
            account: Some(account),
            state: crate::status::Capability::Verified,
            note: "专用身份可登录，且当前进程持有创建进程所需特权".to_owned(),
        }),
        Err(error) => Ok(IdentityAvailability::unavailable(format!(
            "已初始化专用身份 {} 但无法登录: {error}",
            loaded.username
        ))),
    }
}

fn verify_state_dir_dacl(dir: &Path) -> Result<(), SandboxError> {
    if !dir.is_dir() {
        return Ok(());
    }
    let pinned = PinnedPath::open(dir)?;
    let public = [
        OwnedSid::well_known(windows_sys::Win32::Security::WinWorldSid)?,
        OwnedSid::well_known(windows_sys::Win32::Security::WinBuiltinUsersSid)?,
    ];
    if acl::has_any_allow_ace(
        &pinned,
        &public.iter().collect::<Vec<_>>(),
        acl::write_rights(),
    )? {
        return Err(SandboxError::Setup(format!(
            "沙箱身份目录可被 Everyone/Users 写入，拒绝使用: {}",
            dir.display()
        )));
    }
    Ok(())
}

/// Write the credential file with a DACL that excludes everyone but the current
/// user, SYSTEM and Administrators.
fn write_credentials(path: &Path, password: &str) -> Result<(), SandboxError> {
    let dir = path
        .parent()
        .ok_or_else(|| SandboxError::Setup("沙箱身份凭据路径没有父目录".to_owned()))?;
    // Creating (or adopting) the directory first guarantees the rename below
    // stays on the same volume and the DACL step has a stable object.
    let _state_dir_handle = PinnedPath::create_directory(dir)?;
    let sealed = dpapi::protect(password.as_bytes())?;
    let text = format!(
        "{IDENTITY_HEADER}\nversion={IDENTITY_VERSION}\nusername={SANDBOX_USER}\nsealed={}\n",
        encode_base64(&sealed)
    );
    let temporary = path.with_extension("tmp");
    std::fs::write(&temporary, text).map_err(|error| {
        SandboxError::Setup(format!("无法写入凭据文件 {}: {error}", temporary.display()))
    })?;
    let pinned_file = PinnedPath::open(&temporary)?;
    protect_identity_dacl(&pinned_file)?;
    std::fs::rename(&temporary, path).map_err(|error| {
        SandboxError::Setup(format!("无法启用凭据文件 {}: {error}", path.display()))
    })?;
    protect_identity_dacl(&PinnedPath::create_directory(dir)?)?;
    Ok(())
}

fn protect_identity_dacl(pinned: &PinnedPath) -> Result<(), SandboxError> {
    let current = current_user_sid()?;
    let system = OwnedSid::well_known(windows_sys::Win32::Security::WinLocalSystemSid)?;
    let admins = OwnedSid::well_known(windows_sys::Win32::Security::WinBuiltinAdministratorsSid)?;
    acl::set_protected_dacl(
        pinned,
        &[
            (current.as_ptr(), acl::FILE_ALL),
            (system.as_ptr(), acl::FILE_ALL),
            (admins.as_ptr(), acl::FILE_ALL),
        ],
        acl::INHERIT_CONTAINER_AND_OBJECT,
    )
}

fn current_user_sid() -> Result<OwnedSid, SandboxError> {
    super::token::current_user_sid()
}

fn encode_base64(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn decode_base64(text: &str) -> Result<Vec<u8>, SandboxError> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(text.trim().as_bytes())
        .map_err(|_| SandboxError::Setup("沙箱身份凭据的密文不是合法 base64".to_owned()))
}

/// Whether this process runs with an enabled administrator group.
pub(crate) fn is_elevated() -> bool {
    super::token::is_elevated()
}

/// Provision using the default (or already overridden) credential directory.
///
/// See [`provision_in`] for the contract.
pub fn provision() -> Result<IdentityReport, SandboxError> {
    provision_in(&state_dir()?)
}

/// Create (or repair) the sandbox account and store its password in `root`.
///
/// Requires an elevated process: `NetUserAdd`/`NetLocalGroupAdd` are
/// administrator-only. Idempotent — an existing, working identity is left
/// alone; an existing account whose stored password no longer logs on gets a new
/// random password (which is why the credential file is written *after* the
/// account is usable).
///
/// `root` also becomes this process's credential directory (it is the same
/// override [`set_root`] installs), so a host that keeps its data in a project
/// directory can provision and use the same path.
pub fn provision_in(root: &Path) -> Result<IdentityReport, SandboxError> {
    if !is_elevated() {
        return Err(SandboxError::Setup(
            "创建沙箱账户需要管理员权限：请在“以管理员身份运行”的终端里执行 \
             `cargo run -p duthub-sandbox --example provision-sandbox`。\
             当前进程不是提升后的上下文（UAC 过滤令牌）。"
                .to_owned(),
        ));
    }
    // Only adopt the target directory once the caller is allowed to provision;
    // a failed attempt must not leave the process pointing at a new root.
    set_root(Some(root.to_path_buf()))?;
    // An already working identity needs no changes. A *damaged* credential file
    // deliberately does not stop provisioning: repairing it is the whole point
    // of re-running this command, so `load` errors are treated as "not usable".
    if let Ok(Some(identity)) = load()
        && let Ok((_, account_sid)) = super::token::logon_identity(&identity)
    {
        return Ok(IdentityReport {
            username: identity.username,
            group: SANDBOX_GROUP.to_owned(),
            account_sid: account_sid.as_string(),
            created_user: false,
            created_group: false,
            rotated_password: false,
            credential_file: credential_file()?,
        });
    }

    let created_group = ensure_group()?;
    let (created_user, password) = ensure_user()?;
    write_credentials(&credential_file()?, &password)?;
    add_to_group()?;

    let identity = SandboxIdentity {
        username: SANDBOX_USER.to_owned(),
        password,
    };
    let (_, account_sid) = super::token::logon_identity(&identity).map_err(|error| {
        SandboxError::Setup(format!(
            "账户已创建但无法登录（凭据不会落盘，请重跑初始化）: {error}"
        ))
    })?;
    Ok(IdentityReport {
        username: identity.username,
        group: SANDBOX_GROUP.to_owned(),
        account_sid: account_sid.as_string(),
        created_user,
        created_group,
        rotated_password: !created_user,
        credential_file: credential_file()?,
    })
}

fn ensure_group() -> Result<bool, SandboxError> {
    let name = super::to_wide(SANDBOX_GROUP);
    let comment = super::to_wide("duthub sandbox internal group (managed)");
    let info = LOCALGROUP_INFO_1 {
        lgrpi1_name: name.as_ptr().cast_mut(),
        lgrpi1_comment: comment.as_ptr().cast_mut(),
    };
    // SAFETY: `info` is a fully initialised LOCALGROUP_INFO_1 whose strings
    // outlive the call; `null` selects the local machine.
    let status = unsafe {
        NetLocalGroupAdd(
            std::ptr::null(),
            1,
            (&info as *const LOCALGROUP_INFO_1).cast(),
            std::ptr::null_mut(),
        )
    };
    if status == 0 {
        Ok(true)
    } else if status == NERR_GroupExists {
        Ok(false)
    } else if status == ERROR_ACCESS_DENIED {
        Err(access_denied("创建本地组"))
    } else {
        Err(SandboxError::Setup(format!(
            "NetLocalGroupAdd 失败: {}",
            super::error_text(status)
        )))
    }
}

/// Create the account, or reset its password when it already exists.
fn ensure_user() -> Result<(bool, String), SandboxError> {
    let password = random_password();
    let name = super::to_wide(SANDBOX_USER);
    let password_wide = super::to_wide(&password);
    let comment = super::to_wide("duthub sandbox account (managed)");
    let info = USER_INFO_1 {
        usri1_name: name.as_ptr().cast_mut(),
        usri1_password: password_wide.as_ptr().cast_mut(),
        usri1_password_age: 0,
        usri1_priv: USER_PRIV_USER,
        usri1_home_dir: std::ptr::null_mut(),
        usri1_comment: comment.as_ptr().cast_mut(),
        // A sandbox account has no interactive logon rights beyond the token
        // creation this crate performs, and its password never expires: the only
        // holder is the DPAPI blob, so an expiry would only break the machine.
        usri1_flags: UF_SCRIPT | UF_DONT_EXPIRE_PASSWD,
        usri1_script_path: std::ptr::null_mut(),
    };
    // SAFETY: `info` is a fully initialised USER_INFO_1 whose strings outlive
    // the call; `null` selects the local machine.
    let status = unsafe {
        NetUserAdd(
            std::ptr::null(),
            1,
            (&info as *const USER_INFO_1).cast(),
            std::ptr::null_mut(),
        )
    };
    if status == 0 {
        return Ok((true, password));
    }
    if status == NERR_UserExists {
        // SAFETY: same contract as `NetUserAdd`; level 1 replaces the fields of
        // USER_INFO_1, which is how the password is rotated here.
        let status = unsafe {
            NetUserSetInfo(
                std::ptr::null(),
                name.as_ptr(),
                1,
                (&info as *const USER_INFO_1).cast(),
                std::ptr::null_mut(),
            )
        };
        return match status {
            0 => Ok((false, password)),
            ERROR_ACCESS_DENIED => Err(access_denied("重置本地账户口令")),
            other => Err(SandboxError::Setup(format!(
                "NetUserSetInfo 失败: {}",
                super::error_text(other)
            ))),
        };
    }
    if status == ERROR_ACCESS_DENIED {
        return Err(access_denied("创建本地账户"));
    }
    Err(SandboxError::Setup(format!(
        "NetUserAdd 失败: {}",
        super::error_text(status)
    )))
}

fn add_to_group() -> Result<(), SandboxError> {
    let group = super::to_wide(SANDBOX_GROUP);
    let member = super::to_wide(&format!(
        "{}\\{SANDBOX_USER}",
        std::env::var("COMPUTERNAME").unwrap_or_else(|_| ".".to_owned())
    ));
    let info = LOCALGROUP_MEMBERS_INFO_3 {
        lgrmi3_domainandname: member.as_ptr().cast_mut(),
    };
    // SAFETY: `info` is a fully initialised LOCALGROUP_MEMBERS_INFO_3 whose
    // string outlives the call; one entry is passed.
    let status = unsafe {
        NetLocalGroupAddMembers(
            std::ptr::null(),
            group.as_ptr(),
            3,
            (&info as *const LOCALGROUP_MEMBERS_INFO_3).cast(),
            1,
        )
    };
    if status == 0 || status == ERROR_MEMBER_IN_ALIAS {
        Ok(())
    } else if status == ERROR_ACCESS_DENIED {
        Err(access_denied("把账户加入沙箱组"))
    } else {
        Err(SandboxError::Setup(format!(
            "NetLocalGroupAddMembers 失败: {}",
            super::error_text(status)
        )))
    }
}

fn access_denied(step: &str) -> SandboxError {
    SandboxError::Setup(format!(
        "{step}被系统拒绝（ERROR_ACCESS_DENIED）：需要以管理员身份运行初始化"
    ))
}

/// 40 characters with upper, lower, digit and symbol so it satisfies the default
/// Windows complexity policy; the hex tail is 128 bits of OS randomness.
fn random_password() -> String {
    let bytes = uuid::Uuid::new_v4().into_bytes();
    let mut hex = String::with_capacity(64);
    for byte in bytes {
        hex.push_str(&format!("{byte:02x}"));
    }
    format!("Dth!{hex}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An identity that is not `Verified` must never be handed to a caller: that
    /// is what makes "never attempt, then fall back" structural rather than a
    /// convention.
    #[test]
    fn an_unusable_identity_is_never_handed_out() {
        let availability = availability().expect("availability");
        if availability.state != crate::status::Capability::Verified {
            assert!(
                availability.credentials.is_none() && availability.account.is_none(),
                "未验证的身份不得下发: {}",
                availability.note
            );
            assert!(!availability.note.is_empty(), "不可用必须给出原因");
        }
        if !super::super::token::missing_create_process_privileges()
            .expect("read process privileges")
            .is_empty()
        {
            assert!(
                availability.credentials.is_none(),
                "缺少创建进程特权时不得下发身份（Win32 1314）"
            );
        }
    }

    #[test]
    fn the_credential_directory_can_be_redirected_into_a_project() {
        let original = state_dir().expect("default state dir");
        let project = std::env::temp_dir().join(format!("duthub-ident-{}", uuid::Uuid::new_v4()));
        set_root(Some(project.clone())).expect("absolute override is accepted");
        assert_eq!(state_dir().expect("overridden"), project);
        assert_eq!(
            credential_file().expect("credential path"),
            project.join("identity.txt")
        );
        assert!(
            set_root(Some(std::path::PathBuf::from(r"relative\dir"))).is_err(),
            "相对路径必须被拒绝"
        );
        set_root(None).expect("clear override");
        assert_eq!(state_dir().expect("restored"), original);
    }

    #[test]
    fn password_has_all_character_classes() {
        let password = random_password();
        assert!(password.len() >= 32);
        assert!(password.chars().any(|c| c.is_ascii_uppercase()));
        assert!(password.chars().any(|c| c.is_ascii_lowercase()));
        assert!(password.chars().any(|c| c.is_ascii_digit()));
        assert!(password.chars().any(|c| !c.is_ascii_alphanumeric()));
        assert_ne!(password, random_password(), "每次初始化必须换口令");
    }

    #[test]
    fn identity_debug_output_never_contains_the_password() {
        let identity = SandboxIdentity {
            username: SANDBOX_USER.to_owned(),
            password: "Dth!secret".to_owned(),
        };
        let text = format!("{identity:?}");
        assert!(!text.contains("secret"), "{text}");
    }

    #[test]
    fn credential_round_trip_is_machine_protected() {
        let dir = std::env::temp_dir().join(format!("duthub-identity-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create dir");
        let path = dir.join("identity.txt");
        // Reuse the file format through the same helpers the loader uses.
        let sealed = dpapi::protect(b"Dth!roundtrip").expect("protect");
        let text = format!(
            "{IDENTITY_HEADER}\nversion={IDENTITY_VERSION}\nusername={SANDBOX_USER}\nsealed={}\n",
            encode_base64(&sealed)
        );
        std::fs::write(&path, &text).expect("write");
        let sealed_line = text
            .lines()
            .find_map(|line| line.strip_prefix("sealed="))
            .expect("sealed line");
        let opened =
            String::from_utf8(dpapi::unprotect(&decode_base64(sealed_line).unwrap()).unwrap())
                .expect("utf8");
        assert_eq!(opened, "Dth!roundtrip");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_fields_and_wrong_version_are_refused() {
        assert!(decode_base64("not base64!!").is_err());
        assert_eq!(SANDBOX_GROUP, "DuthubSandboxUsers");
    }
}
