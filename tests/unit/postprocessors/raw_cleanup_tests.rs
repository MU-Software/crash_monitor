use super::*;
use crate::pipeline::{
    ArtifactKind, ArtifactTransaction, CrashEvent, PluginContext, ReportContext, ReportResult,
    ReportType,
};

fn make_crash_event() -> CrashEvent {
    CrashEvent {
        report_id: crate::pipeline::ReportId::default(),
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
fn test_raw_cleanup_deletes_file() {
    let dir = tempfile::tempdir().unwrap();
    let tmp = dir.path().join("crash_test_raw_cleanup.bin");
    std::fs::write(&tmp, b"raw data").unwrap();
    assert!(tmp.exists());

    let json_path = dir.path().join("crash_test_report.json");
    let mut result = ReportResult {
        artifact_paths: vec![tmp.clone()],
        raw_path: Some(tmp.clone()),
        json_path: Some(json_path),
        session: None,
    };

    let event = make_crash_event();
    let rc = RawCleanup;
    rc.process(&event, &mut result, &PluginContext::without_deadline())
        .unwrap();

    assert!(!tmp.exists(), "raw file should be deleted after cleanup");
    assert!(
        result.raw_path.is_none(),
        "ReportResult must not retain a path deleted by RawCleanup"
    );
}

#[test]
fn test_raw_cleanup_observes_expired_deadline_before_deleting() {
    let dir = tempfile::tempdir().unwrap();
    let raw_path = dir.path().join("report_raw.bin");
    std::fs::write(&raw_path, b"raw data").unwrap();
    let mut result = ReportResult {
        artifact_paths: vec![raw_path.clone()],
        raw_path: Some(raw_path.clone()),
        json_path: Some(dir.path().join("report.json")),
        session: None,
    };

    let error = RawCleanup
        .process(
            &make_crash_event(),
            &mut result,
            &PluginContext::with_timeout(std::time::Duration::ZERO),
        )
        .unwrap_err();

    assert_eq!(error, "plugin deadline reached");
    assert!(raw_path.exists());
    assert_eq!(result.raw_path.as_deref(), Some(raw_path.as_path()));
}

#[test]
fn cleanup_removes_every_sensitive_raw_kind_but_preserves_report_artifacts() {
    let root = tempfile::tempdir().unwrap();
    let event = make_crash_event();
    let transaction = ArtifactTransaction::begin(ReportContext::new(&event, root.path())).unwrap();
    let report = transaction
        .write_bytes("report.json", ArtifactKind::Report, b"{}")
        .unwrap();
    let thread = transaction
        .write_bytes("threads.txt", ArtifactKind::ThreadRaw, b"thread")
        .unwrap();
    let breadcrumbs = transaction
        .write_bytes("breadcrumbs.bin", ArtifactKind::BreadcrumbsRaw, b"crumbs")
        .unwrap();
    let context_raw = transaction
        .write_bytes("context.bin", ArtifactKind::ContextRaw, b"context")
        .unwrap();
    let mut result = ReportResult {
        artifact_paths: transaction.artifact_paths(),
        raw_path: Some(thread.clone()),
        json_path: Some(report.clone()),
        session: None,
    };
    let plugin_context =
        PluginContext::without_deadline().with_artifact_transaction(transaction.clone());

    RawCleanup
        .process(&event, &mut result, &plugin_context)
        .unwrap();

    assert!(report.exists());
    assert!(!thread.exists());
    assert!(!breadcrumbs.exists());
    assert!(!context_raw.exists());
    assert_eq!(transaction.artifact_paths(), vec![report]);
    assert!(result.raw_path.is_none());
}
