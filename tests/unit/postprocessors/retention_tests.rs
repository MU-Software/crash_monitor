use crate::pipeline::{
    ArtifactKind, ArtifactTransaction, CrashEvent, Plugin, PluginContext, PostProcessor,
    PostProcessorPhase, ReportContext, ReportId, ReportResult, ReportType,
};
use crate::postprocessors::RetentionManager;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};

use super::collect_entries_bounded;

fn dummy_event() -> CrashEvent {
    CrashEvent {
        report_id: ReportId::default(),
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

fn dummy_result() -> ReportResult {
    ReportResult {
        artifact_paths: Vec::new(),
        raw_path: None,
        json_path: None,
        session: None,
    }
}

fn current_unix_time() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
}

fn commit_report(pending: &Path, sent: &Path, artifact_size: usize, modified: i64) -> PathBuf {
    let event = dummy_event();
    let transaction = ArtifactTransaction::begin(ReportContext::new(&event, pending)).unwrap();
    transaction.set_destination_root(sent).unwrap();
    transaction
        .write_bytes(
            "report.json",
            ArtifactKind::Report,
            &vec![b'x'; artifact_size],
        )
        .unwrap();
    transaction
        .write_bytes("attachment.txt", ArtifactKind::Attachment, b"attachment")
        .unwrap();
    let report_dir = transaction.commit().unwrap().report_dir;
    filetime::set_file_mtime(&report_dir, filetime::FileTime::from_unix_time(modified, 0)).unwrap();
    report_dir
}

fn report_directories(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter_map(|entry| {
            entry
                .file_type()
                .ok()
                .filter(std::fs::FileType::is_dir)
                .map(|_| entry.path())
        })
        .collect()
}

fn visible_entry_count(dir: &Path) -> usize {
    fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
        .count()
}

#[test]
fn test_plugin_dependency_metadata() {
    let manager = RetentionManager::new(64, 256, 15);
    assert!(manager.hard_dependencies().is_empty());
    assert_eq!(manager.order_after(), &["ZIPArchiver", "MoveToSent"]);
    assert_eq!(manager.phase(), PostProcessorPhase::FinalCleanup);
}

#[test]
fn test_noop_under_limits() {
    let dir = tempfile::tempdir().unwrap();
    let pending = dir.path().to_path_buf();

    for i in 0..3 {
        fs::write(pending.join(format!("report_{i}.json")), "{}").unwrap();
    }

    let manager = RetentionManager::with_dir(64, 256, 15, pending.clone());
    manager
        .process(
            &dummy_event(),
            &mut dummy_result(),
            &PluginContext::without_deadline(),
        )
        .unwrap();

    let count = visible_entry_count(&pending);
    assert_eq!(count, 3, "all files should remain");
}

#[test]
fn test_deletes_oldest_by_count() {
    let dir = tempfile::tempdir().unwrap();
    let pending = dir.path().to_path_buf();

    // Use recent timestamps so age policy (365 days) doesn't delete them
    let now = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap();
    for i in 0..10 {
        let path = pending.join(format!("report_{i:02}.json"));
        fs::write(&path, "{}").unwrap();
        let _ = filetime::set_file_mtime(
            &path,
            filetime::FileTime::from_unix_time(now - 100 + i64::from(i), 0),
        );
    }

    let manager = RetentionManager::with_dir(5, 256, 365, pending.clone());
    manager
        .process(
            &dummy_event(),
            &mut dummy_result(),
            &PluginContext::without_deadline(),
        )
        .unwrap();

    let remaining: Vec<String> = fs::read_dir(&pending)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(remaining.len(), 5, "should keep only 5 reports");
}

#[test]
fn test_deletes_committed_reports_as_whole_directories_by_count() {
    let root = tempfile::tempdir().unwrap();
    let pending = root.path().join("pending");
    let sent = root.path().join("sent");
    fs::create_dir(&pending).unwrap();
    let now = current_unix_time();
    let oldest = commit_report(&pending, &sent, 16, now - 30);
    let middle = commit_report(&pending, &sent, 32, now - 20);
    let newest = commit_report(&pending, &sent, 48, now - 10);

    RetentionManager::with_dir(2, 256, 365, sent.clone())
        .process(
            &dummy_event(),
            &mut dummy_result(),
            &PluginContext::without_deadline(),
        )
        .unwrap();

    assert!(
        !oldest.exists(),
        "the complete oldest report must be removed"
    );
    for retained in [&middle, &newest] {
        assert!(retained.join("manifest.json").is_file());
        assert!(retained.join("report.json").is_file());
        assert!(retained.join("attachment.txt").is_file());
    }
    assert_eq!(report_directories(&sent).len(), 2);
}

#[test]
fn test_deletes_by_size() {
    let dir = tempfile::tempdir().unwrap();
    let pending = dir.path().to_path_buf();

    let now = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap();
    for i in 0..10 {
        let path = pending.join(format!("report_{i:02}.json"));
        fs::write(&path, "x".repeat(1024)).unwrap();
        let _ = filetime::set_file_mtime(
            &path,
            filetime::FileTime::from_unix_time(now - 100 + i64::from(i), 0),
        );
    }

    // max_total_bytes = 0 → delete all
    let manager = RetentionManager::with_dir(100, 0, 365, pending.clone()); // 0 MB = delete all
    manager
        .process(
            &dummy_event(),
            &mut dummy_result(),
            &PluginContext::without_deadline(),
        )
        .unwrap();

    let count = visible_entry_count(&pending);
    assert_eq!(count, 0, "all files should be deleted when size limit is 0");
}

#[test]
fn test_committed_report_size_includes_every_artifact_and_manifest() {
    let root = tempfile::tempdir().unwrap();
    let pending = root.path().join("pending");
    let sent = root.path().join("sent");
    fs::create_dir(&pending).unwrap();
    let now = current_unix_time();
    let oldest = commit_report(&pending, &sent, 600 * 1024, now - 20);
    let newest = commit_report(&pending, &sent, 600 * 1024, now - 10);

    RetentionManager::with_dir(100, 1, 365, sent.clone())
        .process(
            &dummy_event(),
            &mut dummy_result(),
            &PluginContext::without_deadline(),
        )
        .unwrap();

    assert!(!oldest.exists());
    assert!(newest.join("manifest.json").is_file());
    assert!(newest.join("attachment.txt").is_file());
    assert_eq!(report_directories(&sent), vec![newest]);
}

#[test]
fn test_deletes_by_age() {
    let dir = tempfile::tempdir().unwrap();
    let pending = dir.path().to_path_buf();

    // Create files with old timestamps (30 days ago)
    let old_time = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
        - 30 * 86400;
    for i in 0..5 {
        let path = pending.join(format!("report_{i:02}.json"));
        fs::write(&path, "{}").unwrap();
        let _ = filetime::set_file_mtime(
            &path,
            filetime::FileTime::from_unix_time(old_time + i64::from(i), 0),
        );
    }

    // max_age = 15 days → all 30-day-old files should be deleted
    let manager = RetentionManager::with_dir(100, 256, 15, pending.clone());
    manager
        .process(
            &dummy_event(),
            &mut dummy_result(),
            &PluginContext::without_deadline(),
        )
        .unwrap();

    let count = visible_entry_count(&pending);
    assert_eq!(count, 0, "all old files should be deleted");
}

#[test]
fn test_deletes_entire_committed_report_by_directory_age() {
    let root = tempfile::tempdir().unwrap();
    let pending = root.path().join("pending");
    let sent = root.path().join("sent");
    fs::create_dir(&pending).unwrap();
    let old = commit_report(&pending, &sent, 32, current_unix_time() - 30 * 86_400);

    RetentionManager::with_dir(100, 256, 15, sent)
        .process(
            &dummy_event(),
            &mut dummy_result(),
            &PluginContext::without_deadline(),
        )
        .unwrap();

    assert!(!old.exists());
}

#[test]
fn test_ignores_hidden_partial_but_fails_closed_on_visible_manifestless_report() {
    let root = tempfile::tempdir().unwrap();
    let hidden = root
        .path()
        .join(".report-0123456789abcdef0123456789abcdef.pending");
    let manifestless = root.path().join("fedcba9876543210fedcba9876543210");
    fs::create_dir(&hidden).unwrap();
    fs::write(hidden.join("partial.json"), "partial").unwrap();
    fs::create_dir(&manifestless).unwrap();
    fs::write(manifestless.join("report.json"), "uncommitted").unwrap();

    let error = RetentionManager::with_dir(0, 0, 0, root.path().to_path_buf())
        .process(
            &dummy_event(),
            &mut dummy_result(),
            &PluginContext::without_deadline(),
        )
        .unwrap_err();

    assert!(error.contains("cannot safely inventory committed report"));
    assert!(hidden.is_dir());
    assert!(manifestless.is_dir());
}

#[test]
fn test_does_not_delete_report_with_unmanifested_extra_file() {
    let root = tempfile::tempdir().unwrap();
    let pending = root.path().join("pending");
    let sent = root.path().join("sent");
    fs::create_dir(&pending).unwrap();
    let healthy = commit_report(&pending, &sent, 16, current_unix_time() - 20);
    let report = commit_report(&pending, &sent, 16, current_unix_time() - 10);
    fs::write(report.join("unexpected.tmp"), "must fail closed").unwrap();

    let error = RetentionManager::with_dir(0, 0, 365, sent)
        .process(
            &dummy_event(),
            &mut dummy_result(),
            &PluginContext::without_deadline(),
        )
        .unwrap_err();

    assert!(error.contains("unmanifested report artifact"));
    assert!(healthy.join("manifest.json").is_file());
    assert!(healthy.join("report.json").is_file());
    assert!(report.join("manifest.json").is_file());
    assert!(report.join("report.json").is_file());
    assert!(report.join("unexpected.tmp").is_file());
}

#[test]
fn test_never_follows_report_directory_or_artifact_symlinks() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let pending = root.path().join("pending");
    let sent = root.path().join("sent");
    let outside = root.path().join("outside");
    fs::create_dir(&pending).unwrap();
    fs::create_dir(&outside).unwrap();
    let now = current_unix_time();

    let linked_report = commit_report(&pending, &sent, 16, now - 20);
    let linked_name = linked_report.file_name().unwrap().to_owned();
    let outside_report = outside.join(&linked_name);
    fs::rename(&linked_report, &outside_report).unwrap();
    symlink(&outside_report, &linked_report).unwrap();

    let invalid_artifact_report = commit_report(&pending, &sent, 16, now - 10);
    let outside_file = outside.join("outside.json");
    fs::write(&outside_file, vec![b'x'; 16]).unwrap();
    fs::remove_file(invalid_artifact_report.join("report.json")).unwrap();
    symlink(&outside_file, invalid_artifact_report.join("report.json")).unwrap();

    let error = RetentionManager::with_dir(0, 0, 365, sent)
        .process(
            &dummy_event(),
            &mut dummy_result(),
            &PluginContext::without_deadline(),
        )
        .unwrap_err();

    assert!(error.contains("not a regular file"));
    assert!(
        fs::symlink_metadata(linked_report)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(outside_report.join("manifest.json").is_file());
    assert!(invalid_artifact_report.is_dir());
    assert_eq!(fs::read(outside_file).unwrap(), vec![b'x'; 16]);
}

#[test]
fn test_after_notify_scan_counts_current_report_exactly_once() {
    let root = tempfile::tempdir().unwrap();
    let pending = root.path().join("pending");
    let sent = root.path().join("sent");
    fs::create_dir(&pending).unwrap();
    let now = current_unix_time();
    let oldest = commit_report(&pending, &sent, 16, now - 20);
    let retained = commit_report(&pending, &sent, 16, now - 10);

    let event = dummy_event();
    let transaction = ArtifactTransaction::begin(ReportContext::new(&event, &pending)).unwrap();
    transaction.set_destination_root(&sent).unwrap();
    transaction
        .write_bytes("report.json", ArtifactKind::Report, b"incoming")
        .unwrap();
    let context = PluginContext::without_deadline().with_artifact_transaction(transaction.clone());
    let committed = transaction.commit().unwrap();

    RetentionManager::with_dir(2, 256, 365, sent.clone())
        .process(&event, &mut dummy_result(), &context)
        .unwrap();

    assert!(!oldest.exists());
    assert!(retained.exists());
    assert!(committed.report_dir.is_dir());
    assert_eq!(report_directories(&sent).len(), 2);
}

#[test]
fn test_after_notify_scan_includes_current_report_bytes() {
    let root = tempfile::tempdir().unwrap();
    let pending = root.path().join("pending");
    let sent = root.path().join("sent");
    fs::create_dir(&pending).unwrap();
    let existing = commit_report(&pending, &sent, 700 * 1024, current_unix_time() - 10);

    let event = dummy_event();
    let transaction = ArtifactTransaction::begin(ReportContext::new(&event, &pending)).unwrap();
    transaction.set_destination_root(&sent).unwrap();
    transaction
        .write_bytes("report.json", ArtifactKind::Report, &vec![b'i'; 400 * 1024])
        .unwrap();
    let context = PluginContext::without_deadline().with_artifact_transaction(transaction.clone());
    let committed = transaction.commit().unwrap();

    RetentionManager::with_dir(10, 1, 365, sent.clone())
        .process(&event, &mut dummy_result(), &context)
        .unwrap();

    assert!(!existing.exists());
    assert!(committed.report_dir.is_dir());
    assert_eq!(report_directories(&sent), vec![committed.report_dir]);
}

#[test]
fn test_override_does_not_scan_a_different_committed_destination() {
    let root = tempfile::tempdir().unwrap();
    let pending = root.path().join("pending");
    let sent = root.path().join("sent");
    let other = root.path().join("other");
    fs::create_dir(&pending).unwrap();
    let now = current_unix_time();
    let first = commit_report(&pending, &sent, 16, now - 20);
    let second = commit_report(&pending, &sent, 16, now - 10);

    let event = dummy_event();
    let transaction = ArtifactTransaction::begin(ReportContext::new(&event, &pending)).unwrap();
    transaction.set_destination_root(&other).unwrap();
    transaction
        .write_bytes("report.json", ArtifactKind::Report, b"incoming")
        .unwrap();
    let context = PluginContext::without_deadline().with_artifact_transaction(transaction.clone());
    transaction.commit().unwrap();

    RetentionManager::with_dir(2, 256, 365, sent.clone())
        .process(&event, &mut dummy_result(), &context)
        .unwrap();

    assert!(first.is_dir());
    assert!(second.is_dir());
    assert_eq!(report_directories(&sent).len(), 2);
}

#[test]
fn test_without_override_scans_the_current_committed_parent() {
    let root = tempfile::tempdir().unwrap();
    let pending = root.path().join("pending");
    let sent = root.path().join("custom-sent");
    fs::create_dir(&pending).unwrap();
    let old = commit_report(&pending, &sent, 16, current_unix_time() - 10);

    let event = dummy_event();
    let transaction = ArtifactTransaction::begin(ReportContext::new(&event, &pending)).unwrap();
    transaction.set_destination_root(&sent).unwrap();
    transaction
        .write_bytes("report.json", ArtifactKind::Report, b"incoming")
        .unwrap();
    let context = PluginContext::without_deadline().with_artifact_transaction(transaction.clone());
    let current = transaction.commit().unwrap();

    RetentionManager::new(1, 256, 365)
        .process(&event, &mut dummy_result(), &context)
        .unwrap();

    assert!(!old.exists());
    assert!(current.report_dir.is_dir());
    assert_eq!(report_directories(&sent), vec![current.report_dir]);
}

#[test]
fn test_final_cleanup_defers_impossible_zero_quota_without_deleting_current_report() {
    let root = tempfile::tempdir().unwrap();
    let pending = root.path().join("pending");
    let sent = root.path().join("sent");
    fs::create_dir(&pending).unwrap();

    let event = dummy_event();
    let transaction = ArtifactTransaction::begin(ReportContext::new(&event, &pending)).unwrap();
    transaction.set_destination_root(&sent).unwrap();
    transaction
        .write_bytes("report.json", ArtifactKind::Report, b"current")
        .unwrap();
    let context = PluginContext::without_deadline().with_artifact_transaction(transaction.clone());
    let current = transaction.commit().unwrap();

    let error = RetentionManager::with_dir(0, 0, 365, sent)
        .process(&event, &mut dummy_result(), &context)
        .unwrap_err();
    transaction.release_publication_lease();

    assert!(error.contains("deferred by a live lease"), "{error}");
    assert!(current.report_dir.join("report.json").is_file());
}

#[test]
fn test_final_cleanup_defers_impossible_age_quota_for_current_report() {
    let root = tempfile::tempdir().unwrap();
    let pending = root.path().join("pending");
    let sent = root.path().join("sent");
    fs::create_dir(&pending).unwrap();

    let event = dummy_event();
    let transaction = ArtifactTransaction::begin(ReportContext::new(&event, &pending)).unwrap();
    transaction.set_destination_root(&sent).unwrap();
    transaction
        .write_bytes("report.json", ArtifactKind::Report, b"current")
        .unwrap();
    let context = PluginContext::without_deadline().with_artifact_transaction(transaction.clone());
    let current = transaction.commit().unwrap();
    filetime::set_file_mtime(
        &current.report_dir,
        filetime::FileTime::from_unix_time(current_unix_time() - 60, 0),
    )
    .unwrap();

    let error = RetentionManager::with_dir(10, 256, 0, sent)
        .process(&event, &mut dummy_result(), &context)
        .unwrap_err();
    transaction.release_publication_lease();

    assert!(error.contains("oldest_over_age=true"), "{error}");
    assert!(current.report_dir.join("report.json").is_file());
}

#[test]
fn test_concurrent_publication_leases_preserve_oldest_first_order() {
    let root = tempfile::tempdir().unwrap();
    let pending = root.path().join("pending");
    let sent = root.path().join("sent");
    fs::create_dir(&pending).unwrap();

    let older_event = dummy_event();
    let older = ArtifactTransaction::begin(ReportContext::new(&older_event, &pending)).unwrap();
    older.set_destination_root(&sent).unwrap();
    older
        .write_bytes("report.json", ArtifactKind::Report, b"older")
        .unwrap();
    let older_report = older.commit().unwrap();

    let newer_event = dummy_event();
    let newer = ArtifactTransaction::begin(ReportContext::new(&newer_event, &pending)).unwrap();
    newer.set_destination_root(&sent).unwrap();
    newer
        .write_bytes("report.json", ArtifactKind::Report, b"newer")
        .unwrap();
    let newer_context = PluginContext::without_deadline().with_artifact_transaction(newer.clone());
    let newer_report = newer.commit().unwrap();
    let now = current_unix_time();
    filetime::set_file_mtime(
        &older_report.report_dir,
        filetime::FileTime::from_unix_time(now - 1, 0),
    )
    .unwrap();
    filetime::set_file_mtime(
        &newer_report.report_dir,
        filetime::FileTime::from_unix_time(now, 0),
    )
    .unwrap();

    let deferred = RetentionManager::with_dir(1, 256, 365, sent.clone())
        .process(&newer_event, &mut dummy_result(), &newer_context)
        .unwrap_err();
    assert!(deferred.contains("deferred by a live lease"));
    assert!(older_report.report_dir.join("report.json").is_file());
    assert!(newer_report.report_dir.join("report.json").is_file());

    older.release_publication_lease();
    drop(older);

    RetentionManager::with_dir(1, 256, 365, sent.clone())
        .process(&newer_event, &mut dummy_result(), &newer_context)
        .unwrap();

    assert!(!older_report.report_dir.exists());
    assert!(newer_report.report_dir.join("report.json").is_file());

    newer.release_publication_lease();

    assert_eq!(report_directories(&sent), vec![newer_report.report_dir]);
}

#[test]
fn test_owner_lock_preserves_foreign_report_after_process_lease_is_released() {
    let root = tempfile::tempdir().unwrap();
    let pending = root.path().join("pending");
    let sent = root.path().join("sent");
    fs::create_dir(&pending).unwrap();

    let older_event = dummy_event();
    let older = ArtifactTransaction::begin(ReportContext::new(&older_event, &pending)).unwrap();
    older.set_destination_root(&sent).unwrap();
    older
        .write_bytes("report.json", ArtifactKind::Report, b"older")
        .unwrap();
    let older_report = older.commit().unwrap();
    older.release_publication_lease();

    let newer_event = dummy_event();
    let newer = ArtifactTransaction::begin(ReportContext::new(&newer_event, &pending)).unwrap();
    newer.set_destination_root(&sent).unwrap();
    newer
        .write_bytes("report.json", ArtifactKind::Report, b"newer")
        .unwrap();
    let newer_context = PluginContext::without_deadline().with_artifact_transaction(newer.clone());
    let newer_report = newer.commit().unwrap();
    let now = current_unix_time();
    filetime::set_file_mtime(
        &older_report.report_dir,
        filetime::FileTime::from_unix_time(now - 1, 0),
    )
    .unwrap();
    filetime::set_file_mtime(
        &newer_report.report_dir,
        filetime::FileTime::from_unix_time(now, 0),
    )
    .unwrap();

    let deferred = RetentionManager::with_dir(1, 256, 365, sent.clone())
        .process(&newer_event, &mut dummy_result(), &newer_context)
        .unwrap_err();
    assert!(deferred.contains("deferred by a live lease"), "{deferred}");
    assert!(older_report.report_dir.join("report.json").is_file());

    drop(older);
    RetentionManager::with_dir(1, 256, 365, sent.clone())
        .process(&newer_event, &mut dummy_result(), &newer_context)
        .unwrap();

    assert!(!older_report.report_dir.exists());
    assert!(newer_report.report_dir.join("report.json").is_file());
    newer.release_publication_lease();
}

#[test]
fn test_concurrent_retention_runs_are_serialized() {
    let root = tempfile::tempdir().unwrap();
    let pending = root.path().join("pending");
    let sent = root.path().join("sent");
    fs::create_dir(&pending).unwrap();
    let now = current_unix_time();
    for index in 0..8 {
        commit_report(&pending, &sent, 16, now - 100 + i64::from(index));
    }

    let barrier = Arc::new(Barrier::new(5));
    let mut workers = Vec::new();
    for _ in 0..4 {
        let sent = sent.clone();
        let barrier = barrier.clone();
        workers.push(std::thread::spawn(move || {
            barrier.wait();
            RetentionManager::with_dir(2, 256, 365, sent)
                .process(
                    &dummy_event(),
                    &mut dummy_result(),
                    &PluginContext::without_deadline(),
                )
                .unwrap();
        }));
    }
    barrier.wait();
    for worker in workers {
        worker.join().unwrap();
    }

    assert_eq!(report_directories(&sent).len(), 2);
}

#[test]
fn test_retries_bounded_cleanup_of_hidden_retention_tombstones() {
    let root = tempfile::tempdir().unwrap();
    let tombstone = root.path().join(
        ".retention-0123456789abcdef0123456789abcdef.abcdef0123456789abcdef0123456789.deleting",
    );
    fs::create_dir(&tombstone).unwrap();
    fs::write(tombstone.join("leftover.json"), "partial cleanup").unwrap();

    RetentionManager::with_dir(10, 256, 365, root.path().to_path_buf())
        .process(
            &dummy_event(),
            &mut dummy_result(),
            &PluginContext::without_deadline(),
        )
        .unwrap();

    assert!(!tombstone.exists());
}

#[test]
fn test_stale_tombstone_cleanup_refuses_recursive_directories() {
    let root = tempfile::tempdir().unwrap();
    let tombstone = root.path().join(
        ".retention-0123456789abcdef0123456789abcdef.abcdef0123456789abcdef0123456789.deleting",
    );
    fs::create_dir(&tombstone).unwrap();
    fs::create_dir(tombstone.join("unexpected-directory")).unwrap();

    let error = RetentionManager::with_dir(10, 256, 365, root.path().to_path_buf())
        .process(
            &dummy_event(),
            &mut dummy_result(),
            &PluginContext::without_deadline(),
        )
        .unwrap_err();

    assert!(error.contains("unexpected directory"));
    assert!(tombstone.is_dir());
}

#[test]
fn test_rejects_symlink_retention_root() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let real = root.path().join("real");
    let linked = root.path().join("linked");
    fs::create_dir(&real).unwrap();
    fs::write(real.join("legacy.json"), "{} ").unwrap();
    symlink(&real, &linked).unwrap();

    let error = RetentionManager::with_dir(0, 0, 0, linked)
        .process(
            &dummy_event(),
            &mut dummy_result(),
            &PluginContext::without_deadline(),
        )
        .unwrap_err();

    assert!(error.contains("not a real directory"));
    assert!(real.join("legacy.json").is_file());
}

#[test]
fn test_rejects_symlink_retention_lock_without_following_it() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let outside = root.path().join("outside-lock");
    let target = root.path().join("target");
    fs::create_dir(&target).unwrap();
    fs::write(&outside, "outside").unwrap();
    symlink(&outside, target.join(".retention.lock")).unwrap();

    let error = RetentionManager::with_dir(10, 256, 365, target)
        .process(
            &dummy_event(),
            &mut dummy_result(),
            &PluginContext::without_deadline(),
        )
        .unwrap_err();

    assert!(error.contains("cannot safely open retention lock"));
    assert_eq!(fs::read_to_string(outside).unwrap(), "outside");
}

#[test]
fn test_empty_dir_noop() {
    let dir = tempfile::tempdir().unwrap();
    let manager = RetentionManager::with_dir(64, 256, 15, dir.path().to_path_buf());
    assert!(
        manager
            .process(
                &dummy_event(),
                &mut dummy_result(),
                &PluginContext::without_deadline(),
            )
            .is_ok()
    );
}

#[test]
fn test_directory_scan_respects_entry_cap() {
    let dir = tempfile::tempdir().unwrap();
    for index in 0..3 {
        fs::write(dir.path().join(format!("report_{index}.json")), "{}").unwrap();
    }

    let error = match collect_entries_bounded(dir.path(), &PluginContext::without_deadline(), 2) {
        Err(error) => error,
        Ok(entries) => panic!(
            "an incomplete scan must fail closed before pruning; got {} entries",
            entries.len()
        ),
    };

    assert!(error.contains("bounded limit"));
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 3);
}
