use super::*;

fn make_crash_event(report_type: ReportType) -> CrashEvent {
    CrashEvent {
        report_type,
        termination: None,
        exception_type: None,
        exception_code: None,
        exception_subcode: None,
        crashed_thread: None,
        bail_on_suspend_failure: false,
        pid: 1234,
        process_name: "test".to_string(),
        hang_duration_ms: None,
    }
}

#[test]
fn test_crash_event_is_crash() {
    assert!(make_crash_event(ReportType::Crash).is_crash());
    assert!(!make_crash_event(ReportType::Snapshot).is_crash());
    assert!(!make_crash_event(ReportType::Anr).is_crash());
    assert!(!make_crash_event(ReportType::Oom).is_crash());
    assert!(!make_crash_event(ReportType::ExitFailure).is_crash());
    assert!(!make_crash_event(ReportType::SignalFailure).is_crash());
}

#[test]
fn test_report_type_as_str_roundtrip() {
    assert_eq!(ReportType::Crash.as_str(), "crash");
    assert_eq!(ReportType::Snapshot.as_str(), "snapshot");
    assert_eq!(ReportType::Anr.as_str(), "anr");
    assert_eq!(ReportType::Oom.as_str(), "oom");
    assert_eq!(ReportType::ExitFailure.as_str(), "exit_failure");
    assert_eq!(ReportType::SignalFailure.as_str(), "signal_failure");
}

#[test]
fn test_report_type_serde_roundtrip() {
    for t in [
        ReportType::Crash,
        ReportType::Snapshot,
        ReportType::Anr,
        ReportType::Oom,
        ReportType::ExitFailure,
        ReportType::SignalFailure,
    ] {
        let json = serde_json::to_string(&t).unwrap();
        let back: ReportType = serde_json::from_str(&json).unwrap();
        assert_eq!(t, back);
        assert_eq!(json.trim_matches('"'), t.as_str());
    }
}

#[test]
fn test_termination_reason_serde_roundtrip() {
    let cases = [
        (
            TerminationReason::Exited {
                exit_code: 42,
                runtime_ms: 1_250,
            },
            serde_json::json!({
                "kind": "exited",
                "exit_code": 42,
                "runtime_ms": 1_250
            }),
        ),
        (
            TerminationReason::Signaled {
                signal: 11,
                core_dumped: true,
                runtime_ms: 987,
            },
            serde_json::json!({
                "kind": "signaled",
                "signal": 11,
                "core_dumped": true,
                "runtime_ms": 987
            }),
        ),
    ];

    for (reason, expected) in cases {
        let value = serde_json::to_value(reason).unwrap();
        assert_eq!(value, expected);
        let decoded: TerminationReason = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, reason);
    }
}

#[test]
fn test_diagnostics_succeeded() {
    let mut diag = Diagnostics::new();
    diag.record("test", PluginStatus::Ok, Duration::ZERO);
    assert!(diag.succeeded("test"));
}

#[test]
fn test_diagnostics_not_succeeded() {
    let mut diag = Diagnostics::new();
    diag.record(
        "test",
        PluginStatus::Error("fail".to_string()),
        Duration::ZERO,
    );
    assert!(!diag.succeeded("test"));
}

#[test]
fn test_diagnostics_records_timeout_separately_from_error() {
    let mut diagnostics = Diagnostics::new();
    diagnostics.record("slow", PluginStatus::TimedOut, Duration::from_millis(25));

    let entry = diagnostics.plugins.first().expect("timeout diagnostic");
    assert!(matches!(entry.status, PluginStatus::TimedOut));
    assert_eq!(entry.duration_ms, 25);
    assert!(!diagnostics.succeeded("slow"));
}

#[test]
fn test_diagnostics_pipeline_duration() {
    let diag = Diagnostics::new();
    std::thread::sleep(Duration::from_millis(1));
    assert!(diag.pipeline_duration_ms() > 0);
}

#[test]
fn captured_event_is_an_owned_worker_handoff() {
    fn assert_send_static<T: Send + 'static>() {}
    assert_send_static::<CapturedEvent>();
}
