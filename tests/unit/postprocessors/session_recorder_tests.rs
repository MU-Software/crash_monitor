use super::*;
use crate::pipeline::report::SessionReport;
use crate::pipeline::{CrashEvent, ReportResult, ReportType};
use std::path::PathBuf;

fn make_crash_event() -> CrashEvent {
    CrashEvent {
        report_type: ReportType::Crash,
        exception_type: None,
        exception_code: None,
        exception_subcode: None,
        crashed_thread: None,
        bail_on_suspend_failure: false,
        pid: 9999,
        process_name: "test_app".to_string(),
        hang_duration_ms: None,
    }
}

#[test]
fn test_session_recorder_crash_only() {
    let session = SessionReport {
        id: "test-uuid-1234".to_string(),
        start: "2026-03-29T10:00:00+09:00".to_string(),
        duration_s: 120,
    };

    let mut result = ReportResult {
        raw_path: None,
        json_path: Some(PathBuf::from("/tmp/crash_20260329_100200_9999.json")),
        session: Some(session),
    };

    let event = make_crash_event();
    let recorder = SessionRecorder;
    // This will attempt to write sessions.jsonl under ~/.modelblockbuilder/
    // which should succeed in a real environment. We just verify no panic.
    let res = recorder.process(&event, &mut result);
    assert!(res.is_ok());
}
