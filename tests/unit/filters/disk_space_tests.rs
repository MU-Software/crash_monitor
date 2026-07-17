use crate::filters::DiskSpaceFilter;
use crate::pipeline::{CrashEvent, Filter, Plugin, ReportType};

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

#[test]
fn test_passes_when_enough_space() {
    // 100 MB should be available on any dev machine
    let filter = DiskSpaceFilter::new(100);
    let event = dummy_event();
    assert!(filter.should_process(&event).unwrap());
}

#[test]
fn test_blocks_when_threshold_absurd() {
    // No system has u64::MAX / (1024*1024) MB free
    let filter = DiskSpaceFilter::new(u64::MAX / (1024 * 1024));
    let event = dummy_event();
    assert!(!filter.should_process(&event).unwrap());
}

#[test]
fn test_plugin_metadata() {
    let filter = DiskSpaceFilter::new(100);
    assert_eq!(filter.name(), "DiskSpaceFilter");
    assert!(filter.is_available());
}
