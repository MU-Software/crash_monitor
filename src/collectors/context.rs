//! Collector: Crash context + settings snapshot from shared memory.
//!
//! Reads engine state (active tool, voxel count, undo depth, build info, etc.)
//! and settings snapshot from the shared memory region.
//! Self-contained — all shm reading logic is in `SharedMemory`.

use std::sync::Arc;

use mach2::port::mach_port_t;

use crate::pipeline::{CollectedData, Collector, CrashEvent, Plugin, Priority};
use crate::shm::SharedMemory;

// ═══════════════════════════════════════════════════
//  Plugin + Collector implementation
// ═══════════════════════════════════════════════════

pub struct ContextCollector {
    shm: Arc<SharedMemory>,
}

impl ContextCollector {
    pub fn new(shm: Arc<SharedMemory>) -> Self {
        Self { shm }
    }
}

impl Plugin for ContextCollector {
    fn name(&self) -> &'static str {
        "ContextCollector"
    }
    fn priority(&self) -> Priority {
        Priority::Critical
    }
}

impl Collector for ContextCollector {
    fn collect(
        &self,
        _event: &CrashEvent,
        _task: mach_port_t,
        data: &mut CollectedData,
    ) -> Result<(), String> {
        data.raw.crash_context = self.shm.read_context();
        data.raw.settings_snapshot = self.shm.read_settings();

        if data.raw.crash_context.is_some() {
            eprintln!("[monitor] ContextCollector: crash context loaded from shm");
        } else {
            eprintln!("[monitor] ContextCollector: crash context not available (shm invalid)");
        }

        Ok(())
    }
}
