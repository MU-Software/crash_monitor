//! Native macOS NSAlert feedback dialog.

use objc2::MainThreadOnly;
use objc2_app_kit::{
    NSAlert, NSAlertFirstButtonReturn, NSAlertStyle, NSApplication, NSApplicationActivationPolicy,
    NSTextField, NSView,
};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize, NSString};

/// Show an NSAlert with a text input field for the user to describe the issue.
///
/// Returns `Some(text)` if the user clicked Submit with non-empty text,
/// or `None` if the user clicked Skip or submitted empty text.
///
/// # Errors
/// Returns an error string if AppKit initialization fails.
pub fn show_feedback_dialog(
    report_type: &str,
    process_name: &str,
    timestamp: &str,
) -> Result<Option<String>, String> {
    // Must run on main thread for AppKit.
    let mtm =
        MainThreadMarker::new().ok_or("Not running on main thread — cannot show AppKit dialog")?;

    // Initialize NSApplication (idempotent).
    let app = NSApplication::sharedApplication(mtm);
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);

    // Create alert.
    let alert = NSAlert::new(mtm);
    alert.setAlertStyle(NSAlertStyle::Informational);

    let message = NSString::from_str(&format!("{process_name} — {report_type} Report"));
    alert.setMessageText(&message);

    let info = NSString::from_str(&format!(
        "{process_name} encountered an issue.\n\
         {timestamp}\n\n\
         Please describe what you were doing when this happened:"
    ));
    alert.setInformativeText(&info);

    // Buttons: Submit (first = default) and Skip.
    let submit_label = NSString::from_str("Submit");
    alert.addButtonWithTitle(&submit_label);
    let skip_label = NSString::from_str("Skip");
    alert.addButtonWithTitle(&skip_label);

    // Text field as accessory view (250×80).
    let frame = NSRect::new(
        NSPoint { x: 0.0, y: 0.0 },
        NSSize {
            width: 250.0,
            height: 80.0,
        },
    );
    let text_field = NSTextField::initWithFrame(NSTextField::alloc(mtm), frame);
    text_field.setStringValue(&NSString::from_str(""));
    text_field.setEditable(true);
    text_field.setSelectable(true);

    // NSTextField is an NSView subclass — upcast for setAccessoryView.
    let view: &NSView = &text_field;
    alert.setAccessoryView(Some(view));

    // Run modal.
    let response = alert.runModal();

    if response == NSAlertFirstButtonReturn {
        let value = text_field.stringValue();
        let text = value.to_string();
        if text.trim().is_empty() {
            Ok(None)
        } else {
            Ok(Some(text))
        }
    } else {
        Ok(None)
    }
}
