use super::*;
use crate::pipeline::report::SessionReport;
use crate::pipeline::{CrashEvent, PluginContext, ReportResult, ReportType};
use nix::sys::stat::Mode;
use nix::unistd::mkfifo;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn make_crash_event() -> CrashEvent {
    CrashEvent {
        report_type: ReportType::Crash,
        termination: None,
        exception_type: None,
        exception_code: None,
        exception_subcode: None,
        exception_codes: Vec::new(),
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
    let res = recorder.process(&event, &mut result, &PluginContext::without_deadline());
    assert!(res.is_ok());
}

#[test]
fn test_append_session_record_accepts_regular_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sessions.jsonl");

    append_session_record(
        &path,
        r#"{"id":"session"}"#,
        &PluginContext::without_deadline(),
    )
    .unwrap();

    assert_eq!(
        std::fs::read_to_string(path).unwrap(),
        "{\"id\":\"session\"}\n"
    );
}

#[test]
fn test_append_session_record_rejects_fifo_without_blocking() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sessions.jsonl");
    mkfifo(&path, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
    let started = Instant::now();

    let error = append_session_record(
        &path,
        r#"{"id":"session"}"#,
        &PluginContext::without_deadline(),
    )
    .unwrap_err();

    assert!(error.contains("cannot open") || error.contains("not a regular file"));
    assert!(started.elapsed() < Duration::from_secs(1));
}
