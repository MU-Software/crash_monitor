use crate::notifiers::ConsoleNotifier;
use crate::pipeline::{Notifier, Plugin};
use std::path::Path;

#[test]
fn test_console_notifier_runs() {
    let notifier = ConsoleNotifier;
    let path = Path::new("/tmp/crash_test_report.json");
    assert!(notifier.notify(path).is_ok());
}

#[test]
fn test_plugin_metadata() {
    let notifier = ConsoleNotifier;
    assert_eq!(notifier.name(), "ConsoleNotifier");
    assert!(notifier.is_available());
}
