//! ANR (Application Not Responding) watchdog — pure state machine.
//!
//! The event loop polls the heartbeat counter in shared memory at
//! `check_interval_ms` intervals. If the counter does not advance for
//! `threshold_ms`, an ANR event is fired inline (no dedicated thread).
//!
//! Design doc: `docs/plans/crash_reporter.md` L417-487

/// Pure state for the ANR watchdog state machine (no I/O, no threads).
pub(crate) struct WatchdogState {
    pub prev_heartbeat: u64,
    pub hang_accumulated_ms: u64,
    pub cooldown_remaining_ms: u64,
}

/// Advance the watchdog state machine by one tick.
///
/// Returns `Some(hang_duration_ms)` if an ANR event should be fired, `None` otherwise.
pub(crate) fn update_watchdog_state(
    state: &mut WatchdogState,
    current_heartbeat: u64,
    check_interval_ms: u64,
    threshold_ms: u64,
    cooldown_ms: u64,
) -> Option<u64> {
    // Cooldown: skip checks after ANR was triggered
    if state.cooldown_remaining_ms > 0 {
        state.cooldown_remaining_ms = state
            .cooldown_remaining_ms
            .saturating_sub(check_interval_ms);
        state.prev_heartbeat = current_heartbeat;
        state.hang_accumulated_ms = 0;
        return None;
    }

    if current_heartbeat == state.prev_heartbeat {
        // Heartbeat unchanged — app may be hung
        state.hang_accumulated_ms += check_interval_ms;

        if state.hang_accumulated_ms >= threshold_ms {
            let duration = state.hang_accumulated_ms;
            // Enter cooldown
            state.cooldown_remaining_ms = cooldown_ms;
            state.hang_accumulated_ms = 0;
            state.prev_heartbeat = current_heartbeat;
            return Some(duration);
        }
    } else {
        // Heartbeat advanced — app is responsive
        state.hang_accumulated_ms = 0;
    }

    state.prev_heartbeat = current_heartbeat;
    None
}

#[cfg(test)]
#[path = "../tests/unit/watchdog_tests.rs"]
mod tests;
