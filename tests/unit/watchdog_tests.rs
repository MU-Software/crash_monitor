use std::time::{Duration, Instant};

use super::{
    MonitorWorkRebase, WatchdogState, exclude_monitor_work_from_anr_clock, update_watchdog_state,
};

// --- Pure state machine tests (no threads, no shm) ---

#[test]
fn test_state_machine_normal_heartbeat() {
    let mut state = WatchdogState::new(0, Some(0));

    // Heartbeat advances each tick — should never trigger ANR
    for i in 1..=20u64 {
        let result = update_watchdog_state(&mut state, Some(i), 50, 200, 5000);
        assert!(
            result.is_none(),
            "tick {i}: expected no ANR when heartbeat advances"
        );
        assert_eq!(state.hang_accumulated_ms, 0);
        assert_eq!(state.prev_heartbeat, Some(i));
    }
}

#[test]
fn test_state_machine_stale_triggers_anr() {
    let mut state = WatchdogState::new(0, Some(42));

    let threshold_ms = 200;
    let check_interval_ms = 50;

    // First 3 ticks (150ms) — below threshold
    for tick in 1..=3 {
        let result =
            update_watchdog_state(&mut state, Some(42), check_interval_ms, threshold_ms, 5000);
        assert!(result.is_none(), "tick {tick}: should not trigger yet");
    }
    assert_eq!(state.hang_accumulated_ms, 150);

    // 4th tick (200ms) — reaches threshold, should trigger
    let result = update_watchdog_state(&mut state, Some(42), check_interval_ms, threshold_ms, 5000);
    assert!(result.is_some(), "expected ANR trigger at threshold");
    assert_eq!(result.unwrap(), 200);
    // After trigger, hang resets and cooldown is set
    assert_eq!(state.hang_accumulated_ms, 0);
    assert_eq!(state.cooldown_remaining_ms, 5000);
}

#[test]
fn test_state_machine_cooldown_prevents_retrigger() {
    let mut state = WatchdogState::new(0, Some(10));

    let check_interval_ms = 50;
    let threshold_ms = 100;
    let cooldown_ms = 300;

    // Trigger ANR (heartbeat stays at 10 for 100ms)
    update_watchdog_state(
        &mut state,
        Some(10),
        check_interval_ms,
        threshold_ms,
        cooldown_ms,
    );
    let result = update_watchdog_state(
        &mut state,
        Some(10),
        check_interval_ms,
        threshold_ms,
        cooldown_ms,
    );
    assert!(result.is_some(), "expected ANR trigger");
    assert_eq!(state.cooldown_remaining_ms, cooldown_ms);

    // During cooldown, stale heartbeat should NOT retrigger
    for tick in 1..=5 {
        let result = update_watchdog_state(
            &mut state,
            Some(10),
            check_interval_ms,
            threshold_ms,
            cooldown_ms,
        );
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
    let mut state = WatchdogState::new(0, Some(5));

    let check_interval_ms = 50;
    let threshold_ms = 200;

    // Accumulate 150ms of hang (3 ticks, stale heartbeat)
    for _ in 0..3 {
        update_watchdog_state(&mut state, Some(5), check_interval_ms, threshold_ms, 5000);
    }
    assert_eq!(state.hang_accumulated_ms, 150);

    // Heartbeat changes — accumulator must reset
    let result = update_watchdog_state(&mut state, Some(6), check_interval_ms, threshold_ms, 5000);
    assert!(result.is_none(), "heartbeat changed, no ANR expected");
    assert_eq!(state.hang_accumulated_ms, 0);
    assert_eq!(state.prev_heartbeat, Some(6));

    // Another 3 stale ticks — still below threshold (150ms < 200ms)
    for _ in 0..3 {
        let r = update_watchdog_state(&mut state, Some(6), check_interval_ms, threshold_ms, 5000);
        assert!(r.is_none());
    }
    assert_eq!(state.hang_accumulated_ms, 150);
}

#[test]
fn test_unpublished_producer_never_starts_watchdog() {
    let mut state = WatchdogState::new(250, None);

    for _ in 0..10 {
        assert!(update_watchdog_state(&mut state, None, 1_000, 100, 500).is_none());
    }

    assert_eq!(state.prev_heartbeat, None);
    assert_eq!(state.warmup_remaining_ms, 250);
    assert_eq!(state.hang_accumulated_ms, 0);
    assert_eq!(state.cooldown_remaining_ms, 0);
}

#[test]
fn test_first_ready_observation_only_establishes_baseline() {
    let mut state = WatchdogState::new(100, None);

    assert!(update_watchdog_state(&mut state, Some(7), 10_000, 50, 500).is_none());
    assert_eq!(state.prev_heartbeat, Some(7));
    assert_eq!(state.warmup_remaining_ms, 100);
    assert_eq!(state.hang_accumulated_ms, 0);

    assert!(update_watchdog_state(&mut state, Some(7), 40, 50, 500).is_none());
    assert_eq!(state.warmup_remaining_ms, 60);
    assert_eq!(state.hang_accumulated_ms, 0);
}

#[test]
fn test_warmup_crossing_tick_is_fully_excluded() {
    let mut state = WatchdogState::new(50, Some(7));

    assert!(update_watchdog_state(&mut state, Some(7), 100, 50, 500).is_none());
    assert_eq!(state.warmup_remaining_ms, 0);
    assert_eq!(state.hang_accumulated_ms, 0);

    assert_eq!(
        update_watchdog_state(&mut state, Some(7), 50, 50, 500),
        Some(50)
    );
}

#[test]
fn test_ready_heartbeat_zero_is_a_valid_baseline() {
    let mut state = WatchdogState::new(0, Some(0));

    assert!(update_watchdog_state(&mut state, Some(0), 49, 50, 500).is_none());
    assert_eq!(
        update_watchdog_state(&mut state, Some(0), 1, 50, 500),
        Some(50)
    );
}

#[test]
fn test_heartbeat_wrap_is_observed_as_progress() {
    let mut state = WatchdogState::new(0, Some(u64::MAX));

    assert!(update_watchdog_state(&mut state, Some(0), 200, 100, 500).is_none());
    assert_eq!(state.prev_heartbeat, Some(0));
    assert_eq!(state.hang_accumulated_ms, 0);

    assert_eq!(
        update_watchdog_state(&mut state, Some(0), 100, 100, 500),
        Some(100)
    );
}

#[test]
fn test_rebase_resets_hang_and_preserves_warmup() {
    let mut state = WatchdogState::new(100, Some(1));
    assert!(update_watchdog_state(&mut state, Some(1), 40, 50, 500).is_none());
    assert_eq!(state.warmup_remaining_ms, 60);

    assert_eq!(
        state.rebase_after_monitor_work(Some(2)),
        MonitorWorkRebase::ResetElapsed
    );

    assert_eq!(state.prev_heartbeat, Some(2));
    assert_eq!(state.warmup_remaining_ms, 60);
    assert_eq!(state.hang_accumulated_ms, 0);
    assert_eq!(state.cooldown_remaining_ms, 0);
}

#[test]
fn test_rebase_preserves_pre_event_hang_when_heartbeat_is_unchanged() {
    let mut state = WatchdogState::new(0, Some(2));
    assert!(update_watchdog_state(&mut state, Some(2), 75, 100, 500).is_none());
    assert_eq!(state.hang_accumulated_ms, 75);

    assert_eq!(
        state.rebase_after_monitor_work(Some(2)),
        MonitorWorkRebase::PreserveElapsed
    );

    assert_eq!(state.hang_accumulated_ms, 75);
    assert_eq!(
        update_watchdog_state(&mut state, Some(2), 25, 100, 500),
        Some(100)
    );
}

#[test]
fn test_rebase_resets_hang_and_preserves_cooldown() {
    let mut state = WatchdogState::new(0, Some(9));
    assert_eq!(
        update_watchdog_state(&mut state, Some(9), 100, 100, 300),
        Some(100)
    );

    assert_eq!(
        state.rebase_after_monitor_work(Some(10)),
        MonitorWorkRebase::ResetElapsed
    );

    assert_eq!(state.prev_heartbeat, Some(10));
    assert_eq!(state.hang_accumulated_ms, 0);
    assert_eq!(state.cooldown_remaining_ms, 300);
    assert!(update_watchdog_state(&mut state, Some(10), 50, 100, 300).is_none());
    assert_eq!(state.cooldown_remaining_ms, 250);
}

#[test]
fn test_rebase_without_ready_producer_fully_disarms() {
    let mut state = WatchdogState::new(100, Some(3));
    state.warmup_remaining_ms = 0;
    assert_eq!(
        update_watchdog_state(&mut state, Some(3), 100, 100, 300),
        Some(100)
    );

    assert_eq!(
        state.rebase_after_monitor_work(None),
        MonitorWorkRebase::ResetElapsed
    );

    assert_eq!(state.prev_heartbeat, None);
    assert_eq!(state.warmup_remaining_ms, 100);
    assert_eq!(state.hang_accumulated_ms, 0);
    assert_eq!(state.cooldown_remaining_ms, 0);

    assert!(update_watchdog_state(&mut state, Some(4), 5_000, 100, 300).is_none());
    assert_eq!(state.warmup_remaining_ms, 100);
}

#[test]
fn test_hang_accumulation_saturates() {
    let mut state = WatchdogState::new(0, Some(11));
    state.hang_accumulated_ms = u64::MAX - 5;

    assert_eq!(
        update_watchdog_state(&mut state, Some(11), 10, u64::MAX, 300),
        Some(u64::MAX)
    );
}

#[test]
fn test_monitor_clock_excludes_only_capture_duration() {
    let last_check = Instant::now();
    let monitor_work_started = last_check + Duration::from_millis(150);
    let monitor_work_finished = monitor_work_started + Duration::from_millis(300);
    let next_check = monitor_work_finished + Duration::from_millis(50);

    let adjusted = exclude_monitor_work_from_anr_clock(
        last_check,
        monitor_work_started,
        monitor_work_finished,
    );

    assert_eq!(
        next_check.duration_since(adjusted),
        Duration::from_millis(200)
    );
}
