use super::encode_png;
use crate::pipeline::report::{
    self, CrashReport, ExceptionReport, HeapSummary, LoadedImageReport, ReportHeader, ThreadReport,
    VmRegionReport,
};
use crate::pipeline::{
    ArtifactKind, ArtifactTransaction, CrashEvent, Plugin, PluginContext, PostProcessor,
    ReportContext, ReportResult, ReportType,
};
use crate::postprocessors::PNGConverter;
use nix::sys::stat::Mode;
use nix::unistd::mkfifo;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::PathBuf;
use std::time::{Duration, Instant};

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

fn empty_report() -> CrashReport {
    CrashReport {
        header: ReportHeader {
            version: 1,
            report_id: Option::default(),
            timestamp: "2026-05-24T00:00:00".into(),
            pid: 1234,
            process: "test".into(),
            collector: "test".into(),
            report_type: ReportType::Crash,
            trigger: None,
            hang_duration_ms: None,
        },
        termination: None,
        build: None,
        exception: None::<ExceptionReport>,
        crash_context: None,
        threads: Vec::<ThreadReport>::new(),
        breadcrumbs: None,
        loaded_images: Vec::<LoadedImageReport>::new(),
        memory_map: Vec::<VmRegionReport>::new(),
        heap_summary: None::<HeapSummary>,
        session: None,
        settings_snapshot: None,
        fingerprint: None,
        environment: None,
        process_output: None,
        attachments: vec![],
        user_feedback: None,
        diagnostics: None,
    }
}

fn write_report_with_rgba_attachments(
    dir: &std::path::Path,
    rgba_files: &[(&str, u32, u32, Vec<u8>)],
) -> PathBuf {
    let mut r = empty_report();
    for (file_name, w, h, bytes) in rgba_files {
        std::fs::write(dir.join(file_name), bytes).unwrap();
        r.attachments.push(serde_json::json!({
            "label": file_name.trim_end_matches(".rgba"),
            "file": file_name,
            "format": "rgba",
            "width": w,
            "height": h,
            "size": bytes.len() as u64,
        }));
    }
    let json_path = dir.join("report.json");
    std::fs::write(&json_path, serde_json::to_string_pretty(&r).unwrap()).unwrap();
    json_path
}

fn make_result(json_path: PathBuf) -> ReportResult {
    ReportResult {
        artifact_paths: Vec::new(),
        raw_path: None,
        json_path: Some(json_path),
        session: None,
    }
}

#[test]
fn test_encode_png_round_trip() {
    let pixel = [255u8, 0, 0, 255];
    let rgba: Vec<u8> = pixel.iter().copied().cycle().take(4 * 4 * 4).collect();
    let png_data = encode_png(&rgba, 4, 4).expect("encode_png should succeed");
    assert!(png_data.len() >= 8);
    assert_eq!(&png_data[..4], &[0x89, 0x50, 0x4E, 0x47]);
}

#[test]
fn test_encode_png_real_size() {
    let rgba = vec![0u8; 480 * 270 * 4];
    let png_data = encode_png(&rgba, 480, 270).expect("encode_png should succeed");
    assert!(png_data.len() >= 8);
    assert_eq!(&png_data[..4], &[0x89, 0x50, 0x4E, 0x47]);
}

#[test]
fn test_encode_png_zero_dimension() {
    let result = encode_png(&[], 0, 0);
    assert!(result.is_err(), "zero dimensions should return Err");
}

#[test]
fn test_encode_png_rejects_mismatched_and_unbounded_inputs() {
    assert!(encode_png(&[0_u8; 4], 2, 2).is_err());
    assert!(encode_png(&[], super::MAX_RGBA_DIMENSION + 1, 1).is_err());
    assert!(
        encode_png(&[], super::MAX_RGBA_DIMENSION, super::MAX_RGBA_DIMENSION).is_err(),
        "pixel count must also respect the byte cap"
    );
}

#[test]
fn test_converts_rgba_to_png_and_removes_original() {
    let dir = tempfile::tempdir().unwrap();
    let rgba_bytes: Vec<u8> = std::iter::repeat([0u8, 255, 0, 255])
        .flatten()
        .take(4 * 4 * 4)
        .collect();
    let json_path = write_report_with_rgba_attachments(
        dir.path(),
        &[("a_screenshot_000.rgba", 4, 4, rgba_bytes)],
    );

    PNGConverter
        .process(
            &make_event(),
            &mut make_result(json_path.clone()),
            &PluginContext::without_deadline(),
        )
        .unwrap();

    assert!(
        !dir.path().join("a_screenshot_000.rgba").exists(),
        ".rgba should be deleted after PNG conversion"
    );
    assert!(
        dir.path().join("a_screenshot_000.png").exists(),
        ".png should be created"
    );
    for path in [&json_path, &dir.path().join("a_screenshot_000.png")] {
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o7777,
            crate::utils::paths::PRIVATE_FILE_MODE
        );
    }

    let updated = report::load_report(&json_path).unwrap();
    assert_eq!(updated.attachments.len(), 1);
    let a = &updated.attachments[0];
    assert_eq!(a["format"], "png");
    assert_eq!(a["file"], "a_screenshot_000.png");
    assert!(a["size"].as_u64().unwrap() > 0);
    assert!(
        a.get("width").is_none() && a.get("height").is_none(),
        "width/height should be dropped for PNG (self-describing)"
    );
}

#[test]
fn test_report_fifo_and_symlink_are_rejected_without_blocking() {
    let fifo_dir = tempfile::tempdir().unwrap();
    let fifo_path = fifo_dir.path().join("report.json");
    mkfifo(&fifo_path, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
    let started = Instant::now();
    let error = PNGConverter
        .process(
            &make_event(),
            &mut make_result(fifo_path),
            &PluginContext::without_deadline(),
        )
        .unwrap_err();
    assert!(error.contains("not a regular file"));
    assert!(started.elapsed() < Duration::from_secs(1));

    let symlink_dir = tempfile::tempdir().unwrap();
    let rgba_path = symlink_dir.path().join("image.rgba");
    let report_path = write_report_with_rgba_attachments(
        symlink_dir.path(),
        &[("image.rgba", 1, 1, vec![0_u8; 4])],
    );
    let real_report = symlink_dir.path().join("real-report.json");
    std::fs::rename(&report_path, &real_report).unwrap();
    symlink(&real_report, &report_path).unwrap();

    let error = PNGConverter
        .process(
            &make_event(),
            &mut make_result(report_path),
            &PluginContext::without_deadline(),
        )
        .unwrap_err();
    assert!(error.contains("not private") || error.contains("safely open private"));
    assert!(rgba_path.exists());
    assert!(!symlink_dir.path().join("image.png").exists());
}

#[test]
fn test_existing_png_symlink_blocks_publication_without_following_it() {
    let dir = tempfile::tempdir().unwrap();
    let outside = dir.path().join("outside.txt");
    std::fs::write(&outside, b"do not overwrite").unwrap();
    let png_path = dir.path().join("image.png");
    symlink(&outside, &png_path).unwrap();
    let json_path =
        write_report_with_rgba_attachments(dir.path(), &[("image.rgba", 1, 1, vec![0_u8; 4])]);

    PNGConverter
        .process(
            &make_event(),
            &mut make_result(json_path.clone()),
            &PluginContext::without_deadline(),
        )
        .unwrap();

    assert_eq!(std::fs::read(&outside).unwrap(), b"do not overwrite");
    assert!(
        std::fs::symlink_metadata(&png_path)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        report::load_report(&json_path).unwrap().attachments[0]["file"],
        "image.rgba"
    );
    assert!(dir.path().join("image.rgba").exists());
}

#[test]
fn test_noop_when_no_rgba_attachments() {
    let dir = tempfile::tempdir().unwrap();
    let json_path = write_report_with_rgba_attachments(dir.path(), &[]);
    let before = std::fs::read(&json_path).unwrap();
    PNGConverter
        .process(
            &make_event(),
            &mut make_result(json_path.clone()),
            &PluginContext::without_deadline(),
        )
        .unwrap();
    let after = std::fs::read(&json_path).unwrap();
    assert_eq!(before, after, "no rgba attachments → file unchanged");
}

#[test]
fn test_missing_rgba_file_does_not_panic() {
    let dir = tempfile::tempdir().unwrap();
    // Write report claiming an .rgba attachment but don't create the file.
    let mut r = empty_report();
    r.attachments.push(serde_json::json!({
        "label": "ghost",
        "file": "missing.rgba",
        "format": "rgba",
        "width": 4,
        "height": 4,
        "size": 64,
    }));
    let json_path = dir.path().join("report.json");
    std::fs::write(&json_path, serde_json::to_string_pretty(&r).unwrap()).unwrap();

    // Should succeed (failure-tolerant) and leave attachment unchanged.
    PNGConverter
        .process(
            &make_event(),
            &mut make_result(json_path.clone()),
            &PluginContext::without_deadline(),
        )
        .unwrap();
    let updated = report::load_report(&json_path).unwrap();
    assert_eq!(updated.attachments[0]["format"], "rgba");
}

#[test]
fn test_rgba_size_mismatch_preserves_original_and_report() {
    let dir = tempfile::tempdir().unwrap();
    let json_path =
        write_report_with_rgba_attachments(dir.path(), &[("mismatch.rgba", 1, 1, vec![0_u8; 5])]);

    PNGConverter
        .process(
            &make_event(),
            &mut make_result(json_path.clone()),
            &PluginContext::without_deadline(),
        )
        .unwrap();

    assert!(dir.path().join("mismatch.rgba").exists());
    assert!(!dir.path().join("mismatch.png").exists());
    let report = report::load_report(&json_path).unwrap();
    assert_eq!(report.attachments[0]["format"], "rgba");
    assert_eq!(report.attachments[0]["file"], "mismatch.rgba");
}

#[test]
fn test_non_regular_rgba_input_is_not_converted() {
    let dir = tempfile::tempdir().unwrap();
    let rgba_name = "directory.rgba";
    std::fs::create_dir(dir.path().join(rgba_name)).unwrap();
    let mut report_value = empty_report();
    report_value.attachments.push(serde_json::json!({
        "label": "directory",
        "file": rgba_name,
        "format": "rgba",
        "width": 1,
        "height": 1,
        "size": 4
    }));
    let json_path = dir.path().join("report.json");
    std::fs::write(
        &json_path,
        serde_json::to_vec_pretty(&report_value).unwrap(),
    )
    .unwrap();

    PNGConverter
        .process(
            &make_event(),
            &mut make_result(json_path.clone()),
            &PluginContext::without_deadline(),
        )
        .unwrap();

    assert!(dir.path().join(rgba_name).is_dir());
    assert!(!dir.path().join("directory.png").exists());
    assert_eq!(
        report::load_report(&json_path).unwrap().attachments[0]["format"],
        "rgba"
    );
}

#[test]
fn test_rgba_path_traversal_is_ignored() {
    let root = tempfile::tempdir().unwrap();
    let report_dir = root.path().join("reports");
    std::fs::create_dir(&report_dir).unwrap();
    let outside_rgba = root.path().join("outside.rgba");
    std::fs::write(&outside_rgba, [1_u8, 2, 3, 4]).unwrap();

    let mut report_value = empty_report();
    report_value.attachments.push(serde_json::json!({
        "label": "outside",
        "file": "../outside.rgba",
        "format": "rgba",
        "width": 1,
        "height": 1,
        "size": 4
    }));
    let json_path = report_dir.join("report.json");
    std::fs::write(
        &json_path,
        serde_json::to_vec_pretty(&report_value).unwrap(),
    )
    .unwrap();

    PNGConverter
        .process(
            &make_event(),
            &mut make_result(json_path.clone()),
            &PluginContext::without_deadline(),
        )
        .unwrap();

    assert_eq!(std::fs::read(&outside_rgba).unwrap(), [1_u8, 2, 3, 4]);
    assert!(!root.path().join("outside.png").exists());
    let unchanged = report::load_report(&json_path).unwrap();
    assert_eq!(unchanged.attachments[0]["file"], "../outside.rgba");
    assert_eq!(unchanged.attachments[0]["format"], "rgba");
}

#[test]
fn test_no_json_path_is_noop() {
    let mut result = ReportResult {
        artifact_paths: Vec::new(),
        raw_path: None,
        json_path: None,
        session: None,
    };
    PNGConverter
        .process(
            &make_event(),
            &mut result,
            &PluginContext::without_deadline(),
        )
        .unwrap();
}

#[test]
fn test_plugin_metadata() {
    assert_eq!(PNGConverter.name(), "PNGConverter");
    assert!(PNGConverter.is_available());
}

#[test]
fn test_multiple_screenshots_all_converted() {
    let dir = tempfile::tempdir().unwrap();
    let rgba = vec![0u8; 2 * 2 * 4];
    let json_path = write_report_with_rgba_attachments(
        dir.path(),
        &[
            ("a_screenshot_000.rgba", 2, 2, rgba.clone()),
            ("a_screenshot_001.rgba", 2, 2, rgba.clone()),
            ("a_screenshot_002.rgba", 2, 2, rgba.clone()),
        ],
    );

    PNGConverter
        .process(
            &make_event(),
            &mut make_result(json_path.clone()),
            &PluginContext::without_deadline(),
        )
        .unwrap();

    for i in 0..3 {
        assert!(
            !dir.path()
                .join(format!("a_screenshot_{i:03}.rgba"))
                .exists(),
            ".rgba {i} should be removed"
        );
        assert!(
            dir.path().join(format!("a_screenshot_{i:03}.png")).exists(),
            ".png {i} should be present"
        );
    }
    let updated = report::load_report(&json_path).unwrap();
    for a in &updated.attachments {
        assert_eq!(a["format"], "png");
    }
}

#[test]
fn test_cancellation_after_png_write_commits_json_before_returning() {
    let dir = tempfile::tempdir().unwrap();
    let rgba = vec![0_u8; 2 * 2 * 4];
    let rgba_path = dir.path().join("report_screenshot_000.rgba");
    let png_path = dir.path().join("report_screenshot_000.png");
    let json_path = write_report_with_rgba_attachments(
        dir.path(),
        &[("report_screenshot_000.rgba", 2, 2, rgba)],
    );
    let mut result = make_result(json_path.clone());
    let context = PluginContext::without_deadline();
    let cancellation = context.cancellation_token();

    let error = PNGConverter::process_with_after_png_write(&mut result, &context, || {
        cancellation.cancel();
    })
    .unwrap_err();

    assert_eq!(error, "plugin deadline reached");
    assert!(png_path.exists(), "published PNG must remain available");
    assert!(
        !rgba_path.exists(),
        "RGBA is removed only after the JSON commit succeeds"
    );
    let updated = report::load_report(&json_path).unwrap();
    assert_eq!(updated.attachments[0]["file"], "report_screenshot_000.png");
    assert_eq!(updated.attachments[0]["format"], "png");
    let leaked_temp = std::fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .any(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.starts_with(".report.json.png-converter-") && name.ends_with(".tmp")
        });
    assert!(
        !leaked_temp,
        "atomic replacement must not leak temporary files"
    );
}

#[test]
fn test_png_directory_sync_failure_preserves_exact_transaction_manifest() {
    let root = tempfile::tempdir().unwrap();
    let event = make_event();
    let transaction = ArtifactTransaction::begin(ReportContext::new(&event, root.path())).unwrap();
    let staging = transaction.staging_dir().to_path_buf();
    let rgba_path = staging.join("report_screenshot_000.rgba");
    let json_path = write_report_with_rgba_attachments(
        &staging,
        &[("report_screenshot_000.rgba", 2, 2, vec![0_u8; 16])],
    );
    transaction
        .register_file(&json_path, ArtifactKind::Report)
        .unwrap();
    transaction
        .register_file(&rgba_path, ArtifactKind::ScreenshotRgba)
        .unwrap();
    let mut result = ReportResult {
        artifact_paths: vec![json_path.clone(), rgba_path],
        raw_path: None,
        json_path: Some(json_path),
        session: None,
    };
    let context = PluginContext::without_deadline().with_artifact_transaction(transaction.clone());

    PNGConverter::process_with_directory_sync(&mut result, &context, |_| {
        Err("injected PNG directory sync failure".into())
    })
    .unwrap();

    let committed = transaction.commit().unwrap();
    assert!(
        committed
            .durability_warnings
            .iter()
            .any(|warning| warning.contains("injected PNG directory sync failure"))
    );
    let manifest = crate::pipeline::load_manifest(&committed.manifest_path).unwrap();
    assert_eq!(
        manifest
            .artifacts
            .iter()
            .map(|artifact| (artifact.path.as_str(), artifact.kind))
            .collect::<Vec<_>>(),
        vec![
            ("report.json", ArtifactKind::Report),
            ("report_screenshot_000.png", ArtifactKind::ScreenshotPng),
        ]
    );
    assert!(
        committed
            .report_dir
            .join("report_screenshot_000.png")
            .is_file()
    );
    assert!(
        !committed
            .report_dir
            .join("report_screenshot_000.rgba")
            .exists()
    );
}

#[test]
fn test_stale_legacy_json_temp_does_not_block_or_follow_symlink() {
    let dir = tempfile::tempdir().unwrap();
    let rgba = vec![0_u8; 2 * 2 * 4];
    let rgba_path = dir.path().join("report_screenshot_000.rgba");
    let png_path = dir.path().join("report_screenshot_000.png");
    let json_path = write_report_with_rgba_attachments(
        dir.path(),
        &[("report_screenshot_000.rgba", 2, 2, rgba)],
    );
    let outside = dir.path().join("outside.txt");
    std::fs::write(&outside, b"do not overwrite").unwrap();
    let legacy_tmp = dir.path().join(".report.json.png-converter.tmp");
    symlink(&outside, &legacy_tmp).unwrap();

    PNGConverter
        .process(
            &make_event(),
            &mut make_result(json_path.clone()),
            &PluginContext::without_deadline(),
        )
        .unwrap();

    assert!(!rgba_path.exists());
    assert!(png_path.exists());
    assert_eq!(std::fs::read(&outside).unwrap(), b"do not overwrite");
    assert!(
        std::fs::symlink_metadata(&legacy_tmp)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    let updated = report::load_report(&json_path).unwrap();
    assert_eq!(updated.attachments[0]["file"], "report_screenshot_000.png");
}

#[test]
fn test_replaced_rgba_path_is_not_deleted_after_json_commit() {
    let dir = tempfile::tempdir().unwrap();
    let rgba_path = dir.path().join("image.rgba");
    let original_path = dir.path().join("original.rgba");
    let json_path =
        write_report_with_rgba_attachments(dir.path(), &[("image.rgba", 1, 1, vec![0_u8; 4])]);
    let mut result = make_result(json_path.clone());

    PNGConverter::process_with_after_png_write(
        &mut result,
        &PluginContext::without_deadline(),
        || {
            std::fs::rename(&rgba_path, &original_path).unwrap();
            std::fs::write(&rgba_path, b"replacement").unwrap();
        },
    )
    .unwrap();

    assert_eq!(std::fs::read(&rgba_path).unwrap(), b"replacement");
    assert_eq!(std::fs::read(&original_path).unwrap(), vec![0_u8; 4]);
    assert_eq!(
        report::load_report(&json_path).unwrap().attachments[0]["file"],
        "image.png"
    );
}
