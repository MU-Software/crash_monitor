//! Notifier: macOS system notification via osascript.
//!
//! Disabled by default — must be explicitly enabled via config.

use crate::pipeline::{
    Notifier, Plugin, PluginContext, PluginExecution, PluginRunResult, Priority,
    run_plugin_subprocess,
};
use crate::utils::terminal::escape_terminal;
use std::path::Path;
use std::process::Command;

const OSASCRIPT: &str = "/usr/bin/osascript";

pub struct SystemNotification {
    available: bool,
}

impl SystemNotification {
    #[must_use]
    pub fn new(enabled: bool) -> Self {
        // Availability checks must not execute a potentially hanging helper.
        let available = enabled && Path::new(OSASCRIPT).is_file();
        Self { available }
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
            .unwrap_or("report")
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        let script = format!(
            "display notification \"Crash report saved: {filename}\" with title \"Crash Monitor\""
        );
        let mut command = Command::new(OSASCRIPT);
        command.args(["-e", &script]);
        match run_plugin_subprocess(self.name(), &mut command, context) {
            PluginRunResult::Completed(output) if output.status.success() => Ok(()),
            PluginRunResult::Completed(output) => Err(format!(
                "osascript exited with {}: {}",
                output.status,
                escape_terminal(String::from_utf8_lossy(&output.stderr).trim())
            )),
            PluginRunResult::TimedOut => Err("osascript timed out".to_string()),
            PluginRunResult::Failed(error) => Err(error),
            PluginRunResult::Panicked(message) => {
                Err(format!("osascript supervisor panicked: {message}"))
            }
        }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/notifiers/system_tests.rs"]
mod tests;
