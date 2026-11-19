//! Automatic-restart budget for Agent workers.
//!
//! An ACP worker that dropped out after reaching `Ready` is restarted at most
//! three times inside a sliding ten-minute window, with a 1s/2s/4s exponential
//! backoff. The window is tracked here instead of using a timer per attempt so
//! the policy stays testable with an injected [`Instant`].

use std::time::{Duration, Instant};

/// Sliding window for automatic restarts.
pub const RESTART_WINDOW: Duration = Duration::from_secs(10 * 60);
/// Maximum automatic restarts allowed inside [`RESTART_WINDOW`].
pub const MAX_RESTARTS_PER_WINDOW: usize = 3;
/// First restart delay; the following delays double up to [`MAX_RESTART_DELAY`].
pub const INITIAL_RESTART_DELAY: Duration = Duration::from_secs(1);
/// Longest restart delay (1s, 2s, 4s).
pub const MAX_RESTART_DELAY: Duration = Duration::from_secs(4);

/// Restart admission plus the exponential delay sequence.
#[derive(Debug)]
pub struct RestartBudget {
    window: Duration,
    limit: usize,
    attempts: Vec<Instant>,
    delay_step: usize,
}

impl Default for RestartBudget {
    fn default() -> Self {
        Self::new()
    }
}

impl RestartBudget {
    pub fn new() -> Self {
        Self {
            window: RESTART_WINDOW,
            limit: MAX_RESTARTS_PER_WINDOW,
            attempts: Vec::new(),
            delay_step: 0,
        }
    }

    /// Record a restart at `now` when fewer than [`MAX_RESTARTS_PER_WINDOW`]
    /// restarts happened inside the trailing [`RESTART_WINDOW`].
    ///
    /// Timestamps at exactly one window old are evicted, so the window slides
    /// with wall-clock time instead of resetting on a fixed boundary. When the
    /// sliding window evicts attempts, `delay_step` scales back to match the
    /// active count inside the window so future restarts receive fresh delays.
    pub fn allow(&mut self, now: Instant) -> bool {
        let before = self.attempts.len();
        self.attempts
            .retain(|at| now.saturating_duration_since(*at) < self.window);
        if self.attempts.len() < before {
            self.delay_step = self.delay_step.min(self.attempts.len());
        }
        if self.attempts.len() >= self.limit {
            return false;
        }
        self.attempts.push(now);
        true
    }

    /// Next backoff delay (1s, 2s, 4s), or `None` once the sequence is spent.
    pub fn next_delay(&mut self) -> Option<Duration> {
        if self.delay_step >= self.limit {
            return None;
        }
        let multiplier = 1u32.checked_shl(self.delay_step as u32).unwrap_or(u32::MAX);
        self.delay_step += 1;
        Some(
            INITIAL_RESTART_DELAY
                .saturating_mul(multiplier)
                .min(MAX_RESTART_DELAY),
        )
    }

    /// Restarts admitted inside the current window (diagnostics/tests).
    pub fn restarts_in_window(&self, now: Instant) -> usize {
        self.attempts
            .iter()
            .filter(|at| now.saturating_duration_since(**at) < self.window)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_budget_allows_three_in_ten_minutes_and_denies_fourth() {
        let start = Instant::now();
        let mut budget = RestartBudget::new();
        assert!(budget.allow(start));
        assert!(budget.allow(start + Duration::from_secs(60)));
        assert!(budget.allow(start + Duration::from_secs(120)));
        assert!(
            !budget.allow(start + Duration::from_secs(180)),
            "10 分钟窗口内第 4 次重启必须拒绝"
        );
        assert_eq!(
            budget.restarts_in_window(start + Duration::from_secs(180)),
            3
        );

        // The window slides: once the first admission is ten minutes old it no
        // longer counts, but the two newer admissions still do.
        assert!(budget.allow(start + RESTART_WINDOW + Duration::from_secs(1)));
        assert!(!budget.allow(start + RESTART_WINDOW + Duration::from_secs(2)));
    }

    #[test]
    fn restart_delays_are_one_two_four_seconds_then_exhausted() {
        let mut budget = RestartBudget::new();
        assert_eq!(budget.next_delay(), Some(Duration::from_secs(1)));
        assert_eq!(budget.next_delay(), Some(Duration::from_secs(2)));
        assert_eq!(budget.next_delay(), Some(Duration::from_secs(4)));
        assert_eq!(budget.next_delay(), None);
    }

    #[test]
    fn restarts_expire_exactly_at_the_window_boundary() {
        let start = Instant::now();
        let mut budget = RestartBudget::new();
        assert!(budget.allow(start));
        assert!(budget.allow(start));
        assert!(budget.allow(start));
        assert!(!budget.allow(start + Duration::from_secs(300)));
        assert!(budget.allow(start + RESTART_WINDOW));
        assert_eq!(
            budget.restarts_in_window(start + RESTART_WINDOW),
            1,
            "恰好在窗口边界上的旧记录必须被移除"
        );
    }
}
