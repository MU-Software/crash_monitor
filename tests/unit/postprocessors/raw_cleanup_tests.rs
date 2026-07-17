use super::*;
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
fn test_raw_cleanup_deletes_file() {
    let tmp = std::env::temp_dir().join("crash_test_raw_cleanup.bin");
    std::fs::write(&tmp, b"raw data").unwrap();
    assert!(tmp.exists());

    let json_path = PathBuf::from("/tmp/crash_test_report.json");
    let mut result = ReportResult {
        raw_path: Some(tmp.clone()),
        json_path: Some(json_path),
        session: None,
    };

    let event = make_crash_event();
    let rc = RawCleanup;
    rc.process(&event, &mut result).unwrap();

    assert!(!tmp.exists(), "raw file should be deleted after cleanup");
}
