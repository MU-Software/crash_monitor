use crate::collectors::environment::EnvironmentCollector;
use crate::pipeline::traits::Collector;
use crate::pipeline::{CollectedData, CrashEvent, Plugin, PluginContext, ReportType};

fn dummy_event() -> CrashEvent {
    CrashEvent {
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
    assert!(!env.os_version.is_empty(), "os_version should not be empty");
    assert!(!env.arch.is_empty(), "arch should not be empty");
    assert!(!env.hostname.is_empty(), "hostname should not be empty");
}

#[test]
fn test_filters_sensitive_env_vars() {
    let collector = EnvironmentCollector::new();
    let event = dummy_event();
    let mut data = CollectedData::default();

    // SAFETY: These test-specific env vars don't interfere with other tests.
    unsafe {
        std::env::set_var("CRASH_TEST_NORMAL", "visible");
        std::env::set_var("CRASH_TEST_SECRET_VALUE", "hidden");
        std::env::set_var("CRASH_TEST_API_TOKEN", "hidden");
    }

    collector
        .collect(&event, 0, &mut data, &PluginContext::without_deadline())
        .unwrap();

    let env = data.raw.environment.as_ref().unwrap();

    let has_normal = env.env_vars.iter().any(|(k, _)| k == "CRASH_TEST_NORMAL");
    let has_secret = env
        .env_vars
        .iter()
        .any(|(k, _)| k == "CRASH_TEST_SECRET_VALUE");
    let has_token = env
        .env_vars
        .iter()
        .any(|(k, _)| k == "CRASH_TEST_API_TOKEN");

    assert!(has_normal, "normal env var should be captured");
    assert!(!has_secret, "SECRET env var should be filtered");
    assert!(!has_token, "TOKEN env var should be filtered");

    // Clean up
    unsafe {
        std::env::remove_var("CRASH_TEST_NORMAL");
        std::env::remove_var("CRASH_TEST_SECRET_VALUE");
        std::env::remove_var("CRASH_TEST_API_TOKEN");
    }
}

#[test]
fn test_plugin_metadata() {
    let collector = EnvironmentCollector::new();
    assert_eq!(collector.name(), "EnvironmentCollector");
    assert!(collector.is_available());
}
