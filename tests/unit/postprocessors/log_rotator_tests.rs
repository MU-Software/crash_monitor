use crate::pipeline::{
    CrashEvent, Plugin, PluginContext, PostProcessor, PostProcessorPhase, ReportResult, ReportType,
};
use crate::postprocessors::LogRotator;
use std::fmt::Write as _;

fn dummy_event() -> CrashEvent {
    CrashEvent {
        report_id: crate::pipeline::ReportId::default(),
        report_type: ReportType::Crash,
        termination: None,
        exception_type: Some(1),
        exception_code: Some(0),
        exception_subcode: Some(0),
        exception_codes: Vec::new(),
        crashed_thread: None,
        bail_on_suspend_failure: false,
        pid: 1234,
        process_name: "test".into(),
        hang_duration_ms: None,
    }
}

fn dummy_result() -> ReportResult {
    ReportResult {
        artifact_paths: Vec::new(),
        raw_path: None,
        json_path: None,
        session: None,
    }
}

#[test]
fn test_plugin_dependency_metadata() {
    let rotator = LogRotator::new(1);
    assert!(rotator.hard_dependencies().is_empty());
    assert_eq!(rotator.order_after(), &["SessionRecorder"]);
    assert_eq!(rotator.phase(), PostProcessorPhase::AfterCommit);
}

#[test]
fn test_noop_when_small() {
    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("sessions.jsonl");
    std::fs::write(&log_path, "line1\nline2\nline3\n").unwrap();

    // 1MB threshold — file is tiny, should be unchanged
    let rotator = LogRotator::with_path(1, log_path.clone());
    rotator
        .process(
            &dummy_event(),
            &mut dummy_result(),
            &PluginContext::without_deadline(),
        )
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
        writeln!(content, "line_{i}").unwrap();
    }
    std::fs::write(&log_path, &content).unwrap();

    // Threshold = 0 bytes → always rotate
    let rotator = LogRotator::with_path(0, log_path.clone());
    rotator
        .process(
            &dummy_event(),
            &mut dummy_result(),
            &PluginContext::without_deadline(),
        )
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
        writeln!(content, "line_{i}").unwrap();
    }
    std::fs::write(&log_path, &content).unwrap();

    let rotator = LogRotator::with_path(0, log_path.clone());
    rotator
        .process(
            &dummy_event(),
            &mut dummy_result(),
            &PluginContext::without_deadline(),
        )
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
    assert!(
        rotator
            .process(
                &dummy_event(),
                &mut dummy_result(),
                &PluginContext::without_deadline(),
            )
            .is_ok()
    );
}

#[test]
fn test_oversized_input_is_rejected_without_replacing_original() {
    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("sessions.jsonl");
    let file = std::fs::File::create(&log_path).unwrap();
    file.set_len(u64::try_from(super::MAX_LOG_ROTATION_OVERAGE_BYTES + 1).unwrap())
        .unwrap();

    let rotator = LogRotator::with_path(0, log_path.clone());
    let error = rotator
        .process(
            &dummy_event(),
            &mut dummy_result(),
            &PluginContext::without_deadline(),
        )
        .unwrap_err();

    assert!(error.contains("rotation overage"));
    assert_eq!(
        std::fs::metadata(&log_path).unwrap().len(),
        u64::try_from(super::MAX_LOG_ROTATION_OVERAGE_BYTES + 1).unwrap()
    );
    assert_no_rotation_temp(dir.path());
}

#[test]
fn test_threshold_at_input_overage_still_rotates() {
    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("sessions.jsonl");
    let line = format!("{}\n", "x".repeat(32 * 1024));
    let mut content = line.repeat(257);
    while content.len() <= super::MAX_LOG_ROTATION_OVERAGE_BYTES {
        content.push_str(&line);
    }
    std::fs::write(&log_path, &content).unwrap();

    let rotator = LogRotator {
        max_size_bytes: u64::try_from(super::MAX_LOG_ROTATION_OVERAGE_BYTES).unwrap(),
        log_path_override: Some(log_path.clone()),
    };
    rotator
        .process(
            &dummy_event(),
            &mut dummy_result(),
            &PluginContext::without_deadline(),
        )
        .unwrap();

    assert!(std::fs::metadata(&log_path).unwrap().len() < content.len() as u64);
    assert_no_rotation_temp(dir.path());
}

#[test]
fn test_line_and_count_limits_preserve_original() {
    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("sessions.jsonl");
    let rotator = LogRotator::with_path(0, log_path.clone());

    let oversized_line = format!("{}\n", "x".repeat(super::MAX_LOG_LINE_BYTES + 1));
    std::fs::write(&log_path, &oversized_line).unwrap();
    let error = rotator
        .process(
            &dummy_event(),
            &mut dummy_result(),
            &PluginContext::without_deadline(),
        )
        .unwrap_err();
    assert!(error.contains("line larger"));
    assert_eq!(std::fs::read_to_string(&log_path).unwrap(), oversized_line);

    let too_many_lines = "x\n".repeat(super::MAX_LOG_LINES + 1);
    std::fs::write(&log_path, &too_many_lines).unwrap();
    let error = rotator
        .process(
            &dummy_event(),
            &mut dummy_result(),
            &PluginContext::without_deadline(),
        )
        .unwrap_err();
    assert!(error.contains("line-count limit"));
    assert_eq!(std::fs::read_to_string(&log_path).unwrap(), too_many_lines);
    assert_no_rotation_temp(dir.path());
}

#[test]
fn test_stale_legacy_rotation_temp_does_not_block_or_get_removed() {
    let dir = tempfile::tempdir().unwrap();
    let log_path = dir.path().join("sessions.jsonl");
    let tmp_path = log_path.with_extension("jsonl.tmp");
    std::fs::write(&log_path, "old\nnew\n").unwrap();
    std::fs::write(&tmp_path, "belongs-to-another-rotation").unwrap();

    LogRotator::with_path(0, log_path.clone())
        .process(
            &dummy_event(),
            &mut dummy_result(),
            &PluginContext::without_deadline(),
        )
        .unwrap();

    assert_eq!(std::fs::read_to_string(&log_path).unwrap(), "new\n");
    assert_eq!(
        std::fs::read_to_string(&tmp_path).unwrap(),
        "belongs-to-another-rotation"
    );
    assert_no_rotation_temp_except(dir.path(), &tmp_path);
}

fn assert_no_rotation_temp(dir: &std::path::Path) {
    assert_no_rotation_temp_except(dir, std::path::Path::new(""));
}

fn assert_no_rotation_temp_except(dir: &std::path::Path, allowed: &std::path::Path) {
    let leaked = std::fs::read_dir(dir).unwrap().flatten().any(|entry| {
        entry.path() != allowed && {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with(".sessions.jsonl.log-rotate-") && name.ends_with(".tmp")
        }
    });
    assert!(!leaked, "rotation temporary file leaked");
}
