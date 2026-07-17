use super::*;

fn make_crash_event(report_type: ReportType) -> CrashEvent {
    CrashEvent {
        report_type,
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
}

#[test]
fn test_report_type_as_str_roundtrip() {
    assert_eq!(ReportType::Crash.as_str(), "crash");
    assert_eq!(ReportType::Snapshot.as_str(), "snapshot");
    assert_eq!(ReportType::Anr.as_str(), "anr");
    assert_eq!(ReportType::Oom.as_str(), "oom");
}

#[test]
fn test_report_type_serde_roundtrip() {
    for t in [
        ReportType::Crash,
        ReportType::Snapshot,
        ReportType::Anr,
        ReportType::Oom,
    ] {
        let json = serde_json::to_string(&t).unwrap();
        let back: ReportType = serde_json::from_str(&json).unwrap();
        assert_eq!(t, back);
        assert_eq!(json.trim_matches('"'), t.as_str());
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
fn test_diagnostics_pipeline_duration() {
    let diag = Diagnostics::new();
    std::thread::sleep(Duration::from_millis(1));
    assert!(diag.pipeline_duration_ms() > 0);
}
