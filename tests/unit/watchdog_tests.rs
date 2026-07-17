use super::{WatchdogState, update_watchdog_state};

// --- Pure state machine tests (no threads, no shm) ---

#[test]
fn test_state_machine_normal_heartbeat() {
    let mut state = WatchdogState {
        prev_heartbeat: 0,
        hang_accumulated_ms: 0,
        cooldown_remaining_ms: 0,
    };

    // Heartbeat advances each tick — should never trigger ANR
    for i in 1..=20u64 {
        let result = update_watchdog_state(&mut state, i, 50, 200, 5000);
        assert!(
            result.is_none(),
            "tick {i}: expected no ANR when heartbeat advances"
        );
        assert_eq!(state.hang_accumulated_ms, 0);
        assert_eq!(state.prev_heartbeat, i);
    }
}

#[test]
fn test_state_machine_stale_triggers_anr() {
    let mut state = WatchdogState {
        prev_heartbeat: 42,
        hang_accumulated_ms: 0,
        cooldown_remaining_ms: 0,
    };

    let threshold_ms = 200;
    let check_interval_ms = 50;

    // First 3 ticks (150ms) — below threshold
    for tick in 1..=3 {
        let result = update_watchdog_state(&mut state, 42, check_interval_ms, threshold_ms, 5000);
        assert!(result.is_none(), "tick {tick}: should not trigger yet");
    }
    assert_eq!(state.hang_accumulated_ms, 150);

    // 4th tick (200ms) — reaches threshold, should trigger
    let result = update_watchdog_state(&mut state, 42, check_interval_ms, threshold_ms, 5000);
    assert!(result.is_some(), "expected ANR trigger at threshold");
    assert_eq!(result.unwrap(), 200);
    // After trigger, hang resets and cooldown is set
    assert_eq!(state.hang_accumulated_ms, 0);
    assert_eq!(state.cooldown_remaining_ms, 5000);
}

#[test]
fn test_state_machine_cooldown_prevents_retrigger() {
    let mut state = WatchdogState {
        prev_heartbeat: 10,
        hang_accumulated_ms: 0,
        cooldown_remaining_ms: 0,
    };

    let check_interval_ms = 50;
    let threshold_ms = 100;
    let cooldown_ms = 300;

    // Trigger ANR (heartbeat stays at 10 for 100ms)
    update_watchdog_state(&mut state, 10, check_interval_ms, threshold_ms, cooldown_ms);
    let result =
        update_watchdog_state(&mut state, 10, check_interval_ms, threshold_ms, cooldown_ms);
    assert!(result.is_some(), "expected ANR trigger");
    assert_eq!(state.cooldown_remaining_ms, cooldown_ms);

    // During cooldown, stale heartbeat should NOT retrigger
    for tick in 1..=5 {
        let result =
            update_watchdog_state(&mut state, 10, check_interval_ms, threshold_ms, cooldown_ms);
        assert!(
            result.is_none(),
            "tick {tick}: should not retrigger during cooldown"
        );
        assert_eq!(state.hang_accumulated_ms, 0);
    }

    // Cooldown should be decreasing
    assert!(state.cooldown_remaining_ms < cooldown_ms);
}

#[test]
fn test_state_machine_heartbeat_resets_hang() {
    let mut state = WatchdogState {
        prev_heartbeat: 5,
        hang_accumulated_ms: 0,
        cooldown_remaining_ms: 0,
    };

    let check_interval_ms = 50;
    let threshold_ms = 200;

    // Accumulate 150ms of hang (3 ticks, stale heartbeat)
    for _ in 0..3 {
        update_watchdog_state(&mut state, 5, check_interval_ms, threshold_ms, 5000);
    }
    assert_eq!(state.hang_accumulated_ms, 150);

    // Heartbeat changes — accumulator must reset
    let result = update_watchdog_state(&mut state, 6, check_interval_ms, threshold_ms, 5000);
    assert!(result.is_none(), "heartbeat changed, no ANR expected");
    assert_eq!(state.hang_accumulated_ms, 0);
    assert_eq!(state.prev_heartbeat, 6);

    // Another 3 stale ticks — still below threshold (150ms < 200ms)
    for _ in 0..3 {
        let r = update_watchdog_state(&mut state, 6, check_interval_ms, threshold_ms, 5000);
        assert!(r.is_none());
    }
    assert_eq!(state.hang_accumulated_ms, 150);
}
