//! Plugin trait definitions for each pipeline stage.

use mach2::port::mach_port_t;
use std::path::Path;

use super::types::{CollectedData, CrashEvent, Priority, ReportResult};

/// Base trait for all plugins.
pub trait Plugin: Send + Sync {
    fn name(&self) -> &'static str;
    #[allow(dead_code)] // Phase 4+: used for plugin ordering
    fn priority(&self) -> Priority;
    /// Dependencies within the SAME category. Cross-category dependencies are forbidden.
    fn depends_on(&self) -> &'static [&'static str] {
        &[]
    }
    /// Runtime platform availability check.
    fn is_available(&self) -> bool {
        true
    }
    /// Per-plugin timeout override (seconds).
    /// `u32::MAX` = use category default, `0` = no timeout.
    fn timeout_secs(&self) -> u32 {
        u32::MAX
    }
}

/// Collector — gathers data while the child is suspended.
pub trait Collector: Plugin {
    /// Collect introspection data from the target process.
    ///
    /// # Errors
    /// Returns an error if data collection fails (e.g., Mach API error).
    fn collect(
        &self,
        event: &CrashEvent,
        task: mach_port_t,
        data: &mut CollectedData,
    ) -> Result<(), String>;
}

/// Pre-processor — enriches collected data after resume.
pub trait PreProcessor: Plugin {
    /// Enrich or transform collected data after the target is resumed.
    ///
    /// # Errors
    /// Returns an error if processing fails.
    fn process(&self, event: &CrashEvent, data: &mut CollectedData) -> Result<(), String>;
}

/// Post-processor — operates on the written report file.
pub trait PostProcessor: Plugin {
    /// Process the completed report (e.g., move files, notify).
    /// `result` is mutable so a plugin can update `json_path`/`raw_path`
    /// when it relocates files (used by `MoveToSent`).
    ///
    /// # Errors
    /// Returns an error if post-processing fails.
    fn process(&self, event: &CrashEvent, result: &mut ReportResult) -> Result<(), String>;
}

/// Filter — decides whether to process an event.
pub trait Filter: Plugin {
    /// Decide whether the pipeline should process this event.
    ///
    /// # Errors
    /// Returns an error if the filtering check itself fails.
    fn should_process(&self, event: &CrashEvent) -> Result<bool, String>;
}

/// Notifier — fire-and-forget notification after report generation.
#[allow(dead_code)] // Phase 6+
pub trait Notifier: Plugin {
    /// Send a notification about the generated report.
    ///
    /// # Errors
    /// Returns an error if notification delivery fails.
    fn notify(&self, report_path: &Path) -> Result<(), String>;
}
