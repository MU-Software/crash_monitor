use crate::config::*;

#[test]
fn test_default_config_enables_only_non_sensitive_plugins() {
    let config = CrashReporterConfig::default();
    assert!(config.enabled);
    assert_eq!(config.privacy.level, PrivacyLevel::Minimal);
    assert_eq!(config.privacy.consent, ConsentState::NotGranted);
    assert_eq!(config.privacy.encryption, EncryptionPolicy::None);
    assert!(!config.privacy.raw_shm);
    assert!(config.triggers.enabled);
    assert!(config.triggers.crash.enabled);
    assert!(config.triggers.exit_failure.enabled);
    assert!(config.triggers.signal_failure.enabled);
    assert!(!config.triggers.oom_detection.enabled);
    assert!(config.triggers.anr.enabled);
    assert!(config.triggers.snapshot.enabled);
    assert!(config.filters.enabled);
    assert!(config.filters.disk_space.enabled);
    assert!(config.filters.rate_limiter.enabled);
    assert!(config.collectors.enabled);
    assert!(config.collectors.thread.enabled);
    assert!(!config.collectors.thread.stack_memory);
    assert!(!config.collectors.memory.enabled);
    assert!(!config.collectors.screenshot.enabled);
    assert!(!config.collectors.attachment.enabled);
    assert!(!config.collectors.environment.enabled);
    assert!(config.pre_processors.enabled);
    assert!(config.pre_processors.fingerprint.enabled);
    assert!(config.pre_processors.duplicate.enabled);
    assert!(config.pre_processors.sanitizer.enabled);
    assert!(config.post_processors.enabled);
    assert!(config.post_processors.png_converter.enabled);
    assert!(config.post_processors.zip_archiver.enabled);
    assert!(config.post_processors.move_to_sent.enabled);
    assert!(config.post_processors.retention.enabled);
    assert!(config.notifiers.enabled);
    assert!(config.notifiers.console.enabled);
}

#[test]
fn test_system_notification_default_disabled() {
    let config = CrashReporterConfig::default();
    assert!(!config.notifiers.system_notification.enabled);
}

#[test]
fn test_default_validation_collects_no_sensitive_data() {
    let validated = CrashReporterConfig::default().validate().unwrap();

    for plugin_id in [
        "MemoryCollector",
        "ScreenshotCollector",
        "AttachmentCollector",
        "EnvironmentCollector",
        "ProcessOutputCollector",
    ] {
        assert!(!validated.plugin_enabled(plugin_id));
    }
    assert_eq!(validated.collection_policy(), CollectionPolicy::MINIMAL);
    assert!(validated.diagnostics().is_empty());
}

#[test]
fn property_stack_memory_requires_profile_consent_and_explicit_opt_in() {
    for (level, consent, expected) in [
        ("minimal", "not_granted", false),
        ("minimal", "granted", false),
        ("diagnostic", "not_granted", false),
        ("diagnostic", "granted", true),
        ("full", "not_granted", false),
        ("full", "granted", true),
    ] {
        let json = format!(
            r#"{{
                "privacy": {{ "level": "{level}", "consent": "{consent}" }},
                "collectors": {{ "thread": {{ "enabled": true, "stack_memory": true }} }}
            }}"#
        );
        let validated = serde_json::from_str::<CrashReporterConfig>(&json)
            .unwrap()
            .validate()
            .unwrap();
        assert_eq!(
            validated.collection_policy().capture_stack_memory,
            expected,
            "level={level}, consent={consent}"
        );
        assert_eq!(
            validated.diagnostics().iter().any(|diagnostic| matches!(
                diagnostic,
                ConfigValidationDiagnostic::SensitiveEvidenceDenied {
                    evidence: "thread stack memory",
                    ..
                }
            )),
            !expected,
            "level={level}, consent={consent}"
        );
    }

    let profile_without_toggle = serde_json::from_str::<CrashReporterConfig>(
        r#"{ "privacy": { "level": "diagnostic", "consent": "granted" } }"#,
    )
    .unwrap()
    .validate()
    .unwrap();
    assert!(
        !profile_without_toggle
            .collection_policy()
            .capture_stack_memory
    );
}

#[test]
fn property_raw_shm_requires_full_consent_and_explicit_opt_in() {
    for (level, consent, expected) in [
        ("minimal", "not_granted", false),
        ("minimal", "granted", false),
        ("diagnostic", "not_granted", false),
        ("diagnostic", "granted", false),
        ("full", "not_granted", false),
        ("full", "granted", true),
    ] {
        let json = format!(
            r#"{{
                "privacy": {{
                    "level": "{level}",
                    "consent": "{consent}",
                    "raw_shm": true
                }}
            }}"#
        );
        let validated = serde_json::from_str::<CrashReporterConfig>(&json)
            .unwrap()
            .validate()
            .unwrap();
        assert_eq!(
            validated.collection_policy().persist_raw_shm,
            expected,
            "level={level}, consent={consent}"
        );
        assert_eq!(
            validated.diagnostics().iter().any(|diagnostic| matches!(
                diagnostic,
                ConfigValidationDiagnostic::SensitiveEvidenceDenied {
                    evidence: "raw shared-memory breadcrumbs/context",
                    ..
                }
            )),
            !expected,
            "level={level}, consent={consent}"
        );
    }

    let profile_without_toggle = serde_json::from_str::<CrashReporterConfig>(
        r#"{ "privacy": { "level": "full", "consent": "granted" } }"#,
    )
    .unwrap()
    .validate()
    .unwrap();
    assert!(!profile_without_toggle.collection_policy().persist_raw_shm);
}

#[test]
fn test_legacy_sensitive_toggles_do_not_bypass_missing_privacy_consent() {
    let config: CrashReporterConfig = serde_json::from_str(
        r#"{
            "collectors": {
                "memory": { "enabled": true },
                "screenshot": { "enabled": true },
                "attachment": { "enabled": true },
                "environment": { "enabled": true }
            }
        }"#,
    )
    .unwrap();
    let validated = config.validate().unwrap();

    for plugin_id in [
        "MemoryCollector",
        "ScreenshotCollector",
        "AttachmentCollector",
        "EnvironmentCollector",
    ] {
        assert!(!validated.plugin_enabled(plugin_id));
        assert!(validated.diagnostics().iter().any(|diagnostic| matches!(
            diagnostic,
            ConfigValidationDiagnostic::SensitiveCollectorDisabled {
                plugin_id: disabled,
                level: PrivacyLevel::Minimal,
                consent: ConsentState::NotGranted,
            } if disabled == plugin_id
        )));
    }
    assert_eq!(validated.diagnostics().len(), 4);
}

#[test]
fn property_privacy_profile_and_consent_gate_sensitive_collectors() {
    let cases = [
        ("minimal", "not_granted", &[][..]),
        ("minimal", "granted", &[][..]),
        ("diagnostic", "not_granted", &[][..]),
        ("diagnostic", "granted", &["MemoryCollector"][..]),
        ("full", "not_granted", &[][..]),
        (
            "full",
            "granted",
            &[
                "MemoryCollector",
                "ScreenshotCollector",
                "AttachmentCollector",
                "EnvironmentCollector",
                "ProcessOutputCollector",
            ][..],
        ),
    ];

    for (level, consent, expected) in cases {
        let json = format!(
            r#"{{
                "privacy": {{ "level": "{level}", "consent": "{consent}" }},
                "collectors": {{
                    "memory": {{ "enabled": true }},
                    "screenshot": {{ "enabled": true }},
                    "attachment": {{ "enabled": true }},
                    "environment": {{ "enabled": true }},
                    "process_output": {{ "enabled": true }}
                }}
            }}"#
        );
        let validated = serde_json::from_str::<CrashReporterConfig>(&json)
            .unwrap()
            .validate()
            .unwrap();

        for plugin_id in [
            "MemoryCollector",
            "ScreenshotCollector",
            "AttachmentCollector",
            "EnvironmentCollector",
            "ProcessOutputCollector",
        ] {
            assert_eq!(
                validated.plugin_enabled(plugin_id),
                expected.contains(&plugin_id),
                "level={level}, consent={consent}, plugin={plugin_id}"
            );
        }
        assert_eq!(
            validated.collection_policy().capture_shm_screenshots,
            expected.contains(&"ScreenshotCollector")
        );
        assert_eq!(
            validated.collection_policy().capture_shm_attachments,
            expected.contains(&"AttachmentCollector")
        );
        assert_eq!(validated.diagnostics().len(), 5 - expected.len());
    }
}

#[test]
fn test_profile_and_consent_do_not_auto_enable_sensitive_collectors() {
    let profile_only = serde_json::from_str::<CrashReporterConfig>(
        r#"{ "privacy": { "level": "full", "consent": "granted" } }"#,
    )
    .unwrap()
    .validate()
    .unwrap();
    for plugin_id in [
        "MemoryCollector",
        "ScreenshotCollector",
        "AttachmentCollector",
        "EnvironmentCollector",
        "ProcessOutputCollector",
    ] {
        assert!(!profile_only.plugin_enabled(plugin_id));
    }

    let memory_opt_in = serde_json::from_str::<CrashReporterConfig>(
        r#"{
            "privacy": { "level": "full", "consent": "granted" },
            "collectors": { "memory": { "enabled": true } }
        }"#,
    )
    .unwrap()
    .validate()
    .unwrap();

    assert!(memory_opt_in.plugin_enabled("MemoryCollector"));
    assert!(!memory_opt_in.plugin_enabled("ScreenshotCollector"));
    assert!(!memory_opt_in.plugin_enabled("AttachmentCollector"));
    assert!(!memory_opt_in.plugin_enabled("EnvironmentCollector"));
    assert!(!memory_opt_in.plugin_enabled("ProcessOutputCollector"));
    assert!(memory_opt_in.diagnostics().is_empty());
}

#[test]
fn test_required_application_encryption_fails_closed() {
    let error = serde_json::from_str::<CrashReporterConfig>(
        r#"{ "privacy": { "encryption": "required" } }"#,
    )
    .unwrap()
    .validate()
    .unwrap_err();

    assert_eq!(
        error,
        ConfigValidationError::ApplicationEncryptionUnavailable
    );
    assert!(error.to_string().contains("not implemented"));
}

#[test]
fn test_global_kill_switch_makes_encryption_requirement_vacuous() {
    let validated = serde_json::from_str::<CrashReporterConfig>(
        r#"{
            "enabled": false,
            "privacy": { "encryption": "required" }
        }"#,
    )
    .unwrap()
    .validate()
    .unwrap();

    assert!(!validated.enabled);
    assert!(validated.enabled_plugins.is_empty());
    assert_eq!(validated.collection_policy(), CollectionPolicy::MINIMAL);
}

#[test]
fn test_default_parameter_values() {
    let config = CrashReporterConfig::default();
    assert_eq!(config.filters.disk_space.min_free_mb, 100);
    assert_eq!(config.filters.rate_limiter.max_events, 3);
    assert_eq!(config.filters.rate_limiter.window_secs, 60);
    assert_eq!(config.pre_processors.fingerprint.top_frames, 8);
    assert_eq!(config.pre_processors.duplicate.window_secs, 60);
    assert_eq!(config.post_processors.log_rotator.max_size_mb, 1);
    assert_eq!(config.post_processors.retention.max_reports, 16);
    assert_eq!(config.post_processors.retention.max_size_mb, 64);
    assert_eq!(config.post_processors.retention.max_age_days, 7);
    assert_eq!(config.watchdog.warmup_ms, 10_000);
    assert_eq!(config.watchdog.threshold_ms, 5_000);
    assert_eq!(config.watchdog.check_interval_ms, 2_000);
    assert_eq!(config.watchdog.cooldown_ms, 60_000);
}

#[test]
fn unknown_fields_are_rejected_at_every_config_level() {
    for json in [
        r#"{"mystery":true}"#,
        r#"{"watchdog":{"mystery":1}}"#,
        r#"{"filters":{"rate_limiter":{"mystery":1}}}"#,
    ] {
        let error = serde_json::from_str::<CrashReporterConfig>(json).unwrap_err();
        assert!(error.to_string().contains("unknown field"), "{error}");
    }
}

#[test]
fn enabled_duration_and_count_ranges_are_validated() {
    for (json, expected_field) in [
        (
            r#"{"watchdog":{"threshold_ms":0}}"#,
            "watchdog.threshold_ms",
        ),
        (
            r#"{"filters":{"rate_limiter":{"window_secs":0}}}"#,
            "filters.rate_limiter.window_secs",
        ),
        (
            r#"{"pre_processors":{"fingerprint":{"top_frames":0}}}"#,
            "pre_processors.fingerprint.top_frames",
        ),
    ] {
        let error = serde_json::from_str::<CrashReporterConfig>(json)
            .unwrap()
            .validate()
            .unwrap_err();
        assert!(error.to_string().contains(expected_field), "{error}");
    }
}

#[test]
fn watchdog_duration_error_does_not_imply_an_enable_toggle() {
    let error = serde_json::from_str::<CrashReporterConfig>(r#"{"watchdog":{"threshold_ms":0}}"#)
        .unwrap()
        .validate()
        .unwrap_err();

    assert_eq!(
        error.to_string(),
        "watchdog.threshold_ms must be greater than zero"
    );
}

#[test]
fn watchdog_check_interval_cannot_exceed_threshold() {
    let error = serde_json::from_str::<CrashReporterConfig>(
        r#"{"watchdog":{"threshold_ms":100,"check_interval_ms":101}}"#,
    )
    .unwrap()
    .validate()
    .unwrap_err();

    assert_eq!(
        error.to_string(),
        "watchdog.check_interval_ms must not exceed watchdog.threshold_ms"
    );
}

#[test]
fn zero_rate_limit_and_retention_thresholds_keep_documented_sentinel_meaning() {
    let config = serde_json::from_str::<CrashReporterConfig>(
        r#"{
            "filters":{"rate_limiter":{"max_events":0}},
            "post_processors":{"retention":{"max_reports":1,"max_size_mb":0,"max_age_days":0}}
        }"#,
    )
    .unwrap()
    .validate()
    .unwrap();
    assert_eq!(config.config().filters.rate_limiter.max_events, 0);
    assert_eq!(config.config().post_processors.retention.max_size_mb, 0);
    assert_eq!(config.config().post_processors.retention.max_age_days, 0);
}

#[test]
fn retention_rejects_zero_report_limit_when_enabled() {
    let config: CrashReporterConfig = serde_json::from_str(
        r#"{"post_processors":{"retention":{"enabled":true,"max_reports":0}}}"#,
    )
    .unwrap();
    assert_eq!(
        config.validate().unwrap_err(),
        ConfigValidationError::RetentionMaxReportsZero
    );
}

fn private_config_tempdir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    crate::utils::paths::ensure_private_directory(dir.path()).unwrap();
    dir
}

#[test]
fn test_load_missing_file_returns_default() {
    let dir = private_config_tempdir();
    let config = super::load_config_from_path(&dir.path().join("missing.json")).unwrap();
    assert!(config.enabled);
    assert!(config.filters.enabled);
    assert!(config.collectors.enabled);
}

#[test]
fn test_load_valid_config() {
    let dir = private_config_tempdir();
    let path = dir.path().join("crash_reporter.json");
    std::fs::write(
        &path,
        r#"{
            "filters": {
                "disk_space": { "min_free_mb": 200 }
            },
            "notifiers": {
                "system_notification": { "enabled": true }
            }
        }"#,
    )
    .unwrap();

    let config = super::load_config_from_path(&path).unwrap();
    assert_eq!(config.filters.disk_space.min_free_mb, 200);
    assert!(config.notifiers.system_notification.enabled);
    // Unspecified fields keep defaults
    assert!(config.enabled);
    assert!(config.filters.enabled);
    assert!(config.filters.disk_space.enabled);
    assert_eq!(config.pre_processors.duplicate.window_secs, 60);
}

#[test]
fn loader_rejects_unknown_top_level_field() {
    let dir = private_config_tempdir();
    let path = dir.path().join("crash_reporter.json");
    std::fs::write(&path, r#"{ "unexpected": true }"#).unwrap();

    assert!(matches!(
        super::load_config_from_path(&path),
        Err(ConfigLoadError::Parse { .. })
    ));
}

#[test]
fn loader_rejects_range_violation() {
    let dir = private_config_tempdir();
    let path = dir.path().join("crash_reporter.json");
    std::fs::write(
        &path,
        r#"{ "post_processors": { "retention": { "max_reports": 0 } } }"#,
    )
    .unwrap();

    let loaded = super::load_config_from_path(&path).expect("syntax-level load succeeds");
    assert_eq!(
        loaded.validate().unwrap_err(),
        ConfigValidationError::RetentionMaxReportsZero
    );
}

#[test]
fn test_plugin_disabled() {
    let json = r#"{ "filters": { "disk_space": { "enabled": false } } }"#;
    let config: CrashReporterConfig = serde_json::from_str(json).unwrap();
    assert!(!config.filters.disk_space.enabled);
    // Other plugins remain enabled
    assert!(config.collectors.thread.enabled);
    assert!(config.pre_processors.fingerprint.enabled);
}

#[test]
fn test_category_disabled_overrides_plugin() {
    let json = r#"{ "filters": { "enabled": false, "disk_space": { "enabled": true } } }"#;
    let config: CrashReporterConfig = serde_json::from_str(json).unwrap();
    // Category is disabled, so even if plugin says enabled, is_enabled returns false
    assert!(!is_enabled(
        config.enabled,
        config.filters.enabled,
        config.filters.disk_space.enabled
    ));
}

#[test]
fn test_global_disabled_overrides_all() {
    let json = r#"{ "enabled": false }"#;
    let config: CrashReporterConfig = serde_json::from_str(json).unwrap();
    assert!(!config.enabled);
    assert!(!is_enabled(
        config.enabled,
        config.filters.enabled,
        config.filters.disk_space.enabled
    ));
    assert!(!is_enabled(
        config.enabled,
        config.collectors.enabled,
        config.collectors.thread.enabled
    ));
}

#[test]
fn test_validation_keeps_global_kill_switch_explicit() {
    let config: CrashReporterConfig = serde_json::from_str(r#"{ "enabled": false }"#).unwrap();
    let validated = config.validate().unwrap();

    assert!(!validated.enabled);
    // Per-trigger values retain their independently configured meaning. The
    // Pipeline combines them with the authoritative global switch.
    assert!(validated.triggers.crash);
    assert!(validated.triggers.exit_failure);
    assert!(validated.triggers.signal_failure);
    assert!(!validated.triggers.probable_oom);
    assert!(validated.triggers.anr);
    assert!(validated.triggers.snapshot);
}

#[test]
fn test_trigger_category_disables_every_report_source() {
    let config: CrashReporterConfig =
        serde_json::from_str(r#"{ "triggers": { "enabled": false } }"#).unwrap();
    let validated = config.validate().unwrap();

    assert!(validated.enabled);
    assert!(!validated.triggers.crash);
    assert!(!validated.triggers.exit_failure);
    assert!(!validated.triggers.signal_failure);
    assert!(!validated.triggers.probable_oom);
    assert!(!validated.triggers.anr);
    assert!(!validated.triggers.snapshot);
}

#[test]
fn test_each_trigger_is_resolved_independently() {
    let config: CrashReporterConfig = serde_json::from_str(
        r#"{
            "triggers": {
                "crash": { "enabled": false },
                "exit_failure": { "enabled": true },
                "signal_failure": { "enabled": false },
                "oom_detection": { "enabled": true },
                "anr": { "enabled": false },
                "snapshot": { "enabled": true }
            }
        }"#,
    )
    .unwrap();
    let validated = config.validate().unwrap();

    assert!(!validated.triggers.crash);
    assert!(validated.triggers.exit_failure);
    assert!(!validated.triggers.signal_failure);
    assert!(validated.triggers.probable_oom);
    assert!(!validated.triggers.anr);
    assert!(validated.triggers.snapshot);
}

#[test]
fn test_legacy_partial_trigger_config_defaults_new_triggers_on() {
    let config: CrashReporterConfig =
        serde_json::from_str(r#"{ "triggers": { "oom_detection": { "enabled": false } } }"#)
            .unwrap();
    let validated = config.validate().unwrap();

    assert!(validated.triggers.crash);
    assert!(validated.triggers.exit_failure);
    assert!(validated.triggers.signal_failure);
    assert!(!validated.triggers.probable_oom);
    assert!(validated.triggers.anr);
    assert!(validated.triggers.snapshot);
}

#[test]
fn test_malformed_json_fails_closed() {
    let dir = private_config_tempdir();
    let path = dir.path().join("crash_reporter.json");
    std::fs::write(&path, "{ not valid json }}}").unwrap();

    let result = super::load_config_from_path(&path);
    assert!(matches!(result, Err(ConfigLoadError::Parse { .. })));
}

#[test]
fn test_existing_non_regular_config_fails_closed() {
    let dir = private_config_tempdir();
    let result = super::load_config_from_path(dir.path());
    assert!(matches!(result, Err(ConfigLoadError::UnsafeFile { .. })));
}

#[test]
fn test_existing_unreadable_config_fails_closed() {
    use std::os::unix::fs::PermissionsExt;

    // Root can read mode-000 files, so this permission test has no meaningful
    // unreadable state under a root test runner.
    // SAFETY: `geteuid` has no arguments or memory-safety preconditions.
    if unsafe { nix::libc::geteuid() } == 0 {
        return;
    }

    let dir = private_config_tempdir();
    let path = dir.path().join("crash_reporter.json");
    std::fs::write(&path, r#"{ "enabled": false }"#).unwrap();
    let original = std::fs::metadata(&path).unwrap().permissions();
    let mut unreadable = original.clone();
    unreadable.set_mode(0o000);
    std::fs::set_permissions(&path, unreadable).unwrap();

    let result = super::load_config_from_path(&path);
    std::fs::set_permissions(&path, original).unwrap();
    assert!(matches!(
        result,
        Err(ConfigLoadError::Read { .. } | ConfigLoadError::UnsafeFile { .. })
    ));
}

#[test]
fn test_config_symlink_is_rejected_without_following() {
    let dir = private_config_tempdir();
    let target = dir.path().join("target.json");
    let link = dir.path().join("crash_reporter.json");
    std::fs::write(&target, r#"{ "enabled": false }"#).unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let result = super::load_config_from_path(&link);
    assert!(matches!(result, Err(ConfigLoadError::UnsafeFile { .. })));
}

#[test]
fn test_broken_config_symlink_is_not_mistaken_for_missing_file() {
    let dir = private_config_tempdir();
    let link = dir.path().join("crash_reporter.json");
    std::os::unix::fs::symlink(dir.path().join("absent-target.json"), &link).unwrap();

    let result = super::load_config_from_path(&link);
    assert!(matches!(result, Err(ConfigLoadError::UnsafeFile { .. })));
}

#[test]
fn test_config_parent_symlink_is_rejected_without_following() {
    let dir = private_config_tempdir();
    let target = dir.path().join("target");
    let link = dir.path().join("config-link");
    std::fs::create_dir(&target).unwrap();
    std::fs::write(
        target.join("crash_reporter.json"),
        r#"{ "enabled": false }"#,
    )
    .unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let result = super::load_config_from_path(&link.join("crash_reporter.json"));
    assert!(matches!(result, Err(ConfigLoadError::UnsafeFile { .. })));
}

#[test]
fn test_required_encryption_cannot_disappear_through_unrelated_parse_error() {
    let dir = private_config_tempdir();
    let path = dir.path().join("crash_reporter.json");
    std::fs::write(
        &path,
        r#"{
            "privacy": { "encryption": "required" },
            "filters": { "disk_space": { "min_free_mb": "invalid" } }
        }"#,
    )
    .unwrap();

    let result = super::load_config_from_path(&path);
    assert!(matches!(result, Err(ConfigLoadError::Parse { .. })));
}

#[test]
fn test_legacy_thread_toggle_shape_remains_compatible() {
    let config = serde_json::from_str::<CrashReporterConfig>(
        r#"{ "collectors": { "thread": { "enabled": false } } }"#,
    )
    .unwrap();
    assert!(!config.collectors.thread.enabled);
    assert!(!config.collectors.thread.stack_memory);
}

#[test]
fn test_partial_config_merges_with_defaults() {
    let json = r#"{ "post_processors": { "retention": { "max_reports": 32 } } }"#;
    let config: CrashReporterConfig = serde_json::from_str(json).unwrap();
    // Specified field changed
    assert_eq!(config.post_processors.retention.max_reports, 32);
    // Unspecified fields in same struct keep defaults
    assert_eq!(config.post_processors.retention.max_size_mb, 64);
    assert_eq!(config.post_processors.retention.max_age_days, 7);
    assert!(config.post_processors.retention.enabled);
    // Other categories untouched
    assert!(config.filters.enabled);
    assert_eq!(config.filters.disk_space.min_free_mb, 100);
}

#[test]
fn test_is_enabled_all_true() {
    assert!(is_enabled(true, true, true));
}

#[test]
fn test_is_enabled_any_false() {
    assert!(!is_enabled(false, true, true));
    assert!(!is_enabled(true, false, true));
    assert!(!is_enabled(true, true, false));
}

#[test]
fn test_empty_json_object_returns_defaults() {
    let config: CrashReporterConfig = serde_json::from_str("{}").unwrap();
    assert!(config.enabled);
    assert!(config.filters.enabled);
    assert!(config.filters.disk_space.enabled);
    assert_eq!(config.filters.disk_space.min_free_mb, 100);
    assert!(!config.notifiers.system_notification.enabled);
}

#[test]
fn property_toggle_combinations_preserve_opt_out_and_close_hard_dependencies() {
    for global in [false, true] {
        for category in [false, true] {
            for fingerprint in [false, true] {
                for duplicate in [false, true] {
                    let json = format!(
                        r#"{{
                            "enabled": {global},
                            "pre_processors": {{
                                "enabled": {category},
                                "fingerprint": {{ "enabled": {fingerprint} }},
                                "duplicate": {{ "enabled": {duplicate} }}
                            }}
                        }}"#
                    );
                    let validated = serde_json::from_str::<CrashReporterConfig>(&json)
                        .unwrap()
                        .validate()
                        .unwrap();

                    assert_eq!(
                        validated.plugin_enabled("Fingerprinter"),
                        global && category && fingerprint
                    );
                    assert_eq!(
                        validated.plugin_enabled("DuplicateDetector"),
                        global && category && duplicate && fingerprint
                    );
                    let expected_diagnostic = global && category && duplicate && !fingerprint;
                    assert_eq!(
                        validated.diagnostics().len(),
                        usize::from(expected_diagnostic)
                    );
                    if expected_diagnostic {
                        assert!(matches!(
                            &validated.diagnostics()[0],
                            ConfigValidationDiagnostic::DependentDisabled {
                                category: crate::pipeline::PluginCategory::PreProcessor,
                                plugin_id,
                                dependency,
                            } if plugin_id == "DuplicateDetector" && dependency == "Fingerprinter"
                        ));
                    }
                }
            }
        }
    }
}

#[test]
fn property_order_only_dependencies_never_change_enablement() {
    for symbolizer in [false, true] {
        for fingerprint in [false, true] {
            for sanitizer in [false, true] {
                for feedback in [false, true] {
                    for zip in [false, true] {
                        let json = format!(
                            r#"{{
                                "pre_processors": {{
                                    "symbolizer": {{ "enabled": {symbolizer} }},
                                    "fingerprint": {{ "enabled": {fingerprint} }},
                                    "duplicate": {{ "enabled": false }},
                                    "sanitizer": {{ "enabled": {sanitizer} }}
                                }},
                                "post_processors": {{
                                    "feedback_dialog": {{ "enabled": {feedback} }},
                                    "zip_archiver": {{ "enabled": {zip} }}
                                }}
                            }}"#
                        );
                        let validated = serde_json::from_str::<CrashReporterConfig>(&json)
                            .unwrap()
                            .validate()
                            .unwrap();
                        assert_eq!(validated.plugin_enabled("SymbolResolver"), symbolizer);
                        assert_eq!(validated.plugin_enabled("Fingerprinter"), fingerprint);
                        assert_eq!(validated.plugin_enabled("Sanitizer"), sanitizer);
                        assert_eq!(validated.plugin_enabled("FeedbackDialog"), feedback);
                        assert_eq!(validated.plugin_enabled("ZIPArchiver"), zip);
                        assert!(validated.diagnostics().is_empty());
                    }
                }
            }
        }
    }
}

const CONFIG_PLUGIN_PATHS: &[(&str, &str, &str)] = &[
    ("DiskSpaceFilter", "filters", "disk_space"),
    ("RateLimiter", "filters", "rate_limiter"),
    ("ThreadCollector", "collectors", "thread"),
    ("BreadcrumbCollector", "collectors", "breadcrumb"),
    ("ContextCollector", "collectors", "context"),
    ("MemoryCollector", "collectors", "memory"),
    ("DylibCollector", "collectors", "dylib"),
    ("ScreenshotCollector", "collectors", "screenshot"),
    ("AttachmentCollector", "collectors", "attachment"),
    ("EnvironmentCollector", "collectors", "environment"),
    ("ProcessOutputCollector", "collectors", "process_output"),
    ("SessionEnricher", "pre_processors", "session"),
    ("SymbolResolver", "pre_processors", "symbolizer"),
    ("Fingerprinter", "pre_processors", "fingerprint"),
    ("BuildInfoEnricher", "pre_processors", "build_info"),
    ("DuplicateDetector", "pre_processors", "duplicate"),
    ("Sanitizer", "pre_processors", "sanitizer"),
    ("RawCleanup", "post_processors", "raw_cleanup"),
    ("SessionRecorder", "post_processors", "session_recorder"),
    ("PNGConverter", "post_processors", "png_converter"),
    ("FeedbackDialog", "post_processors", "feedback_dialog"),
    ("ZIPArchiver", "post_processors", "zip_archiver"),
    ("MoveToSent", "post_processors", "move_to_sent"),
    ("LogRotator", "post_processors", "log_rotator"),
    ("RetentionManager", "post_processors", "retention"),
    ("ConsoleNotifier", "notifiers", "console"),
    ("SystemNotification", "notifiers", "system_notification"),
];

fn config_with_json_toggle(category: &str, plugin: &str, enabled: bool) -> CrashReporterConfig {
    let mut plugin_config = serde_json::Map::new();
    plugin_config.insert("enabled".to_string(), serde_json::Value::Bool(enabled));
    let mut category_config = serde_json::Map::new();
    category_config.insert(plugin.to_string(), serde_json::Value::Object(plugin_config));
    let mut root = serde_json::Map::new();
    root.insert(
        category.to_string(),
        serde_json::Value::Object(category_config),
    );
    serde_json::from_value(serde_json::Value::Object(root)).unwrap()
}

#[test]
fn property_every_configured_plugin_toggle_maps_to_exactly_its_runtime_id() {
    assert_eq!(CONFIG_PLUGIN_PATHS.len(), PLUGIN_SPECS.len());
    let registry_ids: std::collections::BTreeSet<&str> =
        PLUGIN_SPECS.iter().map(|spec| spec.id).collect();
    let config_ids: std::collections::BTreeSet<&str> =
        CONFIG_PLUGIN_PATHS.iter().map(|(id, _, _)| *id).collect();
    assert_eq!(registry_ids, config_ids);

    for &(disabled_id, category, plugin) in CONFIG_PLUGIN_PATHS {
        let config = config_with_json_toggle(category, plugin, false);
        let requested = configured_plugin_toggles(&config);
        let validated = config.validate().unwrap();

        for (plugin_id, was_requested) in requested {
            let hard_closure_disables =
                disabled_id == "Fingerprinter" && plugin_id == "DuplicateDetector";
            assert_eq!(
                validated.plugin_enabled(plugin_id),
                was_requested && !hard_closure_disables,
                "toggle for {disabled_id} mapped incorrectly to {plugin_id}"
            );
        }
        assert!(!validated.plugin_enabled(disabled_id));
    }
}

#[test]
fn property_global_and_all_five_category_masks_resolve_every_plugin() {
    for global in [false, true] {
        for category_mask in 0_u8..32 {
            let config = CrashReporterConfig {
                enabled: global,
                filters: FilterConfig {
                    enabled: category_mask & 0b00001 != 0,
                    ..FilterConfig::default()
                },
                collectors: CollectorConfig {
                    enabled: category_mask & 0b00010 != 0,
                    ..CollectorConfig::default()
                },
                pre_processors: PreProcessorConfig {
                    enabled: category_mask & 0b00100 != 0,
                    ..PreProcessorConfig::default()
                },
                post_processors: PostProcessorConfig {
                    enabled: category_mask & 0b01000 != 0,
                    ..PostProcessorConfig::default()
                },
                notifiers: NotifierConfig {
                    enabled: category_mask & 0b10000 != 0,
                    ..NotifierConfig::default()
                },
                ..CrashReporterConfig::default()
            };
            let requested = configured_plugin_toggles(&config);
            let validated = config.validate().unwrap();

            for (plugin_id, category_requested) in requested {
                assert_eq!(
                    validated.plugin_enabled(plugin_id),
                    global && category_requested,
                    "global={global}, category_mask={category_mask:#07b}, plugin={plugin_id}"
                );
            }
            assert!(validated.diagnostics().is_empty());
        }
    }
}

fn config_with_category_plugin_mask(
    category: &str,
    plugins: &[(&str, &str, &str)],
    mask: usize,
) -> CrashReporterConfig {
    let mut category_config = serde_json::Map::new();
    category_config.insert("enabled".to_string(), serde_json::Value::Bool(true));
    for (bit, (_, _, plugin_key)) in plugins.iter().enumerate() {
        let mut plugin_config = serde_json::Map::new();
        plugin_config.insert(
            "enabled".to_string(),
            serde_json::Value::Bool(mask & (1 << bit) != 0),
        );
        category_config.insert(
            (*plugin_key).to_string(),
            serde_json::Value::Object(plugin_config),
        );
    }
    let mut root = serde_json::Map::new();
    root.insert(
        "privacy".to_string(),
        serde_json::json!({ "level": "full", "consent": "granted" }),
    );
    root.insert(
        category.to_string(),
        serde_json::Value::Object(category_config),
    );
    serde_json::from_value(serde_json::Value::Object(root)).unwrap()
}

#[test]
fn property_every_category_plugin_mask_resolves_exactly() {
    for (category, expected_plugin_count) in [
        ("filters", 2_usize),
        ("collectors", 9),
        ("pre_processors", 6),
        ("post_processors", 8),
        ("notifiers", 2),
    ] {
        let plugins: Vec<_> = CONFIG_PLUGIN_PATHS
            .iter()
            .copied()
            .filter(|(_, plugin_category, _)| *plugin_category == category)
            .collect();
        assert_eq!(plugins.len(), expected_plugin_count);

        for mask in 0..(1_usize << plugins.len()) {
            let config = config_with_category_plugin_mask(category, &plugins, mask);
            let validated = config.validate().unwrap();
            let fingerprint_enabled = plugins
                .iter()
                .position(|(id, _, _)| *id == "Fingerprinter")
                .is_none_or(|bit| mask & (1 << bit) != 0);

            for (bit, (plugin_id, _, _)) in plugins.iter().enumerate() {
                let requested = mask & (1 << bit) != 0;
                let expected =
                    requested && (*plugin_id != "DuplicateDetector" || fingerprint_enabled);
                assert_eq!(
                    validated.plugin_enabled(plugin_id),
                    expected,
                    "category={category}, mask={mask:#010b}, plugin={plugin_id}"
                );
            }

            let duplicate_requested = plugins
                .iter()
                .position(|(id, _, _)| *id == "DuplicateDetector")
                .is_some_and(|bit| mask & (1 << bit) != 0);
            assert_eq!(
                validated.diagnostics().len(),
                usize::from(duplicate_requested && !fingerprint_enabled),
                "category={category}, mask={mask:#010b}"
            );
        }
    }
}

#[test]
fn property_hard_dependency_closure_is_transitive_and_never_auto_enables() {
    use std::collections::BTreeSet;

    const SPECS: &[PluginSpec] = &[
        PluginSpec {
            category: crate::pipeline::PluginCategory::PreProcessor,
            id: "C",
            hard_dependencies: NO_DEPS,
            order_dependencies: NO_DEPS,
        },
        PluginSpec {
            category: crate::pipeline::PluginCategory::PreProcessor,
            id: "B",
            hard_dependencies: &["C"],
            order_dependencies: NO_DEPS,
        },
        PluginSpec {
            category: crate::pipeline::PluginCategory::PreProcessor,
            id: "A",
            hard_dependencies: &["B"],
            order_dependencies: NO_DEPS,
        },
    ];

    for mask in 0_u8..8 {
        let requested: BTreeSet<&'static str> = ["A", "B", "C"]
            .into_iter()
            .enumerate()
            .filter_map(|(bit, id)| (mask & (1 << bit) != 0).then_some(id))
            .collect();
        let (closed, diagnostics) = close_plugin_enablement(SPECS, requested.clone());

        assert!(
            closed.is_subset(&requested),
            "closure auto-enabled a plugin"
        );
        assert!(!closed.contains("A") || closed.contains("B"));
        assert!(!closed.contains("B") || closed.contains("C"));
        assert_eq!(diagnostics.len(), requested.len() - closed.len());

        let (closed_again, second_diagnostics) = close_plugin_enablement(SPECS, closed.clone());
        assert_eq!(
            closed_again, closed,
            "dependency closure was not idempotent"
        );
        assert!(second_diagnostics.is_empty());
    }
}

fn graph_node(id: impl Into<String>, hard: &[&str], order: &[&str]) -> PluginGraphNode {
    PluginGraphNode {
        id: id.into(),
        hard_dependencies: hard
            .iter()
            .map(|dependency| (*dependency).to_string())
            .collect(),
        order_dependencies: order
            .iter()
            .map(|dependency| (*dependency).to_string())
            .collect(),
    }
}

#[test]
fn property_duplicate_plugin_ids_are_rejected_in_every_category() {
    use crate::pipeline::PluginCategory;

    for category in [
        PluginCategory::Filter,
        PluginCategory::Collector,
        PluginCategory::PreProcessor,
        PluginCategory::PostProcessor,
        PluginCategory::Notifier,
    ] {
        for unique_prefix_len in 0..8 {
            let mut nodes: Vec<PluginGraphNode> = (0..unique_prefix_len)
                .map(|index| graph_node(format!("plugin-{index}"), &[], &[]))
                .collect();
            nodes.push(graph_node("duplicate", &[], &[]));
            nodes.push(graph_node("duplicate", &[], &[]));
            assert!(matches!(
                validate_plugin_graph(category, &nodes),
                Err(ConfigValidationError::DuplicatePluginId { ref plugin_id, .. })
                    if plugin_id == "duplicate"
            ));
        }
    }
}

#[test]
fn property_complete_registry_rejects_missing_dependencies_but_runtime_allows_absent_order_only() {
    use crate::pipeline::{DependencyKind, PluginCategory};

    for category in [
        PluginCategory::Filter,
        PluginCategory::Collector,
        PluginCategory::PreProcessor,
        PluginCategory::PostProcessor,
        PluginCategory::Notifier,
    ] {
        for suffix in 0..8 {
            let missing = format!("missing-{suffix}");
            let hard = vec![graph_node("dependent", &[missing.as_str()], &[])];
            assert!(matches!(
                validate_plugin_graph(category, &hard),
                Err(ConfigValidationError::MissingDependency {
                    kind: DependencyKind::Hard,
                    ref dependency,
                    ..
                }) if dependency == &missing
            ));

            let order_only = vec![graph_node("dependent", &[], &[missing.as_str()])];
            assert!(matches!(
                validate_plugin_graph(category, &order_only),
                Err(ConfigValidationError::MissingDependency {
                    kind: DependencyKind::OrderOnly,
                    ref dependency,
                    ..
                }) if dependency == &missing
            ));
            validate_runtime_plugin_graph(category, &order_only).unwrap();
        }
    }
}

#[test]
fn property_hard_and_order_only_cycles_are_rejected_without_panicking() {
    use crate::pipeline::PluginCategory;

    for hard_cycle in [false, true] {
        for cycle_len in 1..=8 {
            let ids: Vec<String> = (0..cycle_len).map(|index| format!("p{index}")).collect();
            let nodes: Vec<PluginGraphNode> = (0..cycle_len)
                .map(|index| {
                    let dependency = ids[(index + 1) % cycle_len].as_str();
                    if hard_cycle {
                        graph_node(ids[index].clone(), &[dependency], &[])
                    } else {
                        graph_node(ids[index].clone(), &[], &[dependency])
                    }
                })
                .collect();
            assert!(matches!(
                validate_plugin_graph(PluginCategory::PreProcessor, &nodes),
                Err(ConfigValidationError::DependencyCycle { .. })
            ));
        }
    }
}

#[test]
fn property_invalid_hard_and_order_only_registration_is_structured_for_every_category() {
    use crate::pipeline::{DependencyKind, PluginCategory};

    for category in [
        PluginCategory::Filter,
        PluginCategory::Collector,
        PluginCategory::PreProcessor,
        PluginCategory::PostProcessor,
        PluginCategory::Notifier,
    ] {
        for kind in [DependencyKind::Hard, DependencyKind::OrderOnly] {
            let dependent = match kind {
                DependencyKind::Hard => graph_node("dependent", &["provider"], &[]),
                DependencyKind::OrderOnly => graph_node("dependent", &[], &["provider"]),
            };
            let nodes = vec![dependent, graph_node("provider", &[], &[])];
            assert!(matches!(
                validate_plugin_graph(category, &nodes),
                Err(ConfigValidationError::InvalidDependencyOrder {
                    category: error_category,
                    kind: error_kind,
                    ref plugin_id,
                    ref dependency,
                }) if error_category == category
                    && error_kind == kind
                    && plugin_id == "dependent"
                    && dependency == "provider"
            ));
        }
    }
}

#[test]
fn property_mixed_hard_and_order_only_dag_validates_in_topological_order() {
    use crate::pipeline::PluginCategory;

    for category in [
        PluginCategory::Filter,
        PluginCategory::Collector,
        PluginCategory::PreProcessor,
        PluginCategory::PostProcessor,
        PluginCategory::Notifier,
    ] {
        let nodes = vec![
            graph_node("source", &[], &[]),
            graph_node("ordered", &[], &["source"]),
            graph_node("hard", &["source"], &[]),
            graph_node("leaf", &["hard"], &["ordered"]),
        ];
        validate_plugin_graph(category, &nodes).unwrap();
    }
}

#[test]
fn property_mixed_hard_and_order_only_cycle_is_rejected() {
    use crate::pipeline::PluginCategory;

    let nodes = vec![
        graph_node("A", &["B"], &[]),
        graph_node("B", &[], &["C"]),
        graph_node("C", &["A"], &[]),
    ];
    assert!(matches!(
        validate_plugin_graph(PluginCategory::PreProcessor, &nodes),
        Err(ConfigValidationError::DependencyCycle { ref cycle, .. })
            if cycle.first() == cycle.last()
                && ["A", "B", "C"].iter().all(|id| cycle.iter().any(|node| node == id))
    ));
}

#[test]
fn property_plugin_ids_are_globally_unique_across_categories() {
    use crate::pipeline::PluginCategory;

    let registry = vec![
        (
            PluginCategory::Collector,
            vec![graph_node("shared-id", &[], &[])],
        ),
        (
            PluginCategory::Notifier,
            vec![graph_node("shared-id", &[], &[])],
        ),
    ];
    assert!(matches!(
        validate_plugin_registry(&registry),
        Err(ConfigValidationError::DuplicatePluginId {
            ref plugin_id,
            first_category: PluginCategory::Collector,
            second_category: PluginCategory::Notifier,
        }) if plugin_id == "shared-id"
    ));
}
