use crate::pipeline::{CrashEvent, Plugin, PostProcessor, ReportResult, ReportType};
use crate::postprocessors::MoveToSent;
use std::path::PathBuf;

fn make_event() -> CrashEvent {
    CrashEvent {
        report_type: ReportType::Crash,
        termination: None,
        exception_type: None,
        exception_code: None,
        exception_subcode: None,
        crashed_thread: None,
        bail_on_suspend_failure: false,
        pid: 1234,
        process_name: "test".into(),
        hang_duration_ms: None,
    }
}

fn make_result(json_path: PathBuf, raw_path: Option<PathBuf>) -> ReportResult {
    ReportResult {
        raw_path,
        json_path: Some(json_path),
        session: None,
    }
}

/// Lay out a fake report family in `pending` (json + raw + screenshots + zip).
fn write_report_family(pending: &std::path::Path, stem: &str) -> Vec<PathBuf> {
    std::fs::create_dir_all(pending).unwrap();
    let paths = vec![
        pending.join(format!("{stem}.json")),
        pending.join(format!("{stem}_raw.bin")),
        pending.join(format!("{stem}_screenshot_000.png")),
        pending.join(format!("{stem}.zip")),
    ];
    for p in &paths {
        std::fs::write(p, b"x").unwrap();
    }
    paths
}

#[test]
fn test_moves_all_basename_prefixed_files() {
    let tmp = tempfile::tempdir().unwrap();
    let pending = tmp.path().join("pending");
    let sent = tmp.path().join("sent");

    let paths = write_report_family(&pending, "crash_20260524_1234");
    let mover = MoveToSent::with_dir(sent.clone());
    let mut result = make_result(paths[0].clone(), Some(paths[1].clone()));
    mover.process(&make_event(), &mut result).unwrap();

    // Original files gone from pending
    for p in &paths {
        assert!(
            !p.exists(),
            "{} should be moved out of pending",
            p.display()
        );
    }
    // All present in sent
    for name in [
        "crash_20260524_1234.json",
        "crash_20260524_1234_raw.bin",
        "crash_20260524_1234_screenshot_000.png",
        "crash_20260524_1234.zip",
    ] {
        assert!(sent.join(name).exists(), "{name} should be in sent");
    }
    // json_path + raw_path patched to new location
    assert_eq!(
        result.json_path.unwrap(),
        sent.join("crash_20260524_1234.json")
    );
    assert_eq!(
        result.raw_path.unwrap(),
        sent.join("crash_20260524_1234_raw.bin")
    );
}

#[test]
fn test_does_not_move_other_basenames() {
    let tmp = tempfile::tempdir().unwrap();
    let pending = tmp.path().join("pending");
    let sent = tmp.path().join("sent");
    write_report_family(&pending, "crash_a_1");
    let other = write_report_family(&pending, "crash_b_2");

    let mover = MoveToSent::with_dir(sent.clone());
    let mut result = make_result(pending.join("crash_a_1.json"), None);
    mover.process(&make_event(), &mut result).unwrap();

    for p in &other {
        assert!(
            p.exists(),
            "{} should NOT be moved (other basename)",
            p.display()
        );
    }
    assert!(!sent.join("crash_b_2.json").exists());
}

#[test]
fn test_avoids_partial_prefix_collision() {
    // basename "crash_..._1234" must not match "crash_..._12345".
    let tmp = tempfile::tempdir().unwrap();
    let pending = tmp.path().join("pending");
    let sent = tmp.path().join("sent");
    std::fs::create_dir_all(&pending).unwrap();
    std::fs::write(pending.join("crash_a_1234.json"), b"x").unwrap();
    let neighbor = pending.join("crash_a_12345.json");
    std::fs::write(&neighbor, b"y").unwrap();

    let mover = MoveToSent::with_dir(sent.clone());
    let mut result = make_result(pending.join("crash_a_1234.json"), None);
    mover.process(&make_event(), &mut result).unwrap();

    assert!(sent.join("crash_a_1234.json").exists());
    assert!(
        neighbor.exists(),
        "12345 must not be matched by 1234 prefix"
    );
}

#[test]
fn test_no_json_path_is_noop() {
    let mover = MoveToSent::with_dir(std::env::temp_dir().join("never"));
    let mut result = ReportResult {
        raw_path: None,
        json_path: None,
        session: None,
    };
    mover.process(&make_event(), &mut result).unwrap();
}

#[test]
fn test_plugin_metadata() {
    let mover = MoveToSent::new();
    assert_eq!(mover.name(), "MoveToSent");
    assert!(mover.is_available());
}
