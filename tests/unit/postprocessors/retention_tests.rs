use crate::pipeline::{CrashEvent, PostProcessor, ReportResult, ReportType};
use crate::postprocessors::RetentionManager;
use std::fs;

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
fn test_noop_under_limits() {
    let dir = tempfile::tempdir().unwrap();
    let pending = dir.path().to_path_buf();

    for i in 0..3 {
        fs::write(pending.join(format!("report_{i}.json")), "{}").unwrap();
    }

    let manager = RetentionManager::with_dir(64, 256, 15, pending.clone());
    manager
        .process(&dummy_event(), &mut dummy_result())
        .unwrap();

    let count = fs::read_dir(&pending).unwrap().count();
    assert_eq!(count, 3, "all files should remain");
}

#[test]
fn test_deletes_oldest_by_count() {
    let dir = tempfile::tempdir().unwrap();
    let pending = dir.path().to_path_buf();

    // Use recent timestamps so age policy (365 days) doesn't delete them
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    for i in 0..10 {
        let path = pending.join(format!("report_{i:02}.json"));
        fs::write(&path, "{}").unwrap();
        let _ = filetime::set_file_mtime(
            &path,
            filetime::FileTime::from_unix_time(now - 100 + i64::from(i), 0),
        );
    }

    let manager = RetentionManager::with_dir(5, 256, 365, pending.clone());
    manager
        .process(&dummy_event(), &mut dummy_result())
        .unwrap();

    let remaining: Vec<String> = fs::read_dir(&pending)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(remaining.len(), 5, "should keep only 5 reports");
}

#[test]
fn test_deletes_by_size() {
    let dir = tempfile::tempdir().unwrap();
    let pending = dir.path().to_path_buf();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    for i in 0..10 {
        let path = pending.join(format!("report_{i:02}.json"));
        fs::write(&path, "x".repeat(1024)).unwrap();
        let _ = filetime::set_file_mtime(
            &path,
            filetime::FileTime::from_unix_time(now - 100 + i64::from(i), 0),
        );
    }

    // max_total_bytes = 0 → delete all
    let manager = RetentionManager::with_dir(100, 0, 365, pending.clone()); // 0 MB = delete all
    manager
        .process(&dummy_event(), &mut dummy_result())
        .unwrap();

    let count = fs::read_dir(&pending).unwrap().count();
    assert_eq!(count, 0, "all files should be deleted when size limit is 0");
}

#[test]
fn test_deletes_by_age() {
    let dir = tempfile::tempdir().unwrap();
    let pending = dir.path().to_path_buf();

    // Create files with old timestamps (30 days ago)
    let old_time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        - 30 * 86400;
    for i in 0..5 {
        let path = pending.join(format!("report_{i:02}.json"));
        fs::write(&path, "{}").unwrap();
        let _ = filetime::set_file_mtime(
            &path,
            filetime::FileTime::from_unix_time(old_time + i64::from(i), 0),
        );
    }

    // max_age = 15 days → all 30-day-old files should be deleted
    let manager = RetentionManager::with_dir(100, 256, 15, pending.clone());
    manager
        .process(&dummy_event(), &mut dummy_result())
        .unwrap();

    let count = fs::read_dir(&pending).unwrap().count();
    assert_eq!(count, 0, "all old files should be deleted");
}

#[test]
fn test_empty_dir_noop() {
    let dir = tempfile::tempdir().unwrap();
    let manager = RetentionManager::with_dir(64, 256, 15, dir.path().to_path_buf());
    assert!(manager.process(&dummy_event(), &mut dummy_result()).is_ok());
}
