use crate::filters::RateLimiter;
use crate::pipeline::{CrashEvent, Filter, Plugin, PluginContext, ReportType};
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

fn dummy_event(report_type: ReportType) -> CrashEvent {
    CrashEvent {
        report_id: crate::pipeline::ReportId::default(),
        report_type,
        termination: None,
        exception_type: Some(1),
        exception_code: Some(0),
        exception_subcode: Some(0),
        exception_codes: Vec::new(),
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
fn test_persistent_state_survives_monitor_restart_and_stays_bounded() {
    let directory = tempfile::tempdir().unwrap();
    let state_path = directory.path().join("rate-limit.json");
    let event = dummy_event(ReportType::Crash);
    let context = PluginContext::without_deadline();

    let first = RateLimiter::with_state_path(2, Duration::from_secs(60), state_path.clone());
    assert!(first.should_process(&event, &context).unwrap());
    assert!(first.should_process(&event, &context).unwrap());
    drop(first);

    let restarted = RateLimiter::with_state_path(2, Duration::from_secs(60), state_path.clone());
    assert!(!restarted.should_process(&event, &context).unwrap());
    assert!(std::fs::metadata(state_path).unwrap().len() <= 128 * 1024);
}

#[test]
fn test_separate_limiters_reload_shared_state_before_each_decision() {
    let directory = tempfile::tempdir().unwrap();
    let state_path = directory.path().join("rate-limit.json");
    let event = dummy_event(ReportType::Crash);
    let context = PluginContext::without_deadline();
    let first = RateLimiter::with_state_path(1, Duration::from_secs(60), state_path.clone());
    let second = RateLimiter::with_state_path(1, Duration::from_secs(60), state_path);

    assert!(first.should_process(&event, &context).unwrap());
    assert!(!second.should_process(&event, &context).unwrap());
}

#[test]
fn test_concurrent_limiters_atomically_share_one_quota() {
    let directory = tempfile::tempdir().unwrap();
    let state_path = directory.path().join("rate-limit.json");
    let barrier = Arc::new(Barrier::new(2));

    let handles = (0..2)
        .map(|_| {
            let limiter =
                RateLimiter::with_state_path(1, Duration::from_secs(60), state_path.clone());
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                limiter
                    .should_process(
                        &dummy_event(ReportType::Crash),
                        &PluginContext::without_deadline(),
                    )
                    .unwrap()
            })
        })
        .collect::<Vec<_>>();
    let accepted = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .filter(|accepted| *accepted)
        .count();

    assert_eq!(accepted, 1);
}

#[test]
fn test_corrupt_persistent_state_is_quarantined_and_replaced() {
    let directory = tempfile::tempdir().unwrap();
    let state_path = directory.path().join("rate-limit.json");
    std::fs::write(&state_path, b"not-json").unwrap();
    std::fs::set_permissions(&state_path, std::fs::Permissions::from_mode(0o600)).unwrap();

    let event = dummy_event(ReportType::Crash);
    let limiter = RateLimiter::with_state_path(1, Duration::from_secs(60), state_path.clone());
    let error = limiter
        .should_process(&event, &PluginContext::without_deadline())
        .unwrap_err();
    assert!(error.contains("quarantined and reset"));

    let replacement: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
    assert_eq!(replacement["version"], 1);
    assert!(std::fs::read_dir(directory.path()).unwrap().any(|entry| {
        entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".rate-limit-state.corrupt.")
    }));

    let restarted = RateLimiter::with_state_path(1, Duration::from_secs(60), state_path);
    assert!(
        !restarted
            .should_process(&event, &PluginContext::without_deadline())
            .unwrap()
    );
}

#[test]
fn test_plugin_metadata() {
    let limiter = RateLimiter::new(3, Duration::from_secs(60));
    assert_eq!(limiter.name(), "RateLimiter");
    assert!(limiter.is_available());
    assert!(limiter.hard_dependencies().is_empty());
    assert_eq!(limiter.order_after(), &["DiskSpaceFilter"]);
}
