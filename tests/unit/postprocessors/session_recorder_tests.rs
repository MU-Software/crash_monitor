use super::*;
use crate::pipeline::report::SessionReport;
use crate::pipeline::{CommittedReport, CrashEvent, PluginContext, ReportResult, ReportType};
use nix::sys::stat::Mode;
use nix::unistd::mkfifo;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn make_crash_event() -> CrashEvent {
    CrashEvent {
        report_id: Default::default(),
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

fn committed(event: &CrashEvent, report_dir: &std::path::Path) -> CommittedReport {
    CommittedReport {
        report_id: event.report_id.clone(),
        report_dir: report_dir.to_path_buf(),
        manifest_path: report_dir.join("manifest.json"),
        durability_warnings: Vec::new(),
    }
}

#[test]
fn test_session_record_contains_committed_identity_and_paths() {
    let dir = tempfile::tempdir().unwrap();
    let session = SessionReport {
        id: "test-uuid-1234".to_string(),
        start: "2026-03-29T10:00:00+09:00".to_string(),
        duration_s: 120,
    };

    let event = make_crash_event();
    let report_dir = dir.path().join(event.report_id.as_str());
    let committed = committed(&event, &report_dir);
    let report_context = Arc::new(crate::pipeline::ReportContext::new(&event, dir.path()));
    let context = PluginContext::without_deadline().with_report_context(report_context);

    std::fs::write(dir.path().join("session.lock"), b"locked").unwrap();
    record_crash_in_dir(
        &session,
        &committed,
        Some("report.zip"),
        dir.path(),
        &context,
    )
    .unwrap();

    let line = std::fs::read_to_string(dir.path().join("sessions.jsonl")).unwrap();
    let record: serde_json::Value = serde_json::from_str(line.trim()).unwrap();
    assert_eq!(record["report_id"], event.report_id.as_str());
    assert_eq!(record["report"], "report.zip");
    assert_eq!(
        record["manifest"],
        committed.manifest_path.to_string_lossy().as_ref()
    );
    assert!(!dir.path().join("session.lock").exists());
}

#[test]
fn test_legacy_session_record_without_report_id_still_deserializes() {
    let json = r#"{
        "id":"legacy-session",
        "start":"2026-03-29T10:00:00+09:00",
        "end":"2026-03-29T10:02:00+09:00",
        "status":"crash",
        "duration_s":120,
        "report":"crash_20260329_100200_9999.json"
    }"#;

    let record: SessionRecord = serde_json::from_str(json).unwrap();
    assert_eq!(record.report_id, None);
    assert_eq!(record.manifest, None);
    assert_eq!(
        record.report.as_deref(),
        Some("crash_20260329_100200_9999.json")
    );
}

#[test]
fn session_recorder_requires_commit_before_any_side_effect() {
    let session = SessionReport {
        id: "test-uuid-1234".to_string(),
        start: "2026-03-29T10:00:00+09:00".to_string(),
        duration_s: 120,
    };
    let mut result = ReportResult {
        artifact_paths: vec![PathBuf::from("report.json")],
        raw_path: None,
        json_path: Some(PathBuf::from("report.json")),
        session: Some(session),
    };

    let error = SessionRecorder
        .process(
            &make_crash_event(),
            &mut result,
            &PluginContext::without_deadline(),
        )
        .unwrap_err();
    assert!(error.contains("requires a committed report"));
    assert_eq!(SessionRecorder.phase(), PostProcessorPhase::AfterCommit);
}

#[test]
fn session_recorder_selects_the_canonical_report_from_mixed_artifacts() {
    let dir = tempfile::tempdir().unwrap();
    let event = make_crash_event();
    let transaction = crate::pipeline::ArtifactTransaction::begin(
        crate::pipeline::ReportContext::new(&event, dir.path()),
    )
    .unwrap();
    transaction
        .write_bytes(
            "attachment.log",
            crate::pipeline::ArtifactKind::Attachment,
            b"log",
        )
        .unwrap();
    transaction
        .write_bytes("report.json", crate::pipeline::ArtifactKind::Report, b"{}")
        .unwrap();
    let committed = transaction.commit().unwrap();
    let context = PluginContext::without_deadline().with_artifact_transaction(transaction.clone());
    let session = SessionReport {
        id: "test-uuid-1234".to_string(),
        start: "2026-03-29T10:00:00+09:00".to_string(),
        duration_s: 120,
    };
    let mut result = ReportResult {
        artifact_paths: vec![
            committed.report_dir.join("attachment.log"),
            committed.report_dir.join("report.json"),
        ],
        raw_path: None,
        json_path: None,
        session: Some(session),
    };

    with_test_data_dir(dir.path(), || {
        SessionRecorder
            .process(&event, &mut result, &context)
            .unwrap();
    });

    let sessions_path = dir.path().join("sessions.jsonl");
    let records = std::fs::read_to_string(&sessions_path).unwrap();
    let record: serde_json::Value = serde_json::from_str(records.lines().last().unwrap()).unwrap();
    assert_eq!(record["report"], "report.json");
    transaction.release_publication_lease();
}

#[test]
fn failed_session_append_keeps_session_lock() {
    let dir = tempfile::tempdir().unwrap();
    let sessions_path = dir.path().join("sessions.jsonl");
    let lock_path = dir.path().join("session.lock");
    mkfifo(&sessions_path, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
    std::fs::write(&lock_path, b"locked").unwrap();
    let event = make_crash_event();
    let committed = committed(&event, &dir.path().join(event.report_id.as_str()));
    let session = SessionReport {
        id: "test-uuid-1234".into(),
        start: "2026-03-29T10:00:00+09:00".into(),
        duration_s: 120,
    };

    let error = record_crash_in_dir(
        &session,
        &committed,
        Some("report.json"),
        dir.path(),
        &PluginContext::without_deadline(),
    )
    .unwrap_err();

    assert!(error.contains("cannot open") || error.contains("not a regular file"));
    assert!(lock_path.exists());
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
