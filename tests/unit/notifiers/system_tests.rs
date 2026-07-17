use crate::notifiers::SystemNotification;
use crate::pipeline::Plugin;

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
