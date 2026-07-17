//! Collector: Breadcrumbs from shared memory.
//!
//! Reads per-thread ring buffers from the shared memory region,
//! extracts valid entries, and merge-sorts them by timestamp.
//! Self-contained — all shm reading logic is in `SharedMemory`.

use std::sync::Arc;

use mach2::port::mach_port_t;

use crate::pipeline::{
    CollectedData, Collector, CrashEvent, Plugin, PluginContext, PluginExecution, Priority,
};
use crate::shm::SharedMemory;

// ═══════════════════════════════════════════════════
//  Plugin + Collector implementation
// ═══════════════════════════════════════════════════

pub struct BreadcrumbCollector {
    shm: Arc<SharedMemory>,
}

impl BreadcrumbCollector {
    pub fn new(shm: Arc<SharedMemory>) -> Self {
        Self { shm }
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
        let crumbs = self.shm.read_breadcrumbs();
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
