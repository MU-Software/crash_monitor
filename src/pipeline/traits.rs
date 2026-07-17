//! Plugin trait definitions for each pipeline stage.

use mach2::port::mach_port_t;
use std::path::Path;

use super::safety::PluginContext;
use super::types::{CollectedData, CrashEvent, Priority, ReportResult};

/// Required execution boundary for a registered plugin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginExecution {
    /// Audited implementation whose bounded work observes
    /// [`PluginContext`] checkpoints.
    Cooperative,
    /// A trusted, in-process adapter whose potentially blocking or untrusted
    /// payload must be launched through the killable subprocess supervisor.
    /// The pipeline verifies that the adapter crosses that boundary (or
    /// explicitly records that the invocation is a no-op) before accepting a
    /// completed result.
    Subprocess,
}

/// Base trait for all plugins.
///
/// Metadata methods are evaluated outside the execution runner. They must be
/// constant-time accessors over cached state and must not perform blocking I/O.
pub trait Plugin: Send + Sync {
    fn name(&self) -> &'static str;
    fn execution(&self) -> PluginExecution;
    #[allow(dead_code)] // Phase 4+: used for plugin ordering
    fn priority(&self) -> Priority;
    /// Required data dependencies within the same category.
    ///
    /// A failed hard dependency skips this plugin at runtime. Configuration
    /// validation disables a dependent whose hard dependency was explicitly
    /// disabled rather than silently re-enabling the dependency.
    fn hard_dependencies(&self) -> &'static [&'static str] {
        &[]
    }
    /// Ordering constraints within the same category.
    ///
    /// The referenced ID must exist in the complete static registry. It may be
    /// absent from an enabled runtime subset; when both plugins are registered,
    /// the dependency must appear first. Its runtime failure does not skip this
    /// plugin.
    fn order_after(&self) -> &'static [&'static str] {
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
        context: &PluginContext,
    ) -> Result<(), String>;
}

/// Pre-processor — enriches collected data after resume.
pub trait PreProcessor: Plugin {
    /// Enrich or transform collected data after the target is resumed.
    ///
    /// # Errors
    /// Returns an error if processing fails.
    fn process(
        &self,
        event: &CrashEvent,
        data: &mut CollectedData,
        context: &PluginContext,
    ) -> Result<(), String>;
}

/// Post-processor — operates on the written report file.
pub trait PostProcessor: Plugin {
    /// Process the completed report (e.g., move files, notify).
    /// `result` is mutable so a plugin can update `json_path`/`raw_path`
    /// when it relocates files (used by `MoveToSent`).
    ///
    /// # Errors
    /// Returns an error if post-processing fails.
    fn process(
        &self,
        event: &CrashEvent,
        result: &mut ReportResult,
        context: &PluginContext,
    ) -> Result<(), String>;
}

/// Filter — decides whether to process an event.
pub trait Filter: Plugin {
    /// Decide whether the pipeline should process this event.
    ///
    /// # Errors
    /// Returns an error if the filtering check itself fails.
    fn should_process(&self, event: &CrashEvent, context: &PluginContext) -> Result<bool, String>;
}

/// Notifier — fire-and-forget notification after report generation.
#[allow(dead_code)] // Phase 6+
pub trait Notifier: Plugin {
    /// Send a notification about the generated report.
    ///
    /// # Errors
    /// Returns an error if notification delivery fails.
    fn notify(&self, report_path: &Path, context: &PluginContext) -> Result<(), String>;
}
