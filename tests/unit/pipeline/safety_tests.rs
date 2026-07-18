use std::time::{Duration, Instant};

use super::{
    PluginContext, PluginRunResult, run_plugin_catching_panics, run_plugin_cooperative,
    write_raw_shm_stage1,
};
use crate::pipeline::{ArtifactTransaction, CrashEvent, RawShmSnapshot, ReportContext, ReportType};

fn raw_transaction(root: &std::path::Path) -> std::sync::Arc<ArtifactTransaction> {
    let event = CrashEvent {
        report_id: crate::pipeline::ReportId::default(),
        report_type: ReportType::Crash,
        termination: None,
        exception_type: None,
        exception_code: None,
        exception_subcode: None,
        exception_codes: Vec::new(),
        crashed_thread: None,
        bail_on_suspend_failure: false,
        pid: 1234,
        process_name: "fixture".into(),
        hang_duration_ms: None,
    };
    ArtifactTransaction::begin(ReportContext::new(&event, root)).unwrap()
}

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
fn test_run_plugin_cooperative_ok() {
    let context = PluginContext::without_deadline();
    let result = run_plugin_cooperative("test", &context, |_| Ok(42));
    assert!(matches!(result, PluginRunResult::Completed(42)));
}

#[test]
fn test_run_plugin_cooperative_error_is_typed() {
    let context = PluginContext::without_deadline();
    let result = run_plugin_cooperative::<i32>("test", &context, |_| Err("boom".into()));
    assert!(matches!(result, PluginRunResult::Failed(error) if error == "boom"));
}

#[test]
fn test_run_plugin_cooperative_panic_is_typed() {
    let context = PluginContext::without_deadline();
    let result = run_plugin_cooperative::<i32>("test", &context, |_| {
        panic!("oops");
    });
    assert!(matches!(result, PluginRunResult::Panicked(message) if message == "oops"));
}

#[test]
fn test_run_plugin_cooperative_deadline_is_typed() {
    let context = PluginContext::with_timeout(Duration::from_millis(10));
    let start = Instant::now();
    let result = run_plugin_cooperative::<()>("test", &context, |context| {
        loop {
            context.checkpoint()?;
            std::hint::spin_loop();
        }
    });
    let elapsed = start.elapsed();

    assert!(matches!(result, PluginRunResult::TimedOut));
    assert!(
        elapsed < Duration::from_secs(1),
        "cooperative timeout should complete quickly, took {elapsed:?}"
    );
}

#[test]
fn test_cancellation_token_stops_before_invocation() {
    let context = PluginContext::without_deadline();
    context.cancellation_token().cancel();
    let result = run_plugin_cooperative("cancelled", &context, |_| Ok(42));
    assert!(matches!(result, PluginRunResult::TimedOut));
}

#[test]
fn test_subprocess_cleanup_failure_is_not_relabelled_as_timeout() {
    let context = PluginContext::without_deadline();
    let result = run_plugin_cooperative::<()>("cleanup-failure", &context, |context| {
        context.mark_subprocess_started();
        context.record_subprocess_cleanup_failure("direct child was not reaped".to_string());
        context.cancellation_token().cancel();
        Err("adapter observed timeout".to_string())
    });

    assert!(matches!(
        result,
        PluginRunResult::Failed(error) if error == "direct child was not reaped"
    ));
}

#[test]
fn test_write_raw_shm_stage1_writes_owned_snapshot_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let snapshot = RawShmSnapshot {
        breadcrumbs: vec![0x01, 0x02, 0x03, 0x04],
        context: vec![0xAA, 0xBB, 0xCC],
    };
    let transaction = raw_transaction(dir.path());

    write_raw_shm_stage1(&transaction, &snapshot).unwrap();

    let mut files: Vec<_> = std::fs::read_dir(transaction.staging_dir())
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    files.sort_by_key(std::fs::DirEntry::file_name);
    assert_eq!(files.len(), 2);

    let breadcrumbs = files
        .iter()
        .find(|entry| entry.file_name().to_string_lossy() == "breadcrumbs.bin")
        .expect("breadcrumbs dump");
    let context = files
        .iter()
        .find(|entry| entry.file_name().to_string_lossy() == "context.bin")
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
    let snapshot = RawShmSnapshot {
        breadcrumbs: vec![1],
        context: vec![2],
    };
    let transaction = raw_transaction(dir.path());
    std::fs::remove_dir(transaction.staging_dir()).unwrap();

    let error = write_raw_shm_stage1(&transaction, &snapshot).unwrap_err();
    assert!(error.contains("Failed to write raw breadcrumbs"));
}
