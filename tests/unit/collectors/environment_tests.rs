use crate::collectors::environment::{ChildEnvironmentSnapshot, EnvironmentCollector};
use crate::pipeline::traits::Collector;
use crate::pipeline::{CollectedData, CrashEvent, Plugin, PluginContext, ReportType};
use std::ffi::CString;
use std::sync::Arc;

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

#[test]
fn test_captures_os_info() {
    let collector = EnvironmentCollector::new();
    let event = dummy_event();
    let mut data = CollectedData::default();

    collector
        .collect(&event, 0, &mut data, &PluginContext::without_deadline())
        .unwrap();

    let env = data
        .raw
        .environment
        .as_ref()
        .expect("environment should be populated");
    assert!(
        !env.kernel_release.is_empty(),
        "kernel_release should not be empty"
    );
    assert!(!env.arch.is_empty(), "arch should not be empty");
    assert!(!env.hostname.is_empty(), "hostname should not be empty");
}

#[test]
fn test_without_spawn_snapshot_does_not_capture_monitor_environment() {
    let collector = EnvironmentCollector::new();
    let event = dummy_event();
    let mut data = CollectedData::default();

    collector
        .collect(&event, 0, &mut data, &PluginContext::without_deadline())
        .unwrap();

    let env = data.raw.environment.as_ref().unwrap();

    assert_eq!(env.variables_source, "unavailable");
    assert!(env.env_vars.is_empty());
}

#[test]
fn test_plugin_metadata() {
    let collector = EnvironmentCollector::new();
    assert_eq!(collector.name(), "EnvironmentCollector");
    assert!(collector.is_available());
}

#[test]
fn injected_child_environment_is_used_and_non_utf8_is_skipped() {
    let environment = [
        CString::new("CHILD_VISIBLE=child-value").unwrap(),
        CString::new("CHILD_SECRET=hidden").unwrap(),
        CString::new(vec![b'N', b'O', b'N', b'=', 0xff]).unwrap(),
    ];
    let collector = EnvironmentCollector::with_child_environment(Arc::new(
        ChildEnvironmentSnapshot::from_c_strings(&environment),
    ));
    let mut data = CollectedData::default();

    collector
        .collect(
            &dummy_event(),
            0,
            &mut data,
            &PluginContext::without_deadline(),
        )
        .unwrap();

    let env = &data.raw.environment.as_ref().unwrap().env_vars;
    assert_eq!(
        env,
        &vec![("CHILD_VISIBLE".to_string(), "child-value".to_string())]
    );
    assert_eq!(
        data.raw.environment.as_ref().unwrap().variables_source,
        "spawn_environment_snapshot"
    );
}
