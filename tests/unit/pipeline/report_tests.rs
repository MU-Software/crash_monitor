//! Unit tests for `report::load_report` — plain JSON and ZIP archive loading.

use super::*;
use std::io::{Read, Write};
use std::os::unix::fs::{PermissionsExt, symlink};
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
    assert_eq!(report.header.report_id, None);
    assert!(report.termination.is_none());
}

#[test]
fn external_report_load_does_not_apply_managed_storage_policy() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let path = dir.path().join("exported-report.json");
    std::fs::write(&path, sample_report_json()).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

    let report = load_report(&path).expect("external report should remain importable");

    assert_eq!(report.header.pid, 1234);
    assert_eq!(
        std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o7777,
        0o755
    );
    assert_eq!(
        std::fs::metadata(path).unwrap().permissions().mode() & 0o7777,
        0o644
    );
}

#[test]
fn legacy_report_load_is_stable_and_does_not_invent_an_identity() {
    let legacy = sample_report_json();
    let first: CrashReport = serde_json::from_str(&legacy).unwrap();
    let second: CrashReport = serde_json::from_str(&legacy).unwrap();

    assert_eq!(first.header.report_id, None);
    assert_eq!(second.header.report_id, None);
    let serialized = serde_json::to_value(first).unwrap();
    assert!(serialized["header"].get("report_id").is_none());
}

#[test]
fn report_without_termination_omits_the_field() {
    let report: CrashReport = serde_json::from_str(&sample_report_json()).unwrap();
    let value = serde_json::to_value(report).unwrap();
    assert!(value.get("termination").is_none());
}

#[test]
fn build_report_preserves_exit_termination() {
    let event = CrashEvent {
        report_id: crate::pipeline::ReportId::default(),
        report_type: ReportType::ExitFailure,
        termination: Some(TerminationReason::Exited {
            exit_code: 23,
            runtime_ms: 4_567,
        }),
        exception_type: None,
        exception_code: None,
        exception_subcode: None,
        exception_codes: Vec::new(),
        crashed_thread: None,
        bail_on_suspend_failure: false,
        pid: 1234,
        process_name: "test_app".into(),
        hang_duration_ms: None,
    };

    let report = build_report(&event, &CollectedData::default(), &Diagnostics::new());
    assert_eq!(report.header.report_id.as_ref(), Some(&event.report_id));
    assert_eq!(report.header.report_type, ReportType::ExitFailure);
    assert_eq!(report.termination, event.termination);
    assert!(report.exception.is_none());
    assert_eq!(
        serde_json::to_value(&report).unwrap()["termination"],
        serde_json::json!({
            "kind": "exited",
            "exit_code": 23,
            "runtime_ms": 4_567
        })
    );
}

#[test]
fn build_report_preserves_the_exact_mach_code_array() {
    let raw_codes = vec![0xfedc_ba98_7654_3210, 0x0123_4567_89ab_cdef];
    let event = CrashEvent {
        report_id: crate::pipeline::ReportId::default(),
        report_type: ReportType::Crash,
        termination: None,
        exception_type: Some(1),
        exception_code: raw_codes.first().copied(),
        exception_subcode: raw_codes.get(1).copied(),
        exception_codes: raw_codes.clone(),
        crashed_thread: Some(42),
        bail_on_suspend_failure: false,
        pid: 1234,
        process_name: "test_app".into(),
        hang_duration_ms: None,
    };

    let report = build_report(&event, &CollectedData::default(), &Diagnostics::new());
    let exception = report.exception.expect("Mach crash exception");
    assert_eq!(
        exception.raw_codes,
        vec!["0xfedcba9876543210", "0x123456789abcdef"]
    );
    assert_eq!(event.exception_codes, raw_codes);
}

#[test]
fn report_deserializes_signal_termination() {
    let mut value: serde_json::Value = serde_json::from_str(&sample_report_json()).unwrap();
    value["header"]["type"] = serde_json::json!("signal_failure");
    value["termination"] = serde_json::json!({
        "kind": "signaled",
        "signal": 6,
        "core_dumped": true,
        "runtime_ms": 321
    });

    let report: CrashReport = serde_json::from_value(value).unwrap();
    assert_eq!(report.header.report_type, ReportType::SignalFailure);
    assert_eq!(
        report.termination,
        Some(TerminationReason::Signaled {
            signal: 6,
            core_dumped: true,
            runtime_ms: 321,
        })
    );
}

#[test]
fn update_termination_atomically_rewrites_plain_report() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("crash_test.json");
    let mut original: serde_json::Value = serde_json::from_str(&sample_report_json()).unwrap();
    original["future_plugin_field"] = serde_json::json!({"preserve": [1, 2, 3]});
    original["header"]["future_schema_field"] = serde_json::json!("keep-me");
    std::fs::write(&path, serde_json::to_vec(&original).unwrap()).unwrap();
    let reason = TerminationReason::Signaled {
        signal: 11,
        core_dumped: true,
        runtime_ms: 987,
    };

    update_termination(&path, reason).expect("plain report should be updated");

    assert_eq!(load_report(&path).unwrap().termination, Some(reason));
    let updated: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(
        updated["future_plugin_field"],
        original["future_plugin_field"]
    );
    assert_eq!(
        updated["header"]["future_schema_field"],
        original["header"]["future_schema_field"]
    );
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
        crate::utils::paths::PRIVATE_FILE_MODE
    );
    assert!(!dir.path().join(".crash_test.json.termination.tmp").exists());
}

#[test]
fn atomic_replace_exclusively_creates_private_output_and_rejects_symlink() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("new-report.json");
    atomic_replace(&path, b"{\"private\":true}").unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"{\"private\":true}");
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
        crate::utils::paths::PRIVATE_FILE_MODE
    );

    std::fs::remove_file(&path).unwrap();
    let outside = dir.path().join("outside.json");
    std::fs::write(&outside, b"outside").unwrap();
    symlink(&outside, &path).unwrap();
    let error = atomic_replace(&path, b"replacement").unwrap_err();
    assert!(error.contains("validate existing report"));
    assert_eq!(std::fs::read(&outside).unwrap(), b"outside");
}

#[test]
fn update_termination_rejects_managed_report_symlink_before_reading() {
    let dir = tempfile::tempdir().unwrap();
    let outside = dir.path().join("outside.json");
    let linked = dir.path().join("managed.json");
    let original = sample_report_json();
    std::fs::write(&outside, &original).unwrap();
    std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o600)).unwrap();
    symlink(&outside, &linked).unwrap();

    let error = update_termination(
        &linked,
        TerminationReason::Signaled {
            signal: 11,
            core_dumped: false,
            runtime_ms: 1,
        },
    )
    .unwrap_err();

    assert!(error.contains("safely open private report"), "{error}");
    assert_eq!(std::fs::read_to_string(outside).unwrap(), original);
    assert!(
        std::fs::symlink_metadata(linked)
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[test]
fn update_termination_rewrites_zip_and_preserves_attachments() {
    let json = sample_report_json();
    let attachment = b"unchanged attachment bytes";
    let (dir, zip_path) = make_zip(
        "crash_test.zip",
        &[
            ("crash_test.json", json.as_bytes()),
            ("crash_test_screenshot_000.png", attachment),
        ],
    );
    let reason = TerminationReason::Signaled {
        signal: 6,
        core_dumped: false,
        runtime_ms: 1_234,
    };

    update_termination(&zip_path, reason).expect("ZIP report should be updated");

    assert_eq!(load_report(&zip_path).unwrap().termination, Some(reason));
    let file = std::fs::File::open(&zip_path).unwrap();
    let mut archive = zip::ZipArchive::new(file).unwrap();
    let mut preserved = Vec::new();
    archive
        .by_name("crash_test_screenshot_000.png")
        .unwrap()
        .read_to_end(&mut preserved)
        .unwrap();
    assert_eq!(preserved, attachment);
    assert_eq!(
        std::fs::metadata(&zip_path).unwrap().permissions().mode() & 0o7777,
        crate::utils::paths::PRIVATE_FILE_MODE
    );
    assert!(!dir.path().join(".crash_test.zip.termination.tmp").exists());
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
