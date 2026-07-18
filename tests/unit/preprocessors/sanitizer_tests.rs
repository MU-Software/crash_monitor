use crate::collectors::dylib::RawImageData;
use crate::pipeline::{CollectedData, CrashEvent, Plugin, PluginContext, PreProcessor, ReportType};
use crate::preprocessors::Sanitizer;

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
fn test_masks_username_in_image_paths() {
    // Get the current username to construct test data
    let user = match std::env::var("USER") {
        Ok(u) if !u.is_empty() => u,
        _ => return, // Skip if USER not set
    };

    let sanitizer = Sanitizer::new();
    let event = dummy_event();
    let mut data = CollectedData::default();
    data.raw.images.push(RawImageData {
        path: format!("/Users/{user}/project/libfoo.dylib"),
        base_address: 0x1000,
        slide: None,
        uuid: None,
        architecture: None,
        text_start: None,
        text_end: None,
    });

    sanitizer
        .process(&event, &mut data, &PluginContext::without_deadline())
        .unwrap();
    assert_eq!(
        data.raw.images[0].path,
        "/Users/[USERNAME]/project/libfoo.dylib"
    );
}

#[test]
fn test_masks_username_in_symbols() {
    let user = match std::env::var("USER") {
        Ok(u) if !u.is_empty() => u,
        _ => return,
    };

    let sanitizer = Sanitizer::new();
    let event = dummy_event();
    let mut data = CollectedData::default();
    data.raw
        .symbols
        .insert(0x1000, format!("/Users/{user}/src/main.c:foo"));

    sanitizer
        .process(&event, &mut data, &PluginContext::without_deadline())
        .unwrap();
    assert_eq!(
        data.raw.symbols[&0x1000],
        "/Users/[USERNAME]/src/main.c:foo"
    );
}

#[test]
fn test_no_username_is_noop() {
    // We can't unset USER in a test safely, but we can test that the
    // sanitizer does nothing when username is None.
    let sanitizer = Sanitizer { username: None };
    let event = dummy_event();
    let mut data = CollectedData::default();
    data.raw.images.push(RawImageData {
        path: "/Users/alice/project/lib.dylib".into(),
        base_address: 0x1000,
        slide: None,
        uuid: None,
        architecture: None,
        text_start: None,
        text_end: None,
    });

    sanitizer
        .process(&event, &mut data, &PluginContext::without_deadline())
        .unwrap();
    // Path should remain unchanged
    assert_eq!(data.raw.images[0].path, "/Users/alice/project/lib.dylib");
}

#[test]
fn test_depends_on_fingerprinter() {
    let sanitizer = Sanitizer::new();
    assert!(sanitizer.hard_dependencies().is_empty());
    assert_eq!(sanitizer.order_after(), &["Fingerprinter"]);
}

#[test]
fn test_preserves_non_path_strings() {
    let sanitizer = Sanitizer::new();
    let event = dummy_event();
    let mut data = CollectedData::default();
    data.raw.images.push(RawImageData {
        path: "/usr/lib/system/libsystem.dylib".into(),
        base_address: 0x2000,
        slide: None,
        uuid: None,
        architecture: None,
        text_start: None,
        text_end: None,
    });

    sanitizer
        .process(&event, &mut data, &PluginContext::without_deadline())
        .unwrap();
    assert_eq!(data.raw.images[0].path, "/usr/lib/system/libsystem.dylib");
}
