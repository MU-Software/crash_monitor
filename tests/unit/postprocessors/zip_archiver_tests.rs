use super::{
    MAX_ARCHIVE_ENTRIES, MAX_ARCHIVE_FILE_BYTES, MAX_ARCHIVE_TOTAL_BYTES, STREAM_BUFFER_BYTES,
};
use crate::pipeline::{
    ArtifactKind, ArtifactTransaction, CrashEvent, Plugin, PluginContext, PostProcessor,
    ReportContext, ReportResult, ReportType,
};
use crate::postprocessors::ZIPArchiver;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

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
#[ignore = "spawned with an isolated process-global umask"]
fn zip_writer_private_mode_under_subprocess_umask_helper() {
    let Ok(mask) = std::env::var("CRASH_MONITOR_P014_WRITER_UMASK") else {
        return;
    };
    let root = PathBuf::from(std::env::var_os("CRASH_MONITOR_P014_WRITER_ROOT").unwrap());
    let mask = nix::libc::mode_t::from_str_radix(&mask, 8).unwrap();
    // SAFETY: only this ignored helper runs in the dedicated child process.
    unsafe {
        nix::libc::umask(mask);
    }

    let json_path = root.join("report.json");
    let mut result = ReportResult {
        artifact_paths: vec![json_path.clone()],
        raw_path: None,
        json_path: Some(json_path),
        session: None,
    };
    ZIPArchiver
        .process(
            &dummy_event(),
            &mut result,
            &PluginContext::without_deadline(),
        )
        .unwrap();
    assert_eq!(
        std::fs::metadata(root.join("report.zip"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        crate::utils::paths::PRIVATE_FILE_MODE
    );
}

#[test]
fn zip_writer_is_private_under_permissive_and_restrictive_umasks() {
    for mask in ["000", "777"] {
        let root = tempfile::tempdir().unwrap();
        let json_path = root.path().join("report.json");
        std::fs::write(&json_path, b"{}").unwrap();
        std::fs::set_permissions(&json_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "postprocessors::zip_archiver::tests::zip_writer_private_mode_under_subprocess_umask_helper",
                "--ignored",
                "--nocapture",
            ])
            .env("CRASH_MONITOR_P014_WRITER_UMASK", mask)
            .env("CRASH_MONITOR_P014_WRITER_ROOT", root.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "ZIP writer under umask {mask} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
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
        artifact_paths: vec![json_path.clone(), png_path],
        raw_path: None,
        json_path: Some(json_path),
        session: None,
    };

    let archiver = ZIPArchiver;
    archiver
        .process(
            &dummy_event(),
            &mut result,
            &PluginContext::without_deadline(),
        )
        .unwrap();

    let zip_path = dir.path().join("crash_20260411_120000_1234.zip");
    assert!(zip_path.exists(), "ZIP file should be created");
    assert_eq!(
        std::fs::metadata(&zip_path).unwrap().permissions().mode() & 0o7777,
        crate::utils::paths::PRIVATE_FILE_MODE
    );
    assert_eq!(result.json_path.as_deref(), Some(zip_path.as_path()));
}

#[test]
fn test_zip_directory_sync_failure_preserves_exact_transaction_manifest() {
    let root = tempfile::tempdir().unwrap();
    let event = dummy_event();
    let transaction = ArtifactTransaction::begin(ReportContext::new(&event, root.path())).unwrap();
    let json_path = transaction
        .write_bytes("report.json", ArtifactKind::Report, b"{}")
        .unwrap();
    let mut result = ReportResult {
        artifact_paths: vec![json_path.clone()],
        raw_path: None,
        json_path: Some(json_path),
        session: None,
    };
    let context = PluginContext::without_deadline().with_artifact_transaction(transaction.clone());

    ZIPArchiver::process_with_directory_sync(&mut result, &context, |_| {
        Err("injected ZIP directory sync failure".into())
    })
    .unwrap();

    let committed = transaction.commit().unwrap();
    assert!(
        committed
            .durability_warnings
            .iter()
            .any(|warning| warning.contains("injected ZIP directory sync failure"))
    );
    let manifest = crate::pipeline::load_manifest(&committed.manifest_path).unwrap();
    assert_eq!(manifest.artifacts.len(), 1);
    assert_eq!(manifest.artifacts[0].path, "report.zip");
    assert_eq!(manifest.artifacts[0].kind, ArtifactKind::Archive);
    assert!(committed.report_dir.join("report.zip").is_file());
    assert!(!committed.report_dir.join("report.json").exists());
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
        artifact_paths: vec![json_path.clone(), png_path],
        raw_path: None,
        json_path: Some(json_path),
        session: None,
    };

    ZIPArchiver
        .process(
            &dummy_event(),
            &mut result,
            &PluginContext::without_deadline(),
        )
        .unwrap();

    let zip_path = dir.path().join("crash_20260411_120000_5678.zip");
    let file = std::fs::File::open(&zip_path).unwrap();
    let mut archive = zip::ZipArchive::new(file).unwrap();

    let mut names: Vec<String> = (0..archive.len())
        .map(|i| {
            let entry = archive.by_index(i).unwrap();
            assert_eq!(entry.unix_mode().map(|mode| mode & 0o7777), Some(0o600));
            entry.name().to_string()
        })
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
        artifact_paths: Vec::new(),
        raw_path: None,
        json_path: None,
        session: None,
    };
    assert!(
        ZIPArchiver
            .process(
                &dummy_event(),
                &mut result,
                &PluginContext::without_deadline(),
            )
            .is_ok()
    );
}

#[test]
fn test_originals_deleted_after_zip() {
    let dir = tempfile::tempdir().unwrap();
    let json_path = dir.path().join("snap_20260411_120000_9999.json");
    std::fs::write(&json_path, "{}").unwrap();

    let mut result = ReportResult {
        artifact_paths: vec![json_path.clone()],
        raw_path: None,
        json_path: Some(json_path.clone()),
        session: None,
    };

    ZIPArchiver
        .process(
            &dummy_event(),
            &mut result,
            &PluginContext::without_deadline(),
        )
        .unwrap();
    let zip_path = dir.path().join("snap_20260411_120000_9999.zip");
    assert!(!json_path.exists(), "original JSON should be deleted");
    assert!(zip_path.exists(), "ZIP should exist");
    assert_eq!(result.json_path.as_deref(), Some(zip_path.as_path()));
}

#[test]
fn test_depends_on_feedback() {
    assert_eq!(
        ZIPArchiver.order_after(),
        &["RawCleanup", "PNGConverter", "FeedbackDialog"]
    );
}

#[test]
fn test_plugin_metadata() {
    assert_eq!(ZIPArchiver.name(), "ZIPArchiver");
    assert!(ZIPArchiver.is_available());
}

#[test]
fn test_cancellation_after_zip_publish_commits_canonical_result_path() {
    let dir = tempfile::tempdir().unwrap();
    let json_path = dir.path().join("crash_cancel.json");
    let raw_path = dir.path().join("crash_cancel.raw");
    let png_path = dir.path().join("crash_cancel_screenshot_000.png");
    std::fs::write(&json_path, r#"{"header":{}}"#).unwrap();
    std::fs::write(&raw_path, b"raw report").unwrap();
    std::fs::write(&png_path, b"fakepng").unwrap();
    let mut result = ReportResult {
        artifact_paths: vec![json_path.clone(), raw_path.clone(), png_path.clone()],
        raw_path: Some(raw_path.clone()),
        json_path: Some(json_path.clone()),
        session: None,
    };
    let context = PluginContext::without_deadline();
    let cancellation = context.cancellation_token();

    let error = ZIPArchiver::process_with_after_archive_publish(&mut result, &context, || {
        cancellation.cancel();
    })
    .unwrap_err();

    let zip_path = dir.path().join("crash_cancel.zip");
    assert_eq!(error, "plugin deadline reached");
    assert_eq!(result.json_path.as_deref(), Some(zip_path.as_path()));
    assert_eq!(result.raw_path, None);
    assert!(zip_path.exists(), "published ZIP must remain canonical");
    assert!(
        json_path.exists() && raw_path.exists() && png_path.exists(),
        "cancellation may leave originals, but must not invalidate the ZIP"
    );
    let archive = zip::ZipArchive::new(std::fs::File::open(zip_path).unwrap()).unwrap();
    assert_eq!(archive.len(), 3);
}

#[test]
fn test_cancellation_after_zip_finalize_removes_owned_temporary_file() {
    let dir = tempfile::tempdir().unwrap();
    let json_path = dir.path().join("crash_finalize_cancel.json");
    let raw_path = dir.path().join("crash_finalize_cancel.raw");
    std::fs::write(&json_path, r#"{"header":{}}"#).unwrap();
    std::fs::write(&raw_path, b"raw report").unwrap();
    let mut result = ReportResult {
        artifact_paths: vec![json_path.clone(), raw_path.clone()],
        raw_path: Some(raw_path.clone()),
        json_path: Some(json_path.clone()),
        session: None,
    };
    let context = PluginContext::without_deadline();
    let cancellation = context.cancellation_token();

    let error = ZIPArchiver::process_with_after_archive_finalize(&mut result, &context, || {
        cancellation.cancel();
    })
    .unwrap_err();

    assert_eq!(error, "plugin deadline reached");
    assert_eq!(result.json_path.as_deref(), Some(json_path.as_path()));
    assert_eq!(result.raw_path.as_deref(), Some(raw_path.as_path()));
    assert!(json_path.exists());
    assert!(raw_path.exists());
    assert!(!dir.path().join("crash_finalize_cancel.zip").exists());
    let leaked_tmp = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(Result::ok)
        .any(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with(".crash_finalize_cancel.zip-") && name.ends_with(".tmp")
        });
    assert!(!leaked_tmp, "finalized ZIP temporary file leaked");
}

#[test]
fn test_archiving_raw_file_clears_raw_path() {
    let dir = tempfile::tempdir().unwrap();
    let json_path = dir.path().join("crash_with_raw.json");
    let raw_path = dir.path().join("crash_with_raw.raw");
    std::fs::write(&json_path, "{}").unwrap();
    std::fs::write(&raw_path, b"raw report").unwrap();
    let mut result = ReportResult {
        artifact_paths: vec![json_path.clone(), raw_path.clone()],
        raw_path: Some(raw_path.clone()),
        json_path: Some(json_path),
        session: None,
    };

    ZIPArchiver
        .process(
            &dummy_event(),
            &mut result,
            &PluginContext::without_deadline(),
        )
        .unwrap();

    assert_eq!(result.raw_path, None);
    let zip_path = dir.path().join("crash_with_raw.zip");
    let mut archive = zip::ZipArchive::new(std::fs::File::open(zip_path).unwrap()).unwrap();
    assert!(archive.by_name("crash_with_raw.raw").is_ok());
}

#[test]
fn test_zip_publish_failure_preserves_original_result_path() {
    let dir = tempfile::tempdir().unwrap();
    let json_path = dir.path().join("crash_publish_failure.json");
    std::fs::write(&json_path, r#"{"header":{}}"#).unwrap();
    // Renaming a file over a directory fails deterministically.
    std::fs::create_dir(dir.path().join("crash_publish_failure.zip")).unwrap();
    let mut result = ReportResult {
        artifact_paths: vec![json_path.clone()],
        raw_path: None,
        json_path: Some(json_path.clone()),
        session: None,
    };

    let error = ZIPArchiver
        .process(
            &dummy_event(),
            &mut result,
            &PluginContext::without_deadline(),
        )
        .unwrap_err();

    assert!(error.contains("ZIP rename failed"));
    assert_eq!(result.json_path.as_deref(), Some(json_path.as_path()));
    assert!(json_path.exists());
    assert!(!dir.path().join("crash_publish_failure.zip.tmp").exists());
}

#[test]
fn test_rejects_too_many_matching_entries_before_writing_zip() {
    let dir = tempfile::tempdir().unwrap();
    let json_path = dir.path().join("crash_many.json");
    std::fs::write(&json_path, "{}").unwrap();
    let mut artifact_paths = vec![json_path.clone()];
    for index in 0..MAX_ARCHIVE_ENTRIES {
        let path = dir.path().join(format!("crash_many_{index:03}.log"));
        std::fs::write(&path, []).unwrap();
        artifact_paths.push(path);
    }
    let mut result = ReportResult {
        artifact_paths,
        raw_path: None,
        json_path: Some(json_path.clone()),
        session: None,
    };

    let error = ZIPArchiver
        .process(
            &dummy_event(),
            &mut result,
            &PluginContext::without_deadline(),
        )
        .unwrap_err();

    assert!(error.contains("archive entry limit"));
    assert_eq!(result.json_path.as_deref(), Some(json_path.as_path()));
    assert!(!dir.path().join("crash_many.zip").exists());
}

#[test]
fn test_rejects_oversized_regular_file_before_writing_zip() {
    let dir = tempfile::tempdir().unwrap();
    let json_path = dir.path().join("crash_large.json");
    let file = std::fs::File::create(&json_path).unwrap();
    file.set_len(MAX_ARCHIVE_FILE_BYTES + 1).unwrap();
    let mut result = ReportResult {
        artifact_paths: vec![json_path.clone()],
        raw_path: None,
        json_path: Some(json_path.clone()),
        session: None,
    };

    let error = ZIPArchiver
        .process(
            &dummy_event(),
            &mut result,
            &PluginContext::without_deadline(),
        )
        .unwrap_err();

    assert!(error.contains("per-file limit"));
    assert_eq!(result.json_path.as_deref(), Some(json_path.as_path()));
    assert!(!dir.path().join("crash_large.zip").exists());
}

#[test]
fn test_rejects_archive_family_over_total_byte_limit() {
    let dir = tempfile::tempdir().unwrap();
    let json_path = dir.path().join("crash_total.json");
    let sparse_size = MAX_ARCHIVE_TOTAL_BYTES / 3 + 1;
    let artifact_paths = [
        json_path.clone(),
        dir.path().join("crash_total_a.bin"),
        dir.path().join("crash_total_b.bin"),
    ];
    for path in &artifact_paths {
        std::fs::File::create(path)
            .unwrap()
            .set_len(sparse_size)
            .unwrap();
    }
    let mut result = ReportResult {
        artifact_paths: artifact_paths.to_vec(),
        raw_path: None,
        json_path: Some(json_path.clone()),
        session: None,
    };

    let error = ZIPArchiver
        .process(
            &dummy_event(),
            &mut result,
            &PluginContext::without_deadline(),
        )
        .unwrap_err();

    assert!(error.contains("total archive limit"));
    assert_eq!(result.json_path.as_deref(), Some(json_path.as_path()));
    assert!(!dir.path().join("crash_total.zip").exists());
}

#[test]
fn test_skips_matching_symlink_instead_of_following_it() {
    let dir = tempfile::tempdir().unwrap();
    let json_path = dir.path().join("crash_symlink.json");
    let outside = dir.path().join("outside.log");
    let link = dir.path().join("crash_symlink_attachment.log");
    std::fs::write(&json_path, "{}").unwrap();
    std::fs::write(&outside, "secret").unwrap();
    std::os::unix::fs::symlink(&outside, &link).unwrap();
    let mut result = ReportResult {
        artifact_paths: vec![json_path.clone()],
        raw_path: None,
        json_path: Some(json_path),
        session: None,
    };

    ZIPArchiver
        .process(
            &dummy_event(),
            &mut result,
            &PluginContext::without_deadline(),
        )
        .unwrap();

    let zip_path = dir.path().join("crash_symlink.zip");
    let mut archive = zip::ZipArchive::new(std::fs::File::open(zip_path).unwrap()).unwrap();
    assert_eq!(archive.len(), 1);
    assert_eq!(archive.by_index(0).unwrap().name(), "crash_symlink.json");
    assert!(link.exists(), "skipped symlink must not be deleted");
    assert_eq!(std::fs::read_to_string(outside).unwrap(), "secret");
}

#[test]
fn test_cancellation_between_stream_chunks_removes_partial_zip() {
    let dir = tempfile::tempdir().unwrap();
    let json_path = dir.path().join("crash_stream.json");
    let payload = vec![0x7b; STREAM_BUFFER_BYTES * 2];
    std::fs::write(&json_path, &payload).unwrap();
    let mut result = ReportResult {
        artifact_paths: vec![json_path.clone()],
        raw_path: None,
        json_path: Some(json_path.clone()),
        session: None,
    };
    let context = PluginContext::without_deadline();
    let cancellation = context.cancellation_token();

    let error = ZIPArchiver::process_with_after_archive_chunk(&mut result, &context, || {
        cancellation.cancel();
    })
    .unwrap_err();

    assert!(error.contains("plugin deadline reached"));
    assert_eq!(result.json_path.as_deref(), Some(json_path.as_path()));
    assert_eq!(std::fs::read(&json_path).unwrap(), payload);
    assert!(!dir.path().join("crash_stream.zip").exists());
    let leaked_tmp = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(Result::ok)
        .any(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with(".crash_stream.zip-") && name.ends_with(".tmp")
        });
    assert!(!leaked_tmp, "partial ZIP temporary file leaked");
}
