//! Collector: Crash context + settings snapshot from shared memory.
//!
//! Reads generic annotations, build info, and the versioned producer extension
//! from the event's owned shared-memory snapshot.
//! Self-contained — all payload parsing uses the event's `OwnedShmSnapshot`.

use mach2::port::mach_port_t;

use crate::pipeline::{
    CollectedData, Collector, CrashEvent, Plugin, PluginContext, PluginExecution, Priority,
};
// ═══════════════════════════════════════════════════
//  Plugin + Collector implementation
// ═══════════════════════════════════════════════════

#[derive(Default)]
pub struct ContextCollector;

impl ContextCollector {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Plugin for ContextCollector {
    fn name(&self) -> &'static str {
        "ContextCollector"
    }
    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
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
        context: &PluginContext,
    ) -> Result<(), String> {
        context.checkpoint()?;
        let snapshot = context
            .shm_snapshot()
            .ok_or_else(|| "owned shared-memory snapshot unavailable".to_string())?;
        data.raw.crash_context = snapshot.read_context();
        context.checkpoint()?;
        data.raw.settings_snapshot = snapshot.read_settings();
        context.checkpoint()?;

        if data.raw.crash_context.is_some() {
            eprintln!("[monitor] ContextCollector: crash context loaded from shm");
        } else {
            eprintln!("[monitor] ContextCollector: crash context not available (shm invalid)");
        }

        Ok(())
    }
}
