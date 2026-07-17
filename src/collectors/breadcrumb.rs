//! Collector: Breadcrumbs from shared memory.
//!
//! Reads per-thread ring buffers from the event's owned shared-memory snapshot,
//! extracts valid entries, and merge-sorts them by timestamp.
//! Self-contained — all payload parsing uses the event's `OwnedShmSnapshot`.

use mach2::port::mach_port_t;

use crate::pipeline::{
    CollectedData, Collector, CrashEvent, Plugin, PluginContext, PluginExecution, Priority,
};
// ═══════════════════════════════════════════════════
//  Plugin + Collector implementation
// ═══════════════════════════════════════════════════

#[derive(Default)]
pub struct BreadcrumbCollector;

impl BreadcrumbCollector {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Plugin for BreadcrumbCollector {
    fn name(&self) -> &'static str {
        "BreadcrumbCollector"
    }
    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }
    fn priority(&self) -> Priority {
        Priority::Critical
    }
}

impl Collector for BreadcrumbCollector {
    fn collect(
        &self,
        _event: &CrashEvent,
        _task: mach_port_t,
        data: &mut CollectedData,
        context: &PluginContext,
    ) -> Result<(), String> {
        context.checkpoint()?;
        let snapshot = context
            .shm_snapshot()
            .ok_or_else(|| "owned shared-memory snapshot unavailable".to_string())?;
        let crumbs = snapshot.read_breadcrumbs();
        context.checkpoint()?;
        if crumbs.is_empty() {
            eprintln!("[monitor] BreadcrumbCollector: no breadcrumbs found (shm may be empty)");
        } else {
            eprintln!(
                "[monitor] BreadcrumbCollector: {} breadcrumbs from shm",
                crumbs.len()
            );
        }
        data.raw.breadcrumbs = crumbs;
        Ok(())
    }
}
