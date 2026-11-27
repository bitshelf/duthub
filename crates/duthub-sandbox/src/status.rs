//! Read-only host capability probing backed by `mxc-sdk`.
//!
//! Reports factual containment backend and capability flags without launching
//! any sandboxed process. UI layers translate these structured facts into the
//! active locale rather than parsing free-form strings.

use mxc_sdk::v1::{self, BackendCapability};

/// Native OS containment backend detected on this host.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SandboxBackendKind {
    /// Windows 11 native PSEC `ProcessContainer`.
    ProcessContainer,
    /// Linux Bubblewrap (`bwrap`).
    Bubblewrap,
    /// macOS Seatbelt (`sandbox_init`).
    Seatbelt,
    /// No supported MXC containment backend is usable on this host.
    #[default]
    Unsupported,
}

impl SandboxBackendKind {
    /// Canonical wire/display identifier of the backend.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProcessContainer => "processcontainer",
            Self::Bubblewrap => "bubblewrap",
            Self::Seatbelt => "seatbelt",
            Self::Unsupported => "unsupported",
        }
    }
}

/// Factual sandbox capability report probed from `mxc-sdk`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "capability matrix probed directly from mxc-sdk"
)]
pub struct SandboxStatus {
    pub backend: SandboxBackendKind,
    /// Isolation tier reported by Windows `ProcessContainer` (e.g. `"base-container"`).
    pub tier: Option<String>,
    /// True when the host supports the native process containment backend.
    pub supported: bool,
    /// Kernel-enforced `denied_paths` read isolation is available.
    pub read_deny: bool,
    /// Enumeration-only directory policy (`enumerate_paths`) is available.
    pub enumerate_paths: bool,
    /// Explicit host-loopback allow (needed for Agent MCP listeners) is available.
    pub host_loopback: bool,
    /// Native denial capture (`capture_denials`) is available.
    pub capture_denials: bool,
    /// OS-level UI/clipboard/input-injection isolation is available.
    pub ui_isolation: bool,
    /// Non-fatal capability warnings reported by `mxc-sdk`.
    pub warnings: Vec<String>,
    /// Diagnostic explanation when `supported` is `false`.
    pub reason: Option<String>,
}

impl SandboxStatus {
    /// Status representing an unsupported host (explicitly non-enforced).
    pub fn unsupported(reason: impl Into<String>) -> Self {
        Self {
            backend: SandboxBackendKind::Unsupported,
            reason: Some(reason.into()),
            ..Self::default()
        }
    }

    /// Whether the host can enforce OS-level process and filesystem containment.
    #[must_use]
    pub fn is_enforced(&self) -> bool {
        self.supported && self.backend != SandboxBackendKind::Unsupported
    }

    /// Whether kernel-level read isolation (`denied_paths`) is enforced.
    #[must_use]
    pub fn supports_read_isolation(&self) -> bool {
        self.is_enforced() && self.read_deny
    }

    /// Whether untrusted scripts can be contained with read + write + network isolation.
    #[must_use]
    pub fn supports_script_isolation(&self) -> bool {
        self.is_enforced() && self.read_deny
    }

    /// Whether Agent hosts (which require loopback MCP access alongside
    /// read/write containment) are fully supported.
    #[must_use]
    pub fn supports_agent_isolation(&self) -> bool {
        self.is_enforced() && self.read_deny && self.host_loopback
    }

    /// Concise English diagnostic summary for logs and transcripts.
    #[must_use]
    pub fn summary(&self) -> String {
        if !self.is_enforced() {
            return match self.reason.as_deref() {
                Some(reason) => format!("unsupported ({reason})"),
                None => "unsupported".to_owned(),
            };
        }
        let mut caps = Vec::new();
        if self.read_deny {
            caps.push("read-deny");
        }
        if self.host_loopback {
            caps.push("loopback");
        }
        if self.ui_isolation {
            caps.push("ui-isolation");
        }
        if self.capture_denials {
            caps.push("capture-denials");
        }
        let name = match self.tier.as_deref() {
            Some(tier) => format!("{} ({tier})", self.backend.as_str()),
            None => self.backend.as_str().to_owned(),
        };
        if caps.is_empty() {
            name
        } else {
            format!("{name} [{}]", caps.join(", "))
        }
    }
}

/// Probe the current host through `mxc-sdk` without spawning any container.
#[must_use]
pub fn status() -> SandboxStatus {
    let support = v1::platform_support();
    let backends = v1::available_backends();

    #[cfg(target_os = "windows")]
    {
        if let Some(pc) = backends.iter().find(|b| b.backend == "processcontainer") {
            let probe = v1::probe(None).ok();
            let ui_isolation = probe.as_ref().is_some_and(|p| {
                p.probes.ui_capabilities.can_block_clipboard_read
                    && p.probes.ui_capabilities.can_block_clipboard_write
                    && p.probes.ui_capabilities.can_block_input_injection
            });
            let mut warnings = pc.warnings.clone();
            if let Some(p) = &probe {
                warnings.extend(p.warnings.iter().cloned());
            }
            let read_deny = pc
                .capabilities
                .contains(&BackendCapability::FilesystemDeniedPaths)
                || probe
                    .as_ref()
                    .is_some_and(|p| p.probes.base_container_supports_deny_paths);
            let enumerate_paths = pc
                .capabilities
                .contains(&BackendCapability::FilesystemEnumeratePaths)
                || probe
                    .as_ref()
                    .is_some_and(|p| p.probes.base_container_supports_enumerate_paths);
            let host_loopback = pc
                .capabilities
                .contains(&BackendCapability::IngressHostLoopbackAllow)
                || probe
                    .as_ref()
                    .is_some_and(|p| p.probes.base_container_supports_ingress_host_loopback_allow);
            let capture_denials = pc.capabilities.contains(&BackendCapability::CaptureDenials)
                || probe
                    .as_ref()
                    .is_some_and(|p| p.probes.native_capture_available);

            return SandboxStatus {
                backend: SandboxBackendKind::ProcessContainer,
                tier: pc.tier.clone(),
                supported: true,
                read_deny,
                enumerate_paths,
                host_loopback,
                capture_denials,
                ui_isolation,
                warnings,
                reason: None,
            };
        }
        SandboxStatus::unsupported(support.reason.unwrap_or_else(|| {
            "Windows ProcessContainer (native PSEC 1.1+) is not available on this OS build"
                .to_owned()
        }))
    }

    #[cfg(target_os = "linux")]
    {
        if let Some(bwrap) = backends.iter().find(|b| b.backend == "bubblewrap") {
            return SandboxStatus {
                backend: SandboxBackendKind::Bubblewrap,
                tier: None,
                supported: true,
                read_deny: true,
                enumerate_paths: false,
                host_loopback: true,
                capture_denials: false,
                ui_isolation: false,
                warnings: bwrap.warnings.clone(),
                reason: None,
            };
        }
        return SandboxStatus::unsupported(
            support
                .reason
                .unwrap_or_else(|| "Bubblewrap (bwrap) is not available on this host".to_owned()),
        );
    }

    #[cfg(target_os = "macos")]
    {
        if let Some(seatbelt) = backends.iter().find(|b| b.backend == "seatbelt") {
            let capture_denials = seatbelt
                .capabilities
                .contains(&BackendCapability::CaptureDenials);
            return SandboxStatus {
                backend: SandboxBackendKind::Seatbelt,
                tier: None,
                supported: true,
                read_deny: true,
                enumerate_paths: false,
                host_loopback: true,
                capture_denials,
                ui_isolation: true,
                warnings: seatbelt.warnings.clone(),
                reason: None,
            };
        }
        return SandboxStatus::unsupported(
            support
                .reason
                .unwrap_or_else(|| "macOS Seatbelt (sandbox_init) is not available".to_owned()),
        );
    }

    #[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
    {
        let _ = backends;
        SandboxStatus::unsupported(
            support
                .reason
                .unwrap_or_else(|| "MXC is not supported on this platform".to_owned()),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_status_claims_zero_capabilities() {
        let status = SandboxStatus::unsupported("no PSEC");
        assert_eq!(status.backend, SandboxBackendKind::Unsupported);
        assert!(!status.is_enforced());
        assert!(!status.supports_read_isolation());
        assert!(!status.supports_script_isolation());
        assert!(!status.supports_agent_isolation());
        assert!(status.summary().contains("no PSEC"));
    }

    #[test]
    fn agent_isolation_requires_read_deny_and_loopback() {
        let mut s = SandboxStatus {
            backend: SandboxBackendKind::ProcessContainer,
            tier: Some("base-container".to_owned()),
            supported: true,
            ..SandboxStatus::default()
        };
        assert!(s.is_enforced());
        assert!(!s.supports_agent_isolation());
        s.read_deny = true;
        assert!(s.supports_script_isolation());
        assert!(!s.supports_agent_isolation());
        s.host_loopback = true;
        assert!(s.supports_agent_isolation());
        assert!(s.summary().contains("processcontainer (base-container)"));
    }
}
