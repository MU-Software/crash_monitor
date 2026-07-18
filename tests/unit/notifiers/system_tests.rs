use crate::notifiers::SystemNotification;
use crate::pipeline::{Notifier, Plugin, PluginContext};
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;

fn mock_program(dir: &std::path::Path, body: &str) -> std::path::PathBuf {
    let path = dir.join("mock-osascript");
    let mut file = std::fs::File::create(&path).unwrap();
    writeln!(file, "#!/bin/bash").unwrap();
    writeln!(file, "{body}").unwrap();
    drop(file);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[test]
fn test_disabled_by_default() {
    let notifier = SystemNotification::new(false);
    assert!(!notifier.is_available());
}

#[test]
fn test_enabled_when_configured() {
    let notifier = SystemNotification::new(true);
    assert!(notifier.is_available());
}

#[test]
fn test_plugin_metadata() {
    let notifier = SystemNotification::new(false);
    assert_eq!(notifier.name(), "SystemNotification");
}

#[test]
fn constructor_checks_metadata_without_executing_probe() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("executed");
    let program = mock_program(dir.path(), &format!("touch '{}'", marker.display()));

    let notifier = SystemNotification::with_program(true, program);

    assert!(notifier.is_available());
    assert!(!marker.exists(), "constructor executed a synchronous probe");
}

#[test]
fn notify_requires_success_status_and_preserves_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let program = mock_program(dir.path(), "echo 'notification denied' >&2\nexit 7");
    let notifier = SystemNotification::with_program(true, program);

    let error = notifier
        .notify(
            std::path::Path::new("report.json"),
            &PluginContext::without_deadline(),
        )
        .unwrap_err();

    assert!(error.contains("exit status: 7"));
    assert!(error.contains("notification denied"));
}

#[test]
fn report_filename_is_passed_as_data_not_applescript_source() {
    let dir = tempfile::tempdir().unwrap();
    let captured = dir.path().join("argument");
    let program = mock_program(
        dir.path(),
        &format!("printf '%s' \"$3\" > '{}'", captured.display()),
    );
    let notifier = SystemNotification::with_program(true, program);
    let filename = "report\" & do shell script \"id\" & \".json";

    notifier
        .notify(
            std::path::Path::new(filename),
            &PluginContext::without_deadline(),
        )
        .unwrap();

    assert_eq!(std::fs::read_to_string(captured).unwrap(), filename);
}
