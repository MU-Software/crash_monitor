use crate::pipeline::{CrashEvent, PostProcessor, ReportResult, ReportType};
use crate::postprocessors::LogRotator;

fn dummy_event() -> CrashEvent {
    CrashEvent {
        report_type: ReportType::Crash,
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

fn dummy_result() -> ReportResult {
    ReportResult {
        raw_path: None,
        json_path: None,
        session: None,
    }
}

#[test]
fn test_noop_when_small() {
    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("sessions.jsonl");
    std::fs::write(&log_path, "line1\nline2\nline3\n").unwrap();

    // 1MB threshold — file is tiny, should be unchanged
    let rotator = LogRotator::with_path(1, log_path.clone());
    rotator
        .process(&dummy_event(), &mut dummy_result())
        .unwrap();

    let content = std::fs::read_to_string(&log_path).unwrap();
    assert_eq!(content, "line1\nline2\nline3\n");
}

#[test]
fn test_truncates_when_large() {
    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("sessions.jsonl");

    // Write 100 lines
    let mut content = String::new();
    for i in 0..100 {
        content.push_str(&format!("line_{i}\n"));
    }
    std::fs::write(&log_path, &content).unwrap();

    // Threshold = 0 bytes → always rotate
    let rotator = LogRotator::with_path(0, log_path.clone());
    rotator
        .process(&dummy_event(), &mut dummy_result())
        .unwrap();

    let after = std::fs::read_to_string(&log_path).unwrap();
    let lines_after: Vec<&str> = after.lines().collect();
    assert_eq!(lines_after.len(), 50, "should keep 50% of lines");
}

#[test]
fn test_preserves_recent_entries() {
    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("sessions.jsonl");

    let mut content = String::new();
    for i in 0..10 {
        content.push_str(&format!("line_{i}\n"));
    }
    std::fs::write(&log_path, &content).unwrap();

    let rotator = LogRotator::with_path(0, log_path.clone());
    rotator
        .process(&dummy_event(), &mut dummy_result())
        .unwrap();

    let after = std::fs::read_to_string(&log_path).unwrap();
    let lines: Vec<&str> = after.lines().collect();
    assert_eq!(lines[0], "line_5");
    assert_eq!(lines[4], "line_9");
}

#[test]
fn test_missing_file_noop() {
    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("sessions.jsonl");
    // Don't create the file

    let rotator = LogRotator::with_path(1, log_path);
    assert!(rotator.process(&dummy_event(), &mut dummy_result()).is_ok());
}
