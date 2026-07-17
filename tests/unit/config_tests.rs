use crate::config::*;

#[test]
fn test_default_config_all_enabled() {
    let config = CrashReporterConfig::default();
    assert!(config.enabled);
    assert!(config.triggers.enabled);
    assert!(config.triggers.crash.enabled);
    assert!(config.triggers.exit_failure.enabled);
    assert!(config.triggers.signal_failure.enabled);
    assert!(config.triggers.oom_detection.enabled);
    assert!(config.triggers.anr.enabled);
    assert!(config.triggers.snapshot.enabled);
    assert!(config.filters.enabled);
    assert!(config.filters.disk_space.enabled);
    assert!(config.filters.rate_limiter.enabled);
    assert!(config.collectors.enabled);
    assert!(config.collectors.thread.enabled);
    assert!(config.collectors.environment.enabled);
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
fn test_default_parameter_values() {
    let config = CrashReporterConfig::default();
    assert_eq!(config.filters.disk_space.min_free_mb, 100);
    assert_eq!(config.filters.rate_limiter.max_events, 3);
    assert_eq!(config.filters.rate_limiter.window_secs, 60);
    assert_eq!(config.pre_processors.fingerprint.top_frames, 8);
    assert_eq!(config.pre_processors.duplicate.window_secs, 60);
    assert_eq!(config.post_processors.log_rotator.max_size_mb, 1);
    assert_eq!(config.post_processors.retention.max_reports, 64);
    assert_eq!(config.post_processors.retention.max_size_mb, 256);
    assert_eq!(config.post_processors.retention.max_age_days, 15);
}

#[test]
fn test_load_missing_file_returns_default() {
    let config = load_config();
    assert!(config.enabled);
    assert!(config.filters.enabled);
    assert!(config.collectors.enabled);
}

#[test]
fn test_load_valid_config() {
    let dir = tempfile::tempdir().unwrap();
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

    let config: CrashReporterConfig =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(config.filters.disk_space.min_free_mb, 200);
    assert!(config.notifiers.system_notification.enabled);
    // Unspecified fields keep defaults
    assert!(config.enabled);
    assert!(config.filters.enabled);
    assert!(config.filters.disk_space.enabled);
    assert_eq!(config.pre_processors.duplicate.window_secs, 60);
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
    let validated = config.validate();

    assert!(!validated.enabled);
    // Per-trigger values retain their independently configured meaning. The
    // Pipeline combines them with the authoritative global switch.
    assert!(validated.triggers.crash);
    assert!(validated.triggers.exit_failure);
    assert!(validated.triggers.signal_failure);
    assert!(validated.triggers.probable_oom);
    assert!(validated.triggers.anr);
    assert!(validated.triggers.snapshot);
}

#[test]
fn test_trigger_category_disables_every_report_source() {
    let config: CrashReporterConfig =
        serde_json::from_str(r#"{ "triggers": { "enabled": false } }"#).unwrap();
    let validated = config.validate();

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
    let validated = config.validate();

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
    let validated = config.validate();

    assert!(validated.triggers.crash);
    assert!(validated.triggers.exit_failure);
    assert!(validated.triggers.signal_failure);
    assert!(!validated.triggers.probable_oom);
    assert!(validated.triggers.anr);
    assert!(validated.triggers.snapshot);
}

#[test]
fn test_malformed_json_returns_default() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("crash_reporter.json");
    std::fs::write(&path, "{ not valid json }}}").unwrap();

    // load_config_from_path returns None on parse error
    let result = super::load_config_from_path(&path);
    assert!(result.is_none());
}

#[test]
fn test_partial_config_merges_with_defaults() {
    let json = r#"{ "post_processors": { "retention": { "max_reports": 32 } } }"#;
    let config: CrashReporterConfig = serde_json::from_str(json).unwrap();
    // Specified field changed
    assert_eq!(config.post_processors.retention.max_reports, 32);
    // Unspecified fields in same struct keep defaults
    assert_eq!(config.post_processors.retention.max_size_mb, 256);
    assert_eq!(config.post_processors.retention.max_age_days, 15);
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
