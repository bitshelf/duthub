//! Capability probing with an honest enum.
//!
//! There is no `sandboxed: bool` here on purpose. A host can have a restricted
//! token but no AppContainer, or an AppContainer but no Job Object; a UI that
//! collapses those into one flag ends up claiming isolation it does not have
//! (AGENTS.md pitfall #61). Callers must name the capability they rely on.

/// Whether one enforcement primitive is available and was verified by a real
/// API call during probing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    /// The primitive was established by a real call and read back.
    Verified,
    /// The platform or build does not provide it.
    Unavailable,
    /// It exists but the probe could not verify it (permissions, policy).
    Unverified,
}

impl Capability {
    pub fn is_verified(self) -> bool {
        self == Capability::Verified
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Capability::Verified => "已验证",
            Capability::Unavailable => "不可用",
            Capability::Unverified => "未验证",
        }
    }
}

/// What the one-time elevated provisioning created, for the CLI and the report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityReport {
    pub username: String,
    pub group: String,
    /// `S-1-5-21-…` of the sandbox account.
    pub account_sid: String,
    pub created_user: bool,
    pub created_group: bool,
    pub rotated_password: bool,
    /// Absolute path of the DPAPI-sealed credential file.
    pub credential_file: std::path::PathBuf,
}

/// What the current host can enforce. Every field is a probed fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxStatus {
    /// `windows` or `unsupported`.
    pub platform: &'static str,
    /// `CreateRestrictedToken` + integrity lowering on a real token.
    pub restricted_token: Capability,
    /// `CreateAppContainerProfile` (+ derivation for an existing profile).
    pub app_container: Capability,
    /// Job Object with `ActiveProcessLimit` / `ProcessMemoryLimit` /
    /// `KILL_ON_JOB_CLOSE`, queried back.
    pub job_limits: Capability,
    /// Deny-read ACE written for the sandbox identity SID and read back.
    ///
    /// Only `Verified` when [`Self::identity`] is verified *and* the ACE could be
    /// written for the sandbox account's SID: read isolation needs a principal
    /// other than the host user, and starting a process as that principal needs
    /// an elevated caller. `Unverified` otherwise.
    pub read_deny: Capability,
    /// The dedicated local sandbox account is provisioned, logs on, **and** this
    /// process holds the privileges needed to start a process as it
    /// (`SeAssignPrimaryTokenPrivilege` + `SeIncreaseQuotaPrivilege`; without
    /// them `CreateProcessAsUser` fails with Win32 1314).
    pub identity: Capability,
    /// Human-readable summary (UI/transcript text).
    pub detail: String,
}

impl SandboxStatus {
    /// A platform without a backend: every capability is explicitly
    /// unavailable, never silently absent.
    pub fn unsupported() -> Self {
        Self {
            platform: "unsupported",
            restricted_token: Capability::Unavailable,
            app_container: Capability::Unavailable,
            job_limits: Capability::Unavailable,
            read_deny: Capability::Unavailable,
            identity: Capability::Unavailable,
            detail: "当前平台没有 OS 沙箱后端（仅 Windows 提供受限令牌与 AppContainer），命令一律拒绝执行"
                .to_owned(),
        }
    }

    /// Whether scripts can be isolated (AppContainer + Job limits).
    pub fn script_isolation(&self) -> bool {
        self.app_container.is_verified() && self.job_limits.is_verified()
    }

    /// Whether the Agent host can be write/read isolated (restricted token +
    /// Job limits + read denial).
    pub fn agent_isolation(&self) -> bool {
        self.restricted_token.is_verified()
            && self.job_limits.is_verified()
            && self.read_deny.is_verified()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_status_never_claims_a_capability() {
        let status = SandboxStatus::unsupported();
        assert_eq!(status.platform, "unsupported");
        for capability in [
            status.restricted_token,
            status.app_container,
            status.job_limits,
            status.read_deny,
            status.identity,
        ] {
            assert_eq!(capability, Capability::Unavailable);
        }
        assert!(!status.script_isolation());
        assert!(!status.agent_isolation());
    }

    #[test]
    fn verified_capabilities_are_required_together() {
        let mut status = SandboxStatus::unsupported();
        status.app_container = Capability::Verified;
        assert!(!status.script_isolation(), "Job 未验证时不得宣称脚本可沙箱");
        status.job_limits = Capability::Verified;
        assert!(status.script_isolation());
        let mut status = SandboxStatus::unsupported();
        status.restricted_token = Capability::Verified;
        status.job_limits = Capability::Verified;
        assert!(
            !status.agent_isolation(),
            "读隔离未验证时不得宣称 Agent 可沙箱"
        );
        status.read_deny = Capability::Verified;
        assert!(status.agent_isolation());
    }
}
