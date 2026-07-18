use super::*;
use crate::collectors::RawData;
use crate::collectors::dylib::RawImageData;
use crate::collectors::thread::RawThreadData;
use crate::pipeline::types::CollectedData;
use crate::pipeline::{CrashEvent, Plugin, PluginContext, PreProcessor, ReportType};
use std::collections::BTreeMap;

fn make_event() -> CrashEvent {
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
        pid: 1234,
        process_name: "test".into(),
        hang_duration_ms: None,
    }
}

fn make_data_with_backtrace(
    backtrace: Vec<u64>,
    crashed: bool,
    images: Vec<RawImageData>,
    symbols: BTreeMap<u64, String>,
) -> CollectedData {
    CollectedData {
        raw: RawData {
            threads: vec![RawThreadData {
                thread_port: 0,
                thread_id: 100,
                name: Some("main".into()),
                crashed,
                registers: None,
                backtrace,
                stack_capture: None,
            }],
            images,
            symbols,
            ..RawData::default()
        },
        session: None,
        fingerprint: None,
        build_info: None,
        duplicate_detected: false,
    }
}

fn app_image() -> RawImageData {
    RawImageData {
        path: "/Users/me/build/voxelcore_desktop".into(),
        base_address: 0x1_0000_0000,
        slide: Some(0x1000),
    }
}

fn system_image() -> RawImageData {
    RawImageData {
        path: "/usr/lib/system/libsystem_pthread.dylib".into(),
        base_address: 0x2_0000_0000,
        slide: None,
    }
}

#[test]
fn test_same_backtrace_same_fingerprint() {
    let fp = Fingerprinter::new(8);
    let event = make_event();
    let mut symbols = BTreeMap::new();
    symbols.insert(0x1_0000_1000, "func_a".into());
    symbols.insert(0x1_0000_2000, "func_b".into());

    let mut data1 = make_data_with_backtrace(
        vec![0x1_0000_1000, 0x1_0000_2000],
        true,
        vec![app_image()],
        symbols.clone(),
    );
    let mut data2 = make_data_with_backtrace(
        vec![0x1_0000_1000, 0x1_0000_2000],
        true,
        vec![app_image()],
        symbols,
    );

    let _ = fp.process(&event, &mut data1, &PluginContext::without_deadline());
    let _ = fp.process(&event, &mut data2, &PluginContext::without_deadline());

    assert!(data1.fingerprint.is_some());
    assert_eq!(data1.fingerprint, data2.fingerprint);
}

#[test]
fn test_different_backtrace_different_fingerprint() {
    let fp = Fingerprinter::new(8);
    let event = make_event();

    let mut sym1 = BTreeMap::new();
    sym1.insert(0x1_0000_1000, "func_a".into());
    let mut sym2 = BTreeMap::new();
    sym2.insert(0x1_0000_3000, "func_c".into());

    let mut data1 = make_data_with_backtrace(vec![0x1_0000_1000], true, vec![app_image()], sym1);
    let mut data2 = make_data_with_backtrace(vec![0x1_0000_3000], true, vec![app_image()], sym2);

    let _ = fp.process(&event, &mut data1, &PluginContext::without_deadline());
    let _ = fp.process(&event, &mut data2, &PluginContext::without_deadline());

    assert!(data1.fingerprint.is_some());
    assert!(data2.fingerprint.is_some());
    assert_ne!(data1.fingerprint, data2.fingerprint);
}

#[test]
fn test_offset_ignored_in_fingerprint() {
    // Same function at different offsets should produce the same fingerprint
    let fp = Fingerprinter::new(8);
    let event = make_event();

    let mut symbols = BTreeMap::new();
    // Both addresses resolve to "func_a" (different offsets within the function)
    symbols.insert(0x1_0000_1000, "func_a".into());
    symbols.insert(0x1_0000_1050, "func_a".into());

    let mut data1 = make_data_with_backtrace(
        vec![0x1_0000_1000],
        true,
        vec![app_image()],
        symbols.clone(),
    );
    let mut data2 = make_data_with_backtrace(vec![0x1_0000_1050], true, vec![app_image()], {
        let mut s = BTreeMap::new();
        s.insert(0x1_0000_1050, "func_a".into());
        s
    });

    let _ = fp.process(&event, &mut data1, &PluginContext::without_deadline());
    let _ = fp.process(&event, &mut data2, &PluginContext::without_deadline());

    assert_eq!(data1.fingerprint, data2.fingerprint);
}

#[test]
fn test_system_frames_excluded() {
    let fp = Fingerprinter::new(8);
    let event = make_event();

    let mut symbols = BTreeMap::new();
    symbols.insert(0x1_0000_1000, "app_func".into());
    symbols.insert(0x2_0000_0100, "pthread_create".into());

    // Backtrace: system frame first, then app frame
    let mut data = make_data_with_backtrace(
        vec![0x2_0000_0100, 0x1_0000_1000],
        true,
        vec![app_image(), system_image()],
        symbols,
    );

    let _ = fp.process(&event, &mut data, &PluginContext::without_deadline());

    // Fingerprint should be based only on app_func, not pthread_create
    let mut symbols2 = BTreeMap::new();
    symbols2.insert(0x1_0000_1000, "app_func".into());
    let mut data2 =
        make_data_with_backtrace(vec![0x1_0000_1000], true, vec![app_image()], symbols2);
    let _ = fp.process(&event, &mut data2, &PluginContext::without_deadline());

    assert_eq!(data.fingerprint, data2.fingerprint);
}

#[test]
fn test_empty_backtrace_produces_fingerprint() {
    let fp = Fingerprinter::new(8);
    let event = make_event();

    let mut data = make_data_with_backtrace(vec![], true, vec![app_image()], BTreeMap::new());

    let _ = fp.process(&event, &mut data, &PluginContext::without_deadline());
    // Even an empty backtrace should produce a fingerprint (hash of empty input)
    assert!(data.fingerprint.is_some());
}

#[test]
fn test_no_crashed_thread_uses_thread_0() {
    let fp = Fingerprinter::new(8);
    let event = make_event();

    let mut symbols = BTreeMap::new();
    symbols.insert(0x1_0000_1000, "func_a".into());

    // No crashed thread — should use thread 0
    let mut data = make_data_with_backtrace(
        vec![0x1_0000_1000],
        false, // not crashed
        vec![app_image()],
        symbols,
    );

    let _ = fp.process(&event, &mut data, &PluginContext::without_deadline());
    assert!(data.fingerprint.is_some());
}

#[test]
fn test_no_threads_no_fingerprint() {
    let fp = Fingerprinter::new(8);
    let event = make_event();

    let mut data = CollectedData::default();
    let _ = fp.process(&event, &mut data, &PluginContext::without_deadline());
    // No threads at all — no fingerprint
    assert!(data.fingerprint.is_none());
}

#[test]
fn test_is_system_image() {
    assert!(is_system_image("/usr/lib/system/libsystem_pthread.dylib"));
    assert!(is_system_image(
        "/System/Library/Frameworks/CoreFoundation.framework/CoreFoundation"
    ));
    assert!(is_system_image("/Library/Apple/usr/lib/libdispatch.dylib"));
    assert!(is_system_image("/AppleInternal/Library/test.dylib"));

    assert!(!is_system_image("/Users/me/build/voxelcore_desktop"));
    assert!(!is_system_image("/opt/homebrew/lib/libfreetype.dylib"));
    assert!(!is_system_image(""));
}

#[test]
fn test_fingerprint_is_16_hex_chars() {
    let fp = Fingerprinter::new(8);
    let event = make_event();

    let mut symbols = BTreeMap::new();
    symbols.insert(0x1_0000_1000, "func_a".into());

    let mut data = make_data_with_backtrace(vec![0x1_0000_1000], true, vec![app_image()], symbols);

    let _ = fp.process(&event, &mut data, &PluginContext::without_deadline());
    let fp_str = data.fingerprint.as_ref().unwrap();
    assert_eq!(fp_str.len(), 16);
    assert!(fp_str.chars().all(|c| c.is_ascii_hexdigit()));
}

#[test]
fn test_plugin_metadata() {
    let fp = Fingerprinter::new(8);
    assert_eq!(fp.name(), "Fingerprinter");
    assert_eq!(fp.priority(), Priority::Low);
    assert!(fp.hard_dependencies().is_empty());
    assert_eq!(fp.order_after(), &["SymbolResolver"]);
    assert!(fp.is_available());
}

#[test]
fn test_top_frame_count_is_bounded() {
    let fingerprinter = Fingerprinter::new(usize::MAX);
    assert_eq!(fingerprinter.top_n, MAX_FINGERPRINT_FRAMES);
}
