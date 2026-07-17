//! ANR (Application Not Responding) watchdog — pure state machine.
//!
//! The event loop polls the producer-readiness handshake and heartbeat counter
//! in shared memory. Time before the producer is ready, and time spent doing
//! monitor-owned work, is never counted as application hang time.
//!
//! Design doc: `docs/plans/crash_reporter.md` L417-487

use std::time::Instant;

/// Whether post-capture heartbeat sampling can retain elapsed application time
/// from before the monitor-owned interval.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MonitorWorkRebase {
    /// The heartbeat is unchanged, so only the monitor-owned duration is
    /// removed from the elapsed clock.
    PreserveElapsed,
    /// Readiness changed or the heartbeat advanced, so the completed sample is
    /// a new baseline and elapsed time from before it cannot be attributed to
    /// the new heartbeat value.
    ResetElapsed,
}

/// Move an ANR check baseline forward by exactly the monitor-owned interval.
///
/// When `last_check <= monitor_work_started <= monitor_work_finished`, elapsed
/// time measured after the returned baseline contains the application-running
/// time before and after monitor work, but not the monitor work itself. Invalid
/// or overflowing timestamp combinations conservatively reset to `finished`.
#[must_use]
pub(crate) fn exclude_monitor_work_from_anr_clock(
    last_check: Instant,
    monitor_work_started: Instant,
    monitor_work_finished: Instant,
) -> Instant {
    let excluded = monitor_work_finished.saturating_duration_since(monitor_work_started);
    let Some(adjusted) = last_check.checked_add(excluded) else {
        return monitor_work_finished;
    };
    if adjusted > monitor_work_finished {
        monitor_work_finished
    } else {
        adjusted
    }
}

/// Pure state for the ANR watchdog state machine (no I/O, no threads).
pub(crate) struct WatchdogState {
    initial_warmup_ms: u64,
    warmup_remaining_ms: u64,
    prev_heartbeat: Option<u64>,
    hang_accumulated_ms: u64,
    cooldown_remaining_ms: u64,
}

impl WatchdogState {
    /// Create a watchdog from the producer state observed when monitoring
    /// begins. `None` leaves the watchdog disarmed until the first published
    /// heartbeat is observed.
    #[must_use]
    pub(crate) const fn new(warmup_ms: u64, initial_heartbeat: Option<u64>) -> Self {
        Self {
            initial_warmup_ms: warmup_ms,
            warmup_remaining_ms: warmup_ms,
            prev_heartbeat: initial_heartbeat,
            hang_accumulated_ms: 0,
            cooldown_remaining_ms: 0,
        }
    }

    /// Establish a new heartbeat baseline after monitor-owned work.
    ///
    /// A live observation never changes an active warmup or cooldown. If the
    /// heartbeat is unchanged, already-observed application-running stale time
    /// remains valid and the caller removes only the monitor-owned duration
    /// from its clock. A changed heartbeat establishes a fresh baseline.
    /// Losing producer readiness fully disarms the watchdog.
    pub(crate) fn rebase_after_monitor_work(
        &mut self,
        current_heartbeat: Option<u64>,
    ) -> MonitorWorkRebase {
        let Some(current_heartbeat) = current_heartbeat else {
            self.disarm();
            return MonitorWorkRebase::ResetElapsed;
        };

        match self.prev_heartbeat {
            None => {
                self.prev_heartbeat = Some(current_heartbeat);
                MonitorWorkRebase::ResetElapsed
            }
            Some(previous_heartbeat) if previous_heartbeat != current_heartbeat => {
                self.prev_heartbeat = Some(current_heartbeat);
                self.hang_accumulated_ms = 0;
                MonitorWorkRebase::ResetElapsed
            }
            Some(_) => MonitorWorkRebase::PreserveElapsed,
        }
    }

    fn disarm(&mut self) {
        self.warmup_remaining_ms = self.initial_warmup_ms;
        self.prev_heartbeat = None;
        self.hang_accumulated_ms = 0;
        self.cooldown_remaining_ms = 0;
    }
}

/// Advance the watchdog state machine by one tick.
///
/// Returns `Some(hang_duration_ms)` if an ANR event should be fired, `None` otherwise.
pub(crate) fn update_watchdog_state(
    state: &mut WatchdogState,
    current_heartbeat: Option<u64>,
    elapsed_ms: u64,
    threshold_ms: u64,
    cooldown_ms: u64,
) -> Option<u64> {
    let Some(current_heartbeat) = current_heartbeat else {
        state.disarm();
        return None;
    };

    // The first ready observation only establishes a baseline. In particular,
    // elapsed time from before producer readiness must not consume warmup or
    // count toward a hang.
    let Some(previous_heartbeat) = state.prev_heartbeat else {
        state.prev_heartbeat = Some(current_heartbeat);
        return None;
    };

    // Startup grace begins only after producer readiness. The complete tick
    // that crosses the warmup boundary remains excluded conservatively.
    if state.warmup_remaining_ms > 0 {
        state.warmup_remaining_ms = state.warmup_remaining_ms.saturating_sub(elapsed_ms);
        state.prev_heartbeat = Some(current_heartbeat);
        state.hang_accumulated_ms = 0;
        return None;
    }

    // Cooldown: skip checks after ANR was triggered.
    if state.cooldown_remaining_ms > 0 {
        state.cooldown_remaining_ms = state.cooldown_remaining_ms.saturating_sub(elapsed_ms);
        state.prev_heartbeat = Some(current_heartbeat);
        state.hang_accumulated_ms = 0;
        return None;
    }

    if current_heartbeat == previous_heartbeat {
        // Heartbeat unchanged — app may be hung
        state.hang_accumulated_ms = state.hang_accumulated_ms.saturating_add(elapsed_ms);

        if state.hang_accumulated_ms >= threshold_ms {
            let duration = state.hang_accumulated_ms;
            // Enter cooldown
            state.cooldown_remaining_ms = cooldown_ms;
            state.hang_accumulated_ms = 0;
            state.prev_heartbeat = Some(current_heartbeat);
            return Some(duration);
        }
    } else {
        // Heartbeat advanced — app is responsive
        state.hang_accumulated_ms = 0;
    }

    state.prev_heartbeat = Some(current_heartbeat);
    None
}

#[cfg(test)]
#[path = "../tests/unit/watchdog_tests.rs"]
mod tests;
