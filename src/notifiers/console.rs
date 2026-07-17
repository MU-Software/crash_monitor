//! Notifier: outputs report path to stderr.

use crate::pipeline::{Notifier, Plugin, PluginContext, PluginExecution, Priority};
use std::path::Path;

pub struct ConsoleNotifier;

impl Plugin for ConsoleNotifier {
    fn name(&self) -> &'static str {
        "ConsoleNotifier"
    }
    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }
    fn priority(&self) -> Priority {
        Priority::Low
    }
}

impl Notifier for ConsoleNotifier {
    fn notify(&self, report_path: &Path, context: &PluginContext) -> Result<(), String> {
        context.checkpoint()?;
        eprintln!("[monitor] Report: {}", report_path.display());
        context.checkpoint()?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "../../tests/unit/notifiers/console_tests.rs"]
mod tests;
