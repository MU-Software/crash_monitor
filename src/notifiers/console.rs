//! Notifier: outputs report path to stderr.

use crate::pipeline::{Notifier, Plugin, Priority};
use std::path::Path;

pub struct ConsoleNotifier;

impl Plugin for ConsoleNotifier {
    fn name(&self) -> &'static str {
        "ConsoleNotifier"
    }
    fn priority(&self) -> Priority {
        Priority::Low
    }
}

impl Notifier for ConsoleNotifier {
    fn notify(&self, report_path: &Path) -> Result<(), String> {
        eprintln!("[monitor] Report: {}", report_path.display());
        Ok(())
    }
}

#[cfg(test)]
#[path = "../../tests/unit/notifiers/console_tests.rs"]
mod tests;
