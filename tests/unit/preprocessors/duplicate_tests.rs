use crate::pipeline::{CollectedData, CrashEvent, Plugin, PreProcessor, ReportType};
use crate::preprocessors::DuplicateDetector;
use std::time::Duration;

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

fn data_with_fingerprint(fp: &str) -> CollectedData {
    let mut data = CollectedData::default();
    data.fingerprint = Some(fp.to_string());
    data
}

#[test]
fn test_first_event_always_passes() {
    let detector = DuplicateDetector::new(Duration::from_secs(60));
    let event = dummy_event();
    let mut data = data_with_fingerprint("abc123");

    detector.process(&event, &mut data).unwrap();
    assert!(!data.duplicate_detected);
}

#[test]
fn test_duplicate_within_window_detected() {
    let detector = DuplicateDetector::new(Duration::from_secs(60));
    let event = dummy_event();

    // First event
    let mut data1 = data_with_fingerprint("abc123");
    detector.process(&event, &mut data1).unwrap();
    assert!(!data1.duplicate_detected);

    // Same fingerprint again
    let mut data2 = data_with_fingerprint("abc123");
    detector.process(&event, &mut data2).unwrap();
    assert!(data2.duplicate_detected);
}

#[test]
fn test_duplicate_outside_window_passes() {
    // Use a zero-duration window — entries expire immediately
    let detector = DuplicateDetector::new(Duration::ZERO);
    let event = dummy_event();

    let mut data1 = data_with_fingerprint("abc123");
    detector.process(&event, &mut data1).unwrap();
    assert!(!data1.duplicate_detected);

    // Window expired (ZERO duration) — should pass
    let mut data2 = data_with_fingerprint("abc123");
    detector.process(&event, &mut data2).unwrap();
    assert!(!data2.duplicate_detected);
}

#[test]
fn test_different_fingerprints_not_blocked() {
    let detector = DuplicateDetector::new(Duration::from_secs(60));
    let event = dummy_event();

    let mut data1 = data_with_fingerprint("abc123");
    detector.process(&event, &mut data1).unwrap();

    let mut data2 = data_with_fingerprint("def456");
    detector.process(&event, &mut data2).unwrap();
    assert!(!data2.duplicate_detected);
}

#[test]
fn test_no_fingerprint_passes() {
    let detector = DuplicateDetector::new(Duration::from_secs(60));
    let event = dummy_event();
    let mut data = CollectedData::default(); // fingerprint is None

    detector.process(&event, &mut data).unwrap();
    assert!(!data.duplicate_detected);
}

#[test]
fn test_depends_on_fingerprinter() {
    let detector = DuplicateDetector::new(Duration::from_secs(60));
    assert_eq!(detector.depends_on(), &["Fingerprinter"]);
}

#[test]
fn test_plugin_metadata() {
    let detector = DuplicateDetector::new(Duration::from_secs(60));
    assert_eq!(detector.name(), "DuplicateDetector");
    assert!(detector.is_available());
}
