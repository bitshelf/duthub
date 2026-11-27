//! Port reservation registry shared between the A2A runtime and the host.
//!
//! The runtime reserves a port before binding and releases it on
//! [`crate::listener::A2aRuntime::stop_dut`]; ports reserved by other
//! subsystems (MCP listeners, media servers) are seeded by the host through
//! [`crate::listener::A2aRuntime::start`]'s `reserved_ports` argument, so an
//! A2A listener can never take over a port another surface already owns.

use std::collections::HashMap;
use std::sync::Mutex;

/// Who owns a reserved port. The label never carries secrets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortOwner {
    /// Per-DUT A2A listener bound by the A2A runtime.
    A2a { dut_id: String },
    /// Port held by another subsystem (diagnostic label only).
    External { label: String },
}

impl PortOwner {
    /// Short human-readable identity for conflict reasons and logs.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            PortOwner::A2a { dut_id } => format!("a2a:{dut_id}"),
            PortOwner::External { label } => format!("external:{label}"),
        }
    }
}

/// Port already reserved by someone else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortTaken {
    pub by: PortOwner,
}

/// Maps reserved ports to their owners. Reservation is the single gate the
/// runtime consults before `TcpListener::bind`; OS-level bind failures are
/// reported separately as `A2aStatus::PortConflict`.
#[derive(Debug, Default)]
pub struct PortRegistry {
    reserved: Mutex<HashMap<u16, PortOwner>>,
}

impl PortRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Reserve `port` for `owner`. Fails when any owner (including this
    /// registry's other A2A listeners) already holds it.
    pub fn reserve(&self, port: u16, owner: PortOwner) -> Result<(), PortTaken> {
        let mut reserved = self
            .reserved
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(holder) = reserved.get(&port) { Err(PortTaken { by: holder.clone() }) } else {
            reserved.insert(port, owner);
            Ok(())
        }
    }

    /// Release `port` only if it is still owned by `owner`. Returns whether a
    /// reservation was actually removed.
    pub fn release(&self, port: u16, owner: &PortOwner) -> bool {
        let mut reserved = self
            .reserved
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match reserved.get(&port) {
            Some(holder) if holder == owner => reserved.remove(&port).is_some(),
            _ => false,
        }
    }

    pub fn is_reserved(&self, port: u16) -> bool {
        self.reserved
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&port)
    }

    pub fn owner_of(&self, port: u16) -> Option<PortOwner> {
        self.reserved
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&port)
            .cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserve_release_roundtrip() {
        let registry = PortRegistry::new();
        let owner = PortOwner::A2a {
            dut_id: "dut-a".into(),
        };
        registry.reserve(8790, owner.clone()).unwrap();
        assert!(registry.is_reserved(8790));
        assert_eq!(
            registry.owner_of(8790),
            Some(PortOwner::A2a {
                dut_id: "dut-a".into()
            })
        );
        assert!(registry.release(8790, &owner));
        assert!(!registry.is_reserved(8790));
    }

    #[test]
    fn double_reserve_reports_holder() {
        let registry = PortRegistry::new();
        registry
            .reserve(
                8791,
                PortOwner::External {
                    label: "mcp".into(),
                },
            )
            .unwrap();
        let err = registry
            .reserve(
                8791,
                PortOwner::A2a {
                    dut_id: "dut-b".into(),
                },
            )
            .unwrap_err();
        assert_eq!(
            err.by,
            PortOwner::External {
                label: "mcp".into()
            }
        );
    }

    #[test]
    fn release_only_by_owner() {
        let registry = PortRegistry::new();
        registry
            .reserve(
                8792,
                PortOwner::A2a {
                    dut_id: "dut-c".into(),
                },
            )
            .unwrap();
        assert!(!registry.release(8792, &PortOwner::External { label: "x".into() }));
        assert!(registry.is_reserved(8792));
    }
}
