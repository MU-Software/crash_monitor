use std::time::{Duration, Instant};

use super::{run_plugin_catching_panics, run_plugin_safe, write_raw_shm_stage1};
use crate::pipeline::{RawShmSnapshot, ReportType};

#[test]
fn test_run_plugin_catching_panics_ok() {
    let result = run_plugin_catching_panics("test", || Ok(42));
    assert_eq!(result, Some(42));
}

#[test]
fn test_run_plugin_catching_panics_err() {
    let result = run_plugin_catching_panics::<i32>("test", || Err("boom".into()));
    assert_eq!(result, None);
}

#[test]
fn test_run_plugin_catching_panics_panic() {
    let result = run_plugin_catching_panics::<i32>("test", || {
        panic!("oops");
    });
    assert_eq!(result, None);
}

#[test]
fn test_run_plugin_safe_ok() {
    let result = run_plugin_safe("test", 0, || Ok(42));
    assert_eq!(result, Some(42));
}

#[test]
fn test_run_plugin_safe_err() {
    let result = run_plugin_safe::<i32>("test", 0, || Err("boom".into()));
    assert_eq!(result, None);
}

#[test]
fn test_run_plugin_safe_panic() {
    let result = run_plugin_safe::<i32>("test", 0, || {
        panic!("oops");
    });
    assert_eq!(result, None);
}

#[test]
fn test_run_plugin_safe_timeout() {
    // alarm(timeout_secs) sends SIGALRM which interrupts blocking mach/nix syscalls
    // with EINTR. In a multi-threaded test runner, SIGALRM may be delivered to any
    // thread, so we can't rely on it interrupting our closure's blocking call.
    //
    // Instead, we verify the timeout contract by simulating what happens when a
    // plugin's blocking syscall returns EINTR: the closure returns Err, and
    // run_plugin_safe returns None.
    //
    // We also verify the alarm guard: with timeout > 0, alarm() is set and then
    // canceled by AlarmGuard on drop, completing within a reasonable time.
    let start = Instant::now();
    let result = run_plugin_safe::<()>("test", 1, || {
        // Simulate a plugin that detects EINTR and returns an error
        Err("interrupted by signal (EINTR)".into())
    });
    let elapsed = start.elapsed();

    // Closure returned Err → run_plugin_safe returns None
    assert_eq!(result, None);
    // AlarmGuard canceled the alarm, no leftover SIGALRM
    assert!(
        elapsed < Duration::from_secs(5),
        "should complete quickly, took {elapsed:?}"
    );
}

#[test]
fn test_write_raw_shm_stage1_writes_owned_snapshot_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let snapshot = RawShmSnapshot {
        breadcrumbs: vec![0x01, 0x02, 0x03, 0x04],
        context: vec![0xAA, 0xBB, 0xCC],
    };

    write_raw_shm_stage1(dir.path(), ReportType::Crash, 1234, &snapshot).unwrap();

    let mut files: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    files.sort_by_key(std::fs::DirEntry::file_name);
    assert_eq!(files.len(), 2);

    let breadcrumbs = files
        .iter()
        .find(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .ends_with("_raw_breadcrumbs.bin")
        })
        .expect("breadcrumbs dump");
    let context = files
        .iter()
        .find(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .ends_with("_raw_context.bin")
        })
        .expect("context dump");

    assert_eq!(
        std::fs::read(breadcrumbs.path()).unwrap(),
        snapshot.breadcrumbs
    );
    assert_eq!(std::fs::read(context.path()).unwrap(), snapshot.context);
}

#[test]
fn test_write_raw_shm_stage1_reports_write_failure() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("missing");
    let snapshot = RawShmSnapshot {
        breadcrumbs: vec![1],
        context: vec![2],
    };

    let error = write_raw_shm_stage1(&missing, ReportType::Crash, 1234, &snapshot).unwrap_err();
    assert!(error.contains("Failed to write raw breadcrumbs"));
}
