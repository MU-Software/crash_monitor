use crate::pipeline::{CollectedData, CrashEvent, Plugin, PluginContext, PreProcessor, ReportType};
use crate::preprocessors::DuplicateDetector;
use std::time::Duration;

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

fn data_with_fingerprint(fp: &str) -> CollectedData {
    CollectedData {
        fingerprint: Some(fp.to_string()),
        ..CollectedData::default()
    }
}

#[test]
fn test_first_event_always_passes() {
    let detector = DuplicateDetector::new(Duration::from_secs(60));
    let event = dummy_event();
    let mut data = data_with_fingerprint("abc123");

    detector
        .process(&event, &mut data, &PluginContext::without_deadline())
        .unwrap();
    assert!(!data.duplicate_detected);
}

#[test]
fn test_duplicate_within_window_detected() {
    let detector = DuplicateDetector::new(Duration::from_secs(60));
    let event = dummy_event();

    // First event
    let mut data1 = data_with_fingerprint("abc123");
    detector
        .process(&event, &mut data1, &PluginContext::without_deadline())
        .unwrap();
    assert!(!data1.duplicate_detected);

    // Same fingerprint again
    let mut data2 = data_with_fingerprint("abc123");
    detector
        .process(&event, &mut data2, &PluginContext::without_deadline())
        .unwrap();
    assert!(data2.duplicate_detected);
}

#[test]
fn test_duplicate_outside_window_passes() {
    // Use a zero-duration window — entries expire immediately
    let detector = DuplicateDetector::new(Duration::ZERO);
    let event = dummy_event();

    let mut data1 = data_with_fingerprint("abc123");
    detector
        .process(&event, &mut data1, &PluginContext::without_deadline())
        .unwrap();
    assert!(!data1.duplicate_detected);

    // Window expired (ZERO duration) — should pass
    let mut data2 = data_with_fingerprint("abc123");
    detector
        .process(&event, &mut data2, &PluginContext::without_deadline())
        .unwrap();
    assert!(!data2.duplicate_detected);
}

#[test]
fn test_different_fingerprints_not_blocked() {
    let detector = DuplicateDetector::new(Duration::from_secs(60));
    let event = dummy_event();

    let mut data1 = data_with_fingerprint("abc123");
    detector
        .process(&event, &mut data1, &PluginContext::without_deadline())
        .unwrap();

    let mut data2 = data_with_fingerprint("def456");
    detector
        .process(&event, &mut data2, &PluginContext::without_deadline())
        .unwrap();
    assert!(!data2.duplicate_detected);
}

#[test]
fn test_duplicate_key_separates_report_type_process_and_build() {
    let detector = DuplicateDetector::new(Duration::from_secs(60));
    let base = dummy_event();
    let mut first = data_with_fingerprint("same");
    detector
        .process(&base, &mut first, &PluginContext::without_deadline())
        .unwrap();

    let mut snapshot = base.clone();
    snapshot.report_type = ReportType::Snapshot;
    let mut snapshot_data = data_with_fingerprint("same");
    detector
        .process(
            &snapshot,
            &mut snapshot_data,
            &PluginContext::without_deadline(),
        )
        .unwrap();
    assert!(!snapshot_data.duplicate_detected);

    let mut other_process = base.clone();
    other_process.process_name = "other".into();
    let mut process_data = data_with_fingerprint("same");
    detector
        .process(
            &other_process,
            &mut process_data,
            &PluginContext::without_deadline(),
        )
        .unwrap();
    assert!(!process_data.duplicate_detected);

    let mut build_data = data_with_fingerprint("same");
    build_data.build_info = Some(crate::preprocessors::build_info::BuildInfo {
        app_version: "2.0".into(),
        build_number: 2,
        git_hash: "def".into(),
        build_type: "release".into(),
        build_preset: String::new(),
        annotations: Vec::new(),
    });
    detector
        .process(&base, &mut build_data, &PluginContext::without_deadline())
        .unwrap();
    assert!(!build_data.duplicate_detected);
}

#[test]
fn test_duplicate_observations_do_not_extend_suppression_window() {
    let detector = DuplicateDetector::new(Duration::from_secs(60));
    let event = dummy_event();
    let base = std::time::Instant::now();
    let context = PluginContext::without_deadline();

    let mut first = data_with_fingerprint("same");
    detector
        .process_at(&event, &mut first, &context, base)
        .unwrap();
    let mut repeated = data_with_fingerprint("same");
    detector
        .process_at(
            &event,
            &mut repeated,
            &context,
            base + Duration::from_secs(50),
        )
        .unwrap();
    assert!(repeated.duplicate_detected);

    let entry = detector
        .recent
        .lock()
        .unwrap()
        .values()
        .next()
        .unwrap()
        .clone();
    assert_eq!(entry.first_seen, base);
    assert_eq!(entry.last_observed, base + Duration::from_secs(50));
    assert_eq!(entry.occurrences, 2);

    let mut after_window = data_with_fingerprint("same");
    detector
        .process_at(
            &event,
            &mut after_window,
            &context,
            base + Duration::from_secs(61),
        )
        .unwrap();
    assert!(!after_window.duplicate_detected);
}

#[test]
fn test_contended_state_returns_without_waiting() {
    let detector = DuplicateDetector::new(Duration::from_secs(60));
    let _guard = detector.recent.lock().unwrap();
    let mut data = data_with_fingerprint("same");

    let error = detector
        .process(
            &dummy_event(),
            &mut data,
            &PluginContext::without_deadline(),
        )
        .unwrap_err();

    assert!(error.contains("duplicate state unavailable"));
}

#[test]
fn test_no_fingerprint_passes() {
    let detector = DuplicateDetector::new(Duration::from_secs(60));
    let event = dummy_event();
    let mut data = CollectedData::default(); // fingerprint is None

    detector
        .process(&event, &mut data, &PluginContext::without_deadline())
        .unwrap();
    assert!(!data.duplicate_detected);
}

#[test]
fn test_depends_on_fingerprinter() {
    let detector = DuplicateDetector::new(Duration::from_secs(60));
    assert_eq!(detector.hard_dependencies(), &["Fingerprinter"]);
    assert!(detector.order_after().is_empty());
}

#[test]
fn test_plugin_metadata() {
    let detector = DuplicateDetector::new(Duration::from_secs(60));
    assert_eq!(detector.name(), "DuplicateDetector");
    assert!(detector.is_available());
}
