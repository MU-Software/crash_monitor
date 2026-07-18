//! Tests for the data dir override.
//!
//! These tests rely on `.cargo/config.toml` setting `CRASH_MONITOR_DATA_DIR` to
//! `target/test-crash-data` for all `cargo test` invocations. They never mutate
//! the env var (which would race with parallel tests) — they only assert that
//! the override is in effect, so accidental fallback to the user's home
//! directory is impossible during testing.

use super::*;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

fn mode(path: &std::path::Path) -> u32 {
    std::fs::symlink_metadata(path)
        .unwrap()
        .permissions()
        .mode()
        & 0o7777
}

#[test]
fn test_data_dir_override_env_is_set_during_cargo_test() {
    // .cargo/config.toml MUST set CRASH_MONITOR_DATA_DIR for all cargo invocations.
    // If this assertion fails, the cargo config is missing or broken — and any
    // test that triggers `pending_dir()` could leak files to the user's home.
    let val = std::env::var("CRASH_MONITOR_DATA_DIR").expect(
        "CRASH_MONITOR_DATA_DIR must be set by .cargo/config.toml during cargo test \
         to prevent test fixtures from polluting ~/.crash_monitor/",
    );
    assert!(!val.is_empty(), "CRASH_MONITOR_DATA_DIR must not be empty");
}

#[test]
fn test_data_dir_uses_override_not_home() {
    let dir = data_dir().expect("data_dir() must succeed under override");
    let s = dir.to_string_lossy();
    assert!(
        !s.contains(".crash_monitor"),
        "data_dir() must NOT use ~/.crash_monitor during tests, got: {s}"
    );
    assert!(
        s.contains("test-crash-data"),
        "data_dir() should resolve under target/test-crash-data, got: {s}"
    );
}

#[test]
fn test_pending_dir_uses_override_not_home() {
    let dir = pending_dir().expect("pending_dir() must succeed under override");
    let s = dir.to_string_lossy();
    assert!(
        !s.contains(".crash_monitor"),
        "pending_dir() must NOT use ~/.crash_monitor during tests, got: {s}"
    );
    assert!(
        s.contains("test-crash-data"),
        "pending_dir() should resolve under target/test-crash-data, got: {s}"
    );
    assert!(
        s.ends_with("crashes/pending"),
        "pending_dir() should end with crashes/pending, got: {s}"
    );
}

#[test]
fn existing_owned_directory_mode_is_corrected_without_replacing_it() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("private");
    std::fs::create_dir(&directory).unwrap();
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o2777)).unwrap();
    let before = std::fs::symlink_metadata(&directory).unwrap();

    ensure_private_directory(&directory).unwrap();

    let after = std::fs::symlink_metadata(&directory).unwrap();
    assert_eq!(before.ino(), after.ino());
    assert_eq!(mode(&directory), PRIVATE_DIRECTORY_MODE);
}

#[test]
fn private_parent_does_not_cause_general_existing_ancestors_to_be_chmoded() {
    let root = tempfile::tempdir().unwrap();
    ensure_private_directory(root.path()).unwrap();
    let general = root.path().join("project");
    std::fs::create_dir(&general).unwrap();
    std::fs::set_permissions(&general, std::fs::Permissions::from_mode(0o755)).unwrap();

    let private = general.join("crash-data");
    ensure_private_directory(&private).unwrap();

    assert_eq!(mode(&general), 0o755);
    assert_eq!(mode(&private), PRIVATE_DIRECTORY_MODE);
}

#[test]
fn tempfile_alias_is_supported_but_symlink_inside_private_anchor_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    ensure_private_directory(root.path()).unwrap();

    let outside = tempfile::tempdir().unwrap();
    let linked = root.path().join("linked");
    symlink(outside.path(), &linked).unwrap();
    let requested = linked.join("report");

    let error = ensure_private_directory(&requested).unwrap_err();

    assert!(
        error.contains("safely open") || error.contains("symlink"),
        "{error}"
    );
    assert!(!outside.path().join("report").exists());
    assert!(
        std::fs::symlink_metadata(&linked)
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[test]
fn final_directory_symlink_is_rejected_without_changing_target() {
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("target");
    std::fs::create_dir(&target).unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
    let linked = root.path().join("private");
    symlink(&target, &linked).unwrap();

    let error = ensure_private_directory(&linked).unwrap_err();

    assert!(error.contains("safely open"), "{error}");
    assert_eq!(mode(&target), 0o755);
}

#[test]
#[ignore = "spawned with an isolated process-global umask"]
fn private_modes_under_subprocess_umask_helper() {
    let Ok(mask) = std::env::var("CRASH_MONITOR_P014_UMASK") else {
        return;
    };
    let root = PathBuf::from(std::env::var_os("CRASH_MONITOR_P014_ROOT").unwrap());
    let mask = nix::libc::mode_t::from_str_radix(&mask, 8).unwrap();
    // SAFETY: this helper is the only test selected in a dedicated process,
    // so changing the process-global umask cannot race another test thread.
    unsafe {
        nix::libc::umask(mask);
    }

    ensure_private_directory(&root).unwrap();
    assert_eq!(mode(&root), PRIVATE_DIRECTORY_MODE);

    for round in 0..50 {
        let concurrent = root.join(format!("concurrent-{round}/nested/private"));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(9));
        let handles = std::array::from_fn::<_, 8, _>(|_| {
            let path = concurrent.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                ensure_private_directory(&path)
            })
        });
        barrier.wait();
        for handle in handles {
            handle.join().unwrap().unwrap();
        }
        for directory in [
            root.join(format!("concurrent-{round}")),
            root.join(format!("concurrent-{round}/nested")),
            concurrent,
        ] {
            assert_eq!(mode(&directory), PRIVATE_DIRECTORY_MODE);
        }
    }

    let standalone = root.join("standalone.tmp");
    let mut file = create_private_file(&standalone).unwrap();
    file.write_all(b"private").unwrap();
    file.sync_all().unwrap();
    assert_eq!(mode(&standalone), PRIVATE_FILE_MODE);

    let event = crate::pipeline::CrashEvent {
        report_id: crate::pipeline::ReportId::new(),
        report_type: crate::pipeline::ReportType::Crash,
        termination: None,
        exception_type: None,
        exception_code: None,
        exception_subcode: None,
        exception_codes: Vec::new(),
        crashed_thread: None,
        bail_on_suspend_failure: false,
        pid: 1234,
        process_name: "private-mode-helper".into(),
        hang_duration_ms: None,
    };
    let transaction = crate::pipeline::ArtifactTransaction::begin(
        crate::pipeline::ReportContext::new(&event, &root),
    )
    .unwrap();
    for (name, kind) in [
        ("report.json", crate::pipeline::ArtifactKind::Report),
        ("threads.raw", crate::pipeline::ArtifactKind::ThreadRaw),
        ("screen.rgba", crate::pipeline::ArtifactKind::ScreenshotRgba),
        ("screen.png", crate::pipeline::ArtifactKind::ScreenshotPng),
        ("report.zip", crate::pipeline::ArtifactKind::Archive),
    ] {
        transaction.write_bytes(name, kind, b"artifact").unwrap();
    }
    let committed = transaction.commit().unwrap();
    assert_eq!(mode(&committed.report_dir), PRIVATE_DIRECTORY_MODE);
    assert_eq!(mode(&committed.manifest_path), PRIVATE_FILE_MODE);
    for artifact in crate::pipeline::load_manifest(&committed.manifest_path)
        .unwrap()
        .artifacts
    {
        assert_eq!(
            mode(&committed.report_dir.join(artifact.path)),
            PRIVATE_FILE_MODE
        );
    }
}

#[test]
fn permissive_and_maximally_restrictive_umasks_produce_exact_private_modes() {
    for mask in ["000", "777"] {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join(format!("umask-{mask}"));
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "utils::paths::tests::private_modes_under_subprocess_umask_helper",
                "--ignored",
                "--nocapture",
            ])
            .env("CRASH_MONITOR_P014_UMASK", mask)
            .env("CRASH_MONITOR_P014_ROOT", &root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "umask {mask} helper failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
#[ignore = "spawned with an isolated process current directory"]
fn relative_cwd_security_helper() {
    if std::env::var_os("CRASH_MONITOR_P014_UNSAFE_CWD").is_none() {
        return;
    }
    let error = ensure_private_directory(Path::new("relative-private")).unwrap_err();
    assert!(error.contains("untrusted writes"), "{error}");
    assert!(!Path::new("relative-private").exists());
}

#[test]
fn relative_path_rejects_world_writable_non_sticky_cwd() {
    let cwd = tempfile::tempdir().unwrap();
    std::fs::set_permissions(cwd.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "utils::paths::tests::relative_cwd_security_helper",
            "--ignored",
            "--nocapture",
        ])
        .env("CRASH_MONITOR_P014_UNSAFE_CWD", "1")
        .current_dir(cwd.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "relative-cwd helper failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn exclusive_publication_has_one_winner_and_never_clobbers() {
    let root = tempfile::tempdir().unwrap();
    ensure_private_directory(root.path()).unwrap();
    let first = root.path().join("first.tmp");
    let second = root.path().join("second.tmp");
    let destination = root.path().join("report.json");
    create_private_file(&first)
        .unwrap()
        .write_all(b"first")
        .unwrap();
    create_private_file(&second)
        .unwrap()
        .write_all(b"second")
        .unwrap();

    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
    let handles = [first.clone(), second.clone()].map(|source| {
        let destination = destination.clone();
        let barrier = barrier.clone();
        std::thread::spawn(move || {
            barrier.wait();
            publish_private_path(&source, &destination).map(|()| source)
        })
    });
    barrier.wait();
    let results = handles.map(|handle| handle.join().unwrap());

    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(results.iter().filter(|result| result.is_err()).count(), 1);
    let winner = results.into_iter().find_map(Result::ok).unwrap();
    let loser = if winner == first { second } else { first };
    assert!(!winner.exists());
    assert!(loser.exists());
    assert!(destination.exists());
    assert_eq!(mode(&destination), PRIVATE_FILE_MODE);
}

#[cfg(target_os = "macos")]
#[test]
fn source_swap_after_validation_is_detected_and_quarantined_before_success() {
    let root = tempfile::tempdir().unwrap();
    ensure_private_directory(root.path()).unwrap();
    let source = root.path().join("source.tmp");
    let validated_backup = root.path().join("validated.backup");
    let destination = root.path().join("report.json");
    create_private_file(&source)
        .unwrap()
        .write_all(b"validated")
        .unwrap();

    let error = publish_private_path_with_hook(&source, &destination, || {
        std::fs::rename(&source, &validated_backup).unwrap();
        create_private_file(&source)
            .unwrap()
            .write_all(b"replacement")
            .unwrap();
    })
    .unwrap_err();

    assert!(error.contains("changed after validation"), "{error}");
    assert!(!destination.exists());
    assert_eq!(std::fs::read(&validated_backup).unwrap(), b"validated");
    assert!(!source.exists());
    let quarantine = std::fs::read_dir(root.path())
        .unwrap()
        .map(Result::unwrap)
        .find(|entry| entry.file_name().to_string_lossy().starts_with(".publish-"))
        .expect("replacement should be quarantined");
    assert_eq!(std::fs::read(quarantine.path()).unwrap(), b"replacement");
}

#[cfg(target_os = "macos")]
#[test]
fn destination_swap_before_identity_check_is_contained_without_following_symlink() {
    let root = tempfile::tempdir().unwrap();
    ensure_private_directory(root.path()).unwrap();
    let source = root.path().join("source.tmp");
    let destination = root.path().join("report.json");
    let published_backup = root.path().join("published.backup");
    let outside = root.path().join("outside.txt");
    create_private_file(&source)
        .unwrap()
        .write_all(b"validated")
        .unwrap();
    create_private_file(&outside)
        .unwrap()
        .write_all(b"outside")
        .unwrap();

    let error = publish_private_path_with_hooks(
        &source,
        &destination,
        || {},
        || {
            std::fs::rename(&destination, &published_backup).unwrap();
            std::os::unix::fs::symlink(&outside, &destination).unwrap();
        },
    )
    .unwrap_err();

    assert!(error.contains("changed after validation"), "{error}");
    assert!(!destination.exists());
    assert_eq!(std::fs::read(&published_backup).unwrap(), b"validated");
    assert_eq!(std::fs::read(&outside).unwrap(), b"outside");
    assert!(!source.exists());
    let quarantine = std::fs::read_dir(root.path())
        .unwrap()
        .map(Result::unwrap)
        .find(|entry| entry.file_name().to_string_lossy().starts_with(".publish-"))
        .expect("symlink should be quarantined");
    assert!(quarantine.file_type().unwrap().is_symlink());
    assert_eq!(std::fs::read_link(quarantine.path()).unwrap(), outside);
}

#[cfg(target_os = "macos")]
#[test]
fn destination_permission_drift_before_final_check_is_contained() {
    let root = tempfile::tempdir().unwrap();
    ensure_private_directory(root.path()).unwrap();
    let source = root.path().join("source.tmp");
    let destination = root.path().join("report.json");
    create_private_file(&source)
        .unwrap()
        .write_all(b"validated")
        .unwrap();

    let error = publish_private_path_with_hooks(
        &source,
        &destination,
        || {},
        || {
            std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o644)).unwrap();
        },
    )
    .unwrap_err();

    assert!(error.contains("permissions changed"), "{error}");
    assert!(!destination.exists());
    assert_eq!(std::fs::read(&source).unwrap(), b"validated");
    assert_eq!(mode(&source), PRIVATE_FILE_MODE);
}

#[cfg(target_os = "macos")]
#[test]
fn extended_acl_directory_and_publish_source_are_rejected() {
    let root = tempfile::tempdir().unwrap();
    ensure_private_directory(root.path()).unwrap();
    let directory = root.path().join("acl-directory");
    create_private_directory(&directory).unwrap();
    let source = root.path().join("acl-source.tmp");
    create_private_file(&source).unwrap();

    for path in [&directory, &source] {
        let status = std::process::Command::new("/bin/chmod")
            .args(["+a", "everyone allow read"])
            .arg(path)
            .status()
            .unwrap();
        assert!(status.success(), "failed to install test ACL on {path:?}");
    }

    let directory_error = ensure_private_directory(&directory).unwrap_err();
    assert!(
        directory_error.contains("extended ACL"),
        "{directory_error}"
    );
    let destination = root.path().join("published");
    let publish_error = publish_private_path(&source, &destination).unwrap_err();
    assert!(publish_error.contains("extended ACL"), "{publish_error}");
    assert!(source.exists());
    assert!(!destination.exists());

    for path in [&directory, &source] {
        let _ = std::process::Command::new("/bin/chmod")
            .arg("-N")
            .arg(path)
            .status();
    }
}

#[cfg(target_os = "macos")]
#[test]
fn deny_only_acl_on_external_ancestor_is_allowed_but_allow_acl_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    ensure_private_directory(root.path()).unwrap();
    let ancestor = root.path().join("home-like");
    std::fs::create_dir(&ancestor).unwrap();
    std::fs::set_permissions(&ancestor, std::fs::Permissions::from_mode(0o750)).unwrap();

    let deny_status = std::process::Command::new("/bin/chmod")
        .args(["+a", "everyone deny delete"])
        .arg(&ancestor)
        .status()
        .unwrap();
    assert!(deny_status.success());
    let private = ancestor.join("crash-data");
    ensure_private_directory(&private).unwrap();
    assert_eq!(mode(&ancestor), 0o750);
    assert_eq!(mode(&private), PRIVATE_DIRECTORY_MODE);

    let allow_status = std::process::Command::new("/bin/chmod")
        .args(["+a", "everyone allow write"])
        .arg(&ancestor)
        .status()
        .unwrap();
    assert!(allow_status.success());
    let error = ensure_private_directory(&ancestor.join("other-private")).unwrap_err();
    assert!(error.contains("grants access"), "{error}");

    let _ = std::process::Command::new("/bin/chmod")
        .arg("-N")
        .arg(&ancestor)
        .status();
}
