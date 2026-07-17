//! Unit tests for `report::load_report` — plain JSON and ZIP archive loading.

use super::*;
use std::io::Write;
use std::path::PathBuf;
use tempfile::TempDir;

/// A minimal but schema-valid crash report JSON.
fn sample_report_json() -> String {
    serde_json::json!({
        "header": {
            "version": 1,
            "timestamp": "2026-04-05T12:00:00.000000000+09:00",
            "pid": 1234,
            "process": "voxelcore_desktop",
            "collector": "crash_monitor",
            "type": "crash"
        },
        "threads": [{
            "index": 0,
            "id": 100,
            "name": "main",
            "crashed": true,
            "registers": {},
            "backtrace": []
        }]
    })
    .to_string()
}

/// Write a ZIP named `zip_name` with the given (entry-name, bytes) files into a
/// fresh tempdir. Returns the tempdir (keep alive) and the archive path.
fn make_zip(zip_name: &str, entries: &[(&str, &[u8])]) -> (TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let zip_path = dir.path().join(zip_name);
    let file = std::fs::File::create(&zip_path).unwrap();
    let mut writer = zip::ZipWriter::new(file);
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, bytes) in entries {
        writer.start_file(*name, opts).unwrap();
        writer.write_all(bytes).unwrap();
    }
    writer.finish().unwrap();
    (dir, zip_path)
}

#[test]
fn is_zip_path_matches_extension_case_insensitively() {
    assert!(is_zip_path(Path::new("crash_x.zip")));
    assert!(is_zip_path(Path::new("crash_x.ZIP")));
    assert!(is_zip_path(Path::new("/a/b/report.Zip")));
    assert!(!is_zip_path(Path::new("crash_x.json")));
    assert!(!is_zip_path(Path::new("crash_x")));
    assert!(!is_zip_path(Path::new("archive.zip.json")));
}

#[test]
fn load_report_reads_plain_json() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("crash_test.json");
    std::fs::write(&path, sample_report_json()).unwrap();

    let report = load_report(&path).expect("plain JSON should load");
    assert_eq!(report.header.pid, 1234);
}

#[test]
fn load_report_reads_json_from_zip_matching_stem() {
    let json = sample_report_json();
    let (_dir, zip_path) = make_zip(
        "crash_test.zip",
        &[
            ("crash_test.json", json.as_bytes()),
            ("crash_test_screenshot_000.rgba", b"\x00\x01\x02\x03"),
        ],
    );

    let report = load_report(&zip_path).expect("report JSON should load from ZIP");
    assert_eq!(report.header.pid, 1234);
}

#[test]
fn load_report_falls_back_to_first_json_when_stem_absent() {
    // Archive stem is "archive" but the report entry is named differently;
    // the loader should still find the sole *.json entry.
    let json = sample_report_json();
    let (_dir, zip_path) = make_zip("archive.zip", &[("some_report.json", json.as_bytes())]);

    let report = load_report(&zip_path).expect("first *.json entry should load");
    assert_eq!(report.header.pid, 1234);
}

#[test]
fn load_report_prefers_stem_json_over_other_json_entries() {
    // A non-report attachment JSON appears first; the loader must still pick
    // "<stem>.json" (the real report), not the first *.json entry.
    let json = sample_report_json();
    let (_dir, zip_path) = make_zip(
        "crash_test.zip",
        &[
            ("attachment.json", b"{\"unrelated\":true}"),
            ("crash_test.json", json.as_bytes()),
        ],
    );

    let report = load_report(&zip_path).expect("stem-matching report JSON should win");
    assert_eq!(report.header.pid, 1234);
}

#[test]
fn load_report_errors_when_zip_has_no_json() {
    let (_dir, zip_path) = make_zip("crash_test.zip", &[("crash_test.rgba", b"raw pixels")]);

    let err = load_report(&zip_path)
        .err()
        .expect("ZIP without a report JSON should error");
    assert!(err.contains("no report JSON"), "unexpected error: {err}");
}

#[test]
fn load_report_errors_on_corrupt_zip() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("broken.zip");
    std::fs::write(&path, b"not a real zip file").unwrap();

    let err = load_report(&path).err().expect("corrupt ZIP should error");
    assert!(err.contains("invalid ZIP"), "unexpected error: {err}");
}

#[test]
fn load_report_errors_on_missing_zip() {
    let err = load_report(Path::new("/nonexistent/crash_test.zip"))
        .err()
        .expect("missing ZIP should error");
    assert!(err.contains("cannot open"), "unexpected error: {err}");
}
