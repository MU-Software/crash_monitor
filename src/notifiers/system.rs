//! Notifier: macOS system notification via osascript.
//!
//! Disabled by default — must be explicitly enabled via config.

use crate::pipeline::{Notifier, Plugin, Priority};
use std::path::Path;

pub struct SystemNotification {
    available: bool,
}

impl SystemNotification {
    #[must_use]
    pub fn new(enabled: bool) -> Self {
        let available = enabled
            && std::process::Command::new("osascript")
                .arg("-e")
                .arg("return")
                .output()
                .is_ok();
        Self { available }
    }
}

impl Plugin for SystemNotification {
    fn name(&self) -> &'static str {
        "SystemNotification"
    }
    fn priority(&self) -> Priority {
        Priority::Low
    }
    fn is_available(&self) -> bool {
        self.available
    }
}

impl Notifier for SystemNotification {
    fn notify(&self, report_path: &Path) -> Result<(), String> {
        let filename = report_path
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("report")
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        let script = format!(
            "display notification \"Crash report saved: {filename}\" with title \"MBB Crash Monitor\""
        );
        std::process::Command::new("osascript")
            .args(["-e", &script])
            .output()
            .map_err(|e| format!("osascript failed: {e}"))?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "../../tests/unit/notifiers/system_tests.rs"]
mod tests;
