//! Lightweight agent status types, kernel crash detector, and cached goose
//! installation state used by the desktop workbench and alert pipeline.

use std::time::{Duration, Instant};

use duthub_desktop::agent_host::GooseInstallState;

/// How long the Agent page may reuse a classified goose install fact.
///
/// `goose_install_state()` reads the install manifest and scans PATH; two
/// seconds keeps the page fresh without doing disk I/O on every render frame.
const GOOSE_STATE_TTL: Duration = Duration::from_secs(2);

pub(crate) fn serial_crash_evidence(log: &str) -> Option<&str> {
    log.lines().rev().map(str::trim).find(|line| {
        let message = line
            .strip_prefix('[')
            .and_then(|timestamp| timestamp.split_once(']'))
            .filter(|(timestamp, _)| timestamp.trim().parse::<f64>().is_ok())
            .map_or(*line, |(_, message)| message.trim_start());
        [
            "Kernel panic - not syncing:",
            "Oops:",
            "Internal error: Oops:",
            "BUG: unable to handle kernel",
            "watchdog: BUG: soft lockup",
            "watchdog: BUG: hard LOCKUP",
        ]
        .iter()
        .any(|signature| message.starts_with(signature))
    })
}

/// Cached goose installation facts shared by the Agent configuration page.
#[derive(Default)]
pub struct AgentState {
    pub goose_install: Option<(Instant, GooseInstallState)>,
}

impl AgentState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn goose_install_state(&mut self) -> GooseInstallState {
        let now = Instant::now();
        match &self.goose_install {
            Some((checked, state)) if now.saturating_duration_since(*checked) < GOOSE_STATE_TTL => {
                state.clone()
            }
            _ => {
                let state = duthub_desktop::agent_host::goose_install_state();
                self.goose_install = Some((now, state.clone()));
                state
            }
        }
    }
}

#[cfg(test)]
mod crash_tests {
    use super::serial_crash_evidence;

    #[test]
    fn uses_the_device_log_line_as_evidence() {
        let panic = "[ 23.50] Kernel panic - not syncing: Fatal exception";
        assert_eq!(
            serial_crash_evidence(&format!("booted\r\n{panic}\r\n")),
            Some(panic)
        );
        assert_eq!(
            serial_crash_evidence("# grep 'Kernel panic - not syncing:' /var/log/kern.log"),
            None
        );
    }
}
