use super::{MAX_MOVE_FILE_BYTES, STREAM_BUFFER_BYTES, move_file_with, move_file_with_operations};
use crate::pipeline::{CrashEvent, Plugin, PluginContext, PostProcessor, ReportResult, ReportType};
use crate::postprocessors::MoveToSent;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

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

fn make_result(json_path: PathBuf, raw_path: Option<PathBuf>) -> ReportResult {
    ReportResult {
        artifact_paths: Vec::new(),
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
fn test_moves_only_exact_registered_artifacts() {
    let tmp = tempfile::tempdir().unwrap();
    let pending = tmp.path().join("pending");
    let sent = tmp.path().join("sent");

    let paths = write_report_family(&pending, "crash_20260524_1234");
    let mover = MoveToSent::with_dir(sent.clone());
    let mut result = make_result(paths[0].clone(), Some(paths[1].clone()));
    result.artifact_paths = paths.clone();
    mover
        .process(
            &make_event(),
            &mut result,
            &PluginContext::without_deadline(),
        )
        .unwrap();

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
        let destination = sent.join(name);
        assert!(destination.exists(), "{name} should be in sent");
        assert_eq!(
            std::fs::metadata(destination).unwrap().permissions().mode() & 0o7777,
            crate::utils::paths::PRIVATE_FILE_MODE
        );
    }
    assert_eq!(
        std::fs::metadata(&sent).unwrap().permissions().mode() & 0o7777,
        crate::utils::paths::PRIVATE_DIRECTORY_MODE
    );
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
    mover
        .process(
            &make_event(),
            &mut result,
            &PluginContext::without_deadline(),
        )
        .unwrap();

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
    mover
        .process(
            &make_event(),
            &mut result,
            &PluginContext::without_deadline(),
        )
        .unwrap();

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
        artifact_paths: Vec::new(),
        raw_path: None,
        json_path: None,
        session: None,
    };
    mover
        .process(
            &make_event(),
            &mut result,
            &PluginContext::without_deadline(),
        )
        .unwrap();
}

#[test]
fn test_plugin_metadata() {
    let mover = MoveToSent::new();
    assert_eq!(mover.name(), "MoveToSent");
    assert!(mover.is_available());
    assert!(mover.hard_dependencies().is_empty());
    assert_eq!(mover.order_after(), &["ZIPArchiver"]);
}

#[test]
fn test_cancellation_after_json_move_commits_new_result_path() {
    let tmp = tempfile::tempdir().unwrap();
    let pending = tmp.path().join("pending");
    let sent = tmp.path().join("sent");
    let paths = write_report_family(&pending, "crash_cancel_json");
    let json_path = paths[0].clone();
    let mut result = make_result(json_path.clone(), Some(paths[1].clone()));
    let context = PluginContext::without_deadline();
    let cancellation = context.cancellation_token();

    let error = MoveToSent::with_dir(sent.clone())
        .process_with_after_move(&mut result, &context, |source, _| {
            if source == json_path {
                cancellation.cancel();
            }
        })
        .unwrap_err();

    assert_eq!(error, "plugin deadline reached");
    assert_eq!(
        result.json_path.as_deref(),
        Some(sent.join("crash_cancel_json.json").as_path())
    );
    assert!(!json_path.exists());
    assert!(sent.join("crash_cancel_json.json").exists());
}

#[test]
fn test_cancellation_after_raw_move_commits_new_result_path() {
    let tmp = tempfile::tempdir().unwrap();
    let pending = tmp.path().join("pending");
    let sent = tmp.path().join("sent");
    let paths = write_report_family(&pending, "crash_cancel_raw");
    let raw_path = paths[1].clone();
    let mut result = make_result(paths[0].clone(), Some(raw_path.clone()));
    let context = PluginContext::without_deadline();
    let cancellation = context.cancellation_token();

    let error = MoveToSent::with_dir(sent.clone())
        .process_with_after_move(&mut result, &context, |source, _| {
            if source == raw_path {
                cancellation.cancel();
            }
        })
        .unwrap_err();

    assert_eq!(error, "plugin deadline reached");
    assert_eq!(
        result.raw_path.as_deref(),
        Some(sent.join("crash_cancel_raw_raw.bin").as_path())
    );
    assert!(!raw_path.exists());
    assert!(sent.join("crash_cancel_raw_raw.bin").exists());
}

#[test]
fn test_failed_json_move_does_not_patch_result_path() {
    let tmp = tempfile::tempdir().unwrap();
    let pending = tmp.path().join("pending");
    let sent = tmp.path().join("sent");
    let paths = write_report_family(&pending, "crash_move_failure");
    let json_path = paths[0].clone();
    std::fs::create_dir_all(sent.join("crash_move_failure.json")).unwrap();
    let mut result = make_result(json_path.clone(), Some(paths[1].clone()));

    MoveToSent::with_dir(sent)
        .process(
            &make_event(),
            &mut result,
            &PluginContext::without_deadline(),
        )
        .unwrap();

    assert_eq!(result.json_path.as_deref(), Some(json_path.as_path()));
    assert!(
        json_path.exists(),
        "failed move must retain its source file"
    );
}

#[test]
fn test_non_exdev_rename_error_does_not_fallback_to_copy() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.json");
    let destination = dir.path().join("destination.json");
    std::fs::write(&source, "payload").unwrap();

    let error = move_file_with(
        &source,
        &destination,
        &PluginContext::without_deadline(),
        |_, _| Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied)),
        || {},
        || {},
    )
    .unwrap_err();

    assert!(error.contains("rename"));
    assert!(source.exists());
    assert!(!destination.exists());
}

#[test]
fn test_exdev_fallback_streams_to_tmp_then_publishes() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.json");
    let destination = dir.path().join("destination.json");
    let payload = vec![0x5a; STREAM_BUFFER_BYTES * 2 + 17];
    std::fs::write(&source, &payload).unwrap();
    let mut chunks = 0_usize;

    move_file_with(
        &source,
        &destination,
        &PluginContext::without_deadline(),
        |_, _| Err(std::io::Error::from_raw_os_error(nix::libc::EXDEV)),
        || chunks += 1,
        || {},
    )
    .unwrap();

    assert_eq!(chunks, 3, "copy must proceed in fixed-size chunks");
    assert!(!source.exists());
    assert_eq!(std::fs::read(&destination).unwrap(), payload);
    assert_eq!(
        std::fs::metadata(&destination)
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        crate::utils::paths::PRIVATE_FILE_MODE
    );
    assert_no_move_temporary_files(dir.path());
}

#[test]
fn test_exdev_fallback_rejects_source_growth_after_copy() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.json");
    let destination = dir.path().join("destination.json");
    let payload = b"original payload";
    std::fs::write(&source, payload).unwrap();
    let source_for_hook = source.clone();

    let error = move_file_with(
        &source,
        &destination,
        &PluginContext::without_deadline(),
        |_, _| Err(std::io::Error::from_raw_os_error(nix::libc::EXDEV)),
        || {},
        || {
            std::fs::OpenOptions::new()
                .append(true)
                .open(source_for_hook)
                .unwrap()
                .write_all(b" grew")
                .unwrap();
        },
    )
    .unwrap_err();

    assert!(error.contains("source changed during copy"));
    assert_eq!(std::fs::read(&source).unwrap(), b"original payload grew");
    assert!(!destination.exists());
    assert_no_move_temporary_files(dir.path());
}

#[test]
fn test_exdev_fallback_cancellation_removes_tmp_and_preserves_source() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.json");
    let destination = dir.path().join("destination.json");
    let payload = vec![0x3c; STREAM_BUFFER_BYTES * 2];
    std::fs::write(&source, &payload).unwrap();
    let context = PluginContext::without_deadline();
    let cancellation = context.cancellation_token();

    let error = move_file_with(
        &source,
        &destination,
        &context,
        |_, _| Err(std::io::Error::from_raw_os_error(nix::libc::EXDEV)),
        || cancellation.cancel(),
        || {},
    )
    .unwrap_err();

    assert_eq!(error, "plugin deadline reached");
    assert_eq!(std::fs::read(&source).unwrap(), payload);
    assert!(!destination.exists());
    assert_no_move_temporary_files(dir.path());
}

#[test]
fn test_move_rejects_oversized_file_before_rename() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.json");
    let destination = dir.path().join("destination.json");
    std::fs::File::create(&source)
        .unwrap()
        .set_len(MAX_MOVE_FILE_BYTES + 1)
        .unwrap();

    let error = move_file_with(
        &source,
        &destination,
        &PluginContext::without_deadline(),
        |_, _| panic!("rename must not run for an oversized source"),
        || {},
        || {},
    )
    .unwrap_err();

    assert!(error.contains("exceeds move limit"));
    assert!(source.exists());
    assert!(!destination.exists());
}

#[test]
fn test_move_rejects_symlink_before_rename() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target.json");
    let source = dir.path().join("source.json");
    let destination = dir.path().join("destination.json");
    std::fs::write(&target, "payload").unwrap();
    std::os::unix::fs::symlink(&target, &source).unwrap();

    let error = move_file_with(
        &source,
        &destination,
        &PluginContext::without_deadline(),
        |_, _| panic!("rename must not run for a symlink"),
        || {},
        || {},
    )
    .unwrap_err();

    assert!(error.contains("not a regular file"));
    assert!(source.exists());
    assert!(!destination.exists());
    assert_eq!(std::fs::read_to_string(target).unwrap(), "payload");
}

#[test]
fn test_successful_rename_is_not_rolled_back_by_directory_sync_failure() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.json");
    let destination = dir.path().join("destination.json");
    std::fs::write(&source, b"payload").unwrap();

    move_file_with_operations(
        &source,
        &destination,
        &PluginContext::without_deadline(),
        |source, destination| std::fs::rename(source, destination),
        (|| {}, || {}),
        |_, _| Err("injected directory sync failure".into()),
        |_| panic!("same-filesystem move must not call the copy cleanup"),
    )
    .unwrap();

    assert!(!source.exists());
    assert_eq!(std::fs::read(destination).unwrap(), b"payload");
}

#[test]
fn test_exdev_published_destination_stays_canonical_when_source_cleanup_fails() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source.json");
    let destination = dir.path().join("destination.json");
    std::fs::write(&source, b"payload").unwrap();

    move_file_with_operations(
        &source,
        &destination,
        &PluginContext::without_deadline(),
        |_, _| Err(std::io::Error::from_raw_os_error(nix::libc::EXDEV)),
        (|| {}, || {}),
        |_, _| Err("injected directory sync failure".into()),
        |_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "injected source cleanup failure",
            ))
        },
    )
    .unwrap();

    assert_eq!(std::fs::read(&destination).unwrap(), b"payload");
    assert_eq!(std::fs::read(&source).unwrap(), b"payload");
    assert_no_move_temporary_files(dir.path());
}

fn assert_no_move_temporary_files(dir: &std::path::Path) {
    let has_temporary_file = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .any(|entry| entry.file_name().to_string_lossy().contains(".move-"));
    assert!(!has_temporary_file, "move temporary file leaked");
}
