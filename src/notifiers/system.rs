//! Notifier: macOS system notification via osascript.
//!
//! Disabled by default — must be explicitly enabled via config.

use crate::pipeline::{
    Notifier, Plugin, PluginContext, PluginExecution, PluginRunResult, Priority,
    run_plugin_subprocess,
};
use crate::utils::terminal::escape_terminal;
use std::path::{Path, PathBuf};
use std::process::Command;

const OSASCRIPT: &str = "/usr/bin/osascript";
const NOTIFICATION_SCRIPT: &str = "on run argv\nset reportName to item 1 of argv\ndisplay notification (\"Crash report saved: \" & reportName) with title \"Crash Monitor\"\nend run";

pub struct SystemNotification {
    available: bool,
    osascript: PathBuf,
}

impl SystemNotification {
    #[must_use]
    pub fn new(enabled: bool) -> Self {
        // Availability checks must not execute a potentially hanging helper.
        let available = enabled && Path::new(OSASCRIPT).is_file();
        Self {
            available,
            osascript: PathBuf::from(OSASCRIPT),
        }
    }

    #[cfg(test)]
    fn with_program(enabled: bool, osascript: PathBuf) -> Self {
        let available = enabled && osascript.is_file();
        Self {
            available,
            osascript,
        }
    }
}

impl Plugin for SystemNotification {
    fn name(&self) -> &'static str {
        "SystemNotification"
    }
    fn execution(&self) -> PluginExecution {
        PluginExecution::Subprocess
    }
    fn priority(&self) -> Priority {
        Priority::Low
    }
    fn is_available(&self) -> bool {
        self.available
    }
}

impl Notifier for SystemNotification {
    fn notify(&self, report_path: &Path, context: &PluginContext) -> Result<(), String> {
        let filename = report_path
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("report");
        // Keep report-controlled text out of AppleScript source. osascript
        // passes arguments after the static handler as `argv`.
        let mut command = Command::new(&self.osascript);
        command.args(["-e", NOTIFICATION_SCRIPT, filename]);
        match run_plugin_subprocess(self.name(), &mut command, context) {
            PluginRunResult::Completed(output) if output.status.success() => Ok(()),
            PluginRunResult::Completed(output) => {
                let stderr = escape_terminal(String::from_utf8_lossy(&output.stderr).trim());
                let truncation = if output.stderr_truncated {
                    " (stderr truncated)"
                } else {
                    ""
                };
                Err(format!(
                    "osascript exited with {}: {stderr}{truncation}",
                    output.status
                ))
            }
            PluginRunResult::TimedOut => Err("osascript timed out".to_string()),
            PluginRunResult::Failed(error) => Err(error.to_string()),
            PluginRunResult::Panicked(message) => {
                Err(format!("osascript supervisor panicked: {message}"))
            }
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/notifiers/system_tests.rs"]
mod tests;
