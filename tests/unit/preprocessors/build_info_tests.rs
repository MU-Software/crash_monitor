use crate::pipeline::{CollectedData, CrashEvent, Plugin, PluginContext, PreProcessor, ReportType};
use crate::preprocessors::BuildInfoEnricher;
use crate::shm::RawCrashContext;

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

fn make_context() -> RawCrashContext {
    RawCrashContext {
        heartbeat_counter: 99,
        session_start_ns: 0,
        session_id: "test-session".into(),
        annotations: vec![("env".into(), "test".into())],
        app_version: "1.2.3".into(),
        build_number: 456,
        git_hash: "abc123def".into(),
        git_dirty: false,
        build_type: "Debug".into(),
        build_preset: "default".into(),
        build_timestamp: "2026-04-11".into(),
        compiler: "clang-17".into(),
        os_version: "15.3".into(),
    }
}

#[test]
fn test_extracts_version_from_context() {
    let enricher = BuildInfoEnricher;
    let event = dummy_event();
    let mut data = CollectedData::default();
    data.raw.crash_context = Some(make_context());

    enricher
        .process(&event, &mut data, &PluginContext::without_deadline())
        .unwrap();

    let info = data.build_info.as_ref().expect("build_info should be set");
    assert_eq!(info.app_version, "1.2.3");
    assert_eq!(info.build_number, 456);
    assert_eq!(info.git_hash, "abc123def");
}

#[test]
fn test_extracts_annotations() {
    let enricher = BuildInfoEnricher;
    let event = dummy_event();
    let mut data = CollectedData::default();
    data.raw.crash_context = Some(make_context());

    enricher
        .process(&event, &mut data, &PluginContext::without_deadline())
        .unwrap();

    let info = data.build_info.as_ref().unwrap();
    assert_eq!(info.annotations.len(), 1);
    assert_eq!(info.annotations[0], ("env".to_string(), "test".to_string()));
}

#[test]
fn test_no_context_is_noop() {
    let enricher = BuildInfoEnricher;
    let event = dummy_event();
    let mut data = CollectedData::default();
    // crash_context is None

    enricher
        .process(&event, &mut data, &PluginContext::without_deadline())
        .unwrap();
    assert!(data.build_info.is_none());
}

#[test]
fn test_plugin_metadata() {
    let enricher = BuildInfoEnricher;
    assert_eq!(enricher.name(), "BuildInfoEnricher");
    assert!(enricher.is_available());
    assert!(enricher.depends_on().is_empty());
}
