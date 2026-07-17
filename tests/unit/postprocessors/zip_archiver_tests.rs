use crate::pipeline::{CrashEvent, Plugin, PostProcessor, ReportResult, ReportType};
use crate::postprocessors::ZIPArchiver;
use std::io::Read;

fn dummy_event() -> CrashEvent {
    CrashEvent {
        report_type: ReportType::Crash,
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
fn test_creates_zip_with_json_and_png() {
    let dir = tempfile::tempdir().unwrap();
    let json_path = dir.path().join("crash_20260411_120000_1234.json");
    let png_path = dir
        .path()
        .join("crash_20260411_120000_1234_screenshot_000.png");

    std::fs::write(&json_path, r#"{"header":{}}"#).unwrap();
    std::fs::write(&png_path, b"fakepng").unwrap();

    let mut result = ReportResult {
        raw_path: None,
        json_path: Some(json_path),
        session: None,
    };

    let archiver = ZIPArchiver;
    archiver.process(&dummy_event(), &mut result).unwrap();

    let zip_path = dir.path().join("crash_20260411_120000_1234.zip");
    assert!(zip_path.exists(), "ZIP file should be created");
}

#[test]
fn test_zip_contains_all_files() {
    let dir = tempfile::tempdir().unwrap();
    let json_path = dir.path().join("crash_20260411_120000_5678.json");
    let png_path = dir
        .path()
        .join("crash_20260411_120000_5678_screenshot_000.png");

    std::fs::write(&json_path, r#"{"test":"data"}"#).unwrap();
    std::fs::write(&png_path, b"pngdata").unwrap();

    let mut result = ReportResult {
        raw_path: None,
        json_path: Some(json_path),
        session: None,
    };

    ZIPArchiver.process(&dummy_event(), &mut result).unwrap();

    let zip_path = dir.path().join("crash_20260411_120000_5678.zip");
    let file = std::fs::File::open(&zip_path).unwrap();
    let mut archive = zip::ZipArchive::new(file).unwrap();

    let mut names: Vec<String> = (0..archive.len())
        .map(|i| archive.by_index(i).unwrap().name().to_string())
        .collect();
    names.sort();

    assert!(names.contains(&"crash_20260411_120000_5678.json".to_string()));
    assert!(names.contains(&"crash_20260411_120000_5678_screenshot_000.png".to_string()));

    // Verify content
    let mut json_entry = archive.by_name("crash_20260411_120000_5678.json").unwrap();
    let mut content = String::new();
    json_entry.read_to_string(&mut content).unwrap();
    assert_eq!(content, r#"{"test":"data"}"#);
}

#[test]
fn test_no_json_path_skips() {
    let mut result = ReportResult {
        raw_path: None,
        json_path: None,
        session: None,
    };
    assert!(ZIPArchiver.process(&dummy_event(), &mut result).is_ok());
}

#[test]
fn test_originals_deleted_after_zip() {
    let dir = tempfile::tempdir().unwrap();
    let json_path = dir.path().join("snap_20260411_120000_9999.json");
    std::fs::write(&json_path, "{}").unwrap();

    let mut result = ReportResult {
        raw_path: None,
        json_path: Some(json_path.clone()),
        session: None,
    };

    ZIPArchiver.process(&dummy_event(), &mut result).unwrap();
    assert!(!json_path.exists(), "original JSON should be deleted");
    assert!(
        dir.path().join("snap_20260411_120000_9999.zip").exists(),
        "ZIP should exist"
    );
}

#[test]
fn test_depends_on_feedback() {
    assert_eq!(ZIPArchiver.depends_on(), &["FeedbackDialog"]);
}

#[test]
fn test_plugin_metadata() {
    assert_eq!(ZIPArchiver.name(), "ZIPArchiver");
    assert!(ZIPArchiver.is_available());
}
