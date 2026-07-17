use crate::filters::RateLimiter;
use crate::pipeline::{CrashEvent, Filter, Plugin, PluginContext, ReportType};
use std::time::{Duration, Instant};

fn dummy_event(report_type: ReportType) -> CrashEvent {
    CrashEvent {
        report_type,
        termination: None,
        exception_type: Some(1),
        exception_code: Some(0),
        exception_subcode: Some(0),
        crashed_thread: None,
        bail_on_suspend_failure: false,
        pid: 1234,
        process_name: "test".into(),
        hang_duration_ms: None,
    }
}

fn check(limiter: &RateLimiter, report_type: ReportType, now: Instant) -> bool {
    limiter
        .check_and_record(report_type, now, &PluginContext::without_deadline())
        .unwrap()
}

#[test]
fn test_first_n_pass_within_window() {
    let limiter = RateLimiter::new(3, Duration::from_secs(60));
    let now = Instant::now();
    assert!(check(&limiter, ReportType::Crash, now));
    assert!(check(&limiter, ReportType::Crash, now));
    assert!(check(&limiter, ReportType::Crash, now));
}

#[test]
fn test_blocks_when_exceeding_limit() {
    let limiter = RateLimiter::new(3, Duration::from_secs(60));
    let now = Instant::now();
    for _ in 0..3 {
        assert!(check(&limiter, ReportType::Crash, now));
    }
    assert!(!check(&limiter, ReportType::Crash, now));
    assert!(!check(&limiter, ReportType::Crash, now));
}

#[test]
fn test_contended_state_fails_open_without_waiting() {
    let limiter = RateLimiter::new(1, Duration::from_secs(60));
    let _guard = limiter.recent.lock().unwrap();

    assert!(
        limiter
            .check_and_record(
                ReportType::Crash,
                Instant::now(),
                &PluginContext::without_deadline(),
            )
            .unwrap()
    );
}

#[test]
fn test_window_expiry_allows_new_events() {
    let window = Duration::from_secs(60);
    let limiter = RateLimiter::new(3, window);
    let base = Instant::now();
    for i in 0..3 {
        assert!(check(
            &limiter,
            ReportType::Crash,
            base + Duration::from_secs(i)
        ));
    }
    // Immediately after fills: blocked
    assert!(!check(
        &limiter,
        ReportType::Crash,
        base + Duration::from_secs(3)
    ));

    // After window passes from the oldest entry: allowed
    assert!(check(
        &limiter,
        ReportType::Crash,
        base + window + Duration::from_secs(1)
    ));
}

#[test]
fn test_report_types_count_independently() {
    let limiter = RateLimiter::new(3, Duration::from_secs(60));
    let now = Instant::now();
    for _ in 0..3 {
        assert!(check(&limiter, ReportType::Crash, now));
    }
    // crash limit exhausted, but snapshot/anr still have their own quota
    assert!(!check(&limiter, ReportType::Crash, now));
    assert!(check(&limiter, ReportType::Snapshot, now));
    assert!(check(&limiter, ReportType::Snapshot, now));
    assert!(check(&limiter, ReportType::Snapshot, now));
    assert!(!check(&limiter, ReportType::Snapshot, now));
    assert!(check(&limiter, ReportType::Anr, now));
}

#[test]
fn test_partial_window_expiry() {
    let window = Duration::from_secs(60);
    let limiter = RateLimiter::new(3, window);
    let base = Instant::now();
    assert!(check(&limiter, ReportType::Crash, base));
    assert!(check(
        &limiter,
        ReportType::Crash,
        base + Duration::from_secs(10)
    ));
    assert!(check(
        &limiter,
        ReportType::Crash,
        base + Duration::from_secs(20)
    ));
    assert!(!check(
        &limiter,
        ReportType::Crash,
        base + Duration::from_secs(30)
    ));

    // 65s after base: first entry (base) expires, leaving 2 valid entries.
    // One more should fit.
    let t = base + Duration::from_secs(65);
    assert!(check(&limiter, ReportType::Crash, t));
    // Now we have entries at base+10, base+20, base+65 → full again.
    assert!(!check(&limiter, ReportType::Crash, t));
}

#[test]
fn test_should_process_uses_real_time() {
    let limiter = RateLimiter::new(2, Duration::from_secs(60));
    let event = dummy_event(ReportType::Crash);
    assert!(
        limiter
            .should_process(&event, &PluginContext::without_deadline())
            .unwrap()
    );
    assert!(
        limiter
            .should_process(&event, &PluginContext::without_deadline())
            .unwrap()
    );
    assert!(
        !limiter
            .should_process(&event, &PluginContext::without_deadline())
            .unwrap()
    );
}

#[test]
fn test_plugin_metadata() {
    let limiter = RateLimiter::new(3, Duration::from_secs(60));
    assert_eq!(limiter.name(), "RateLimiter");
    assert!(limiter.is_available());
}
