//! Collector: Screenshots from shared memory.
//!
//! Reads RGBA pixel data from the event's owned shared-memory snapshot.
//! Encoding to PNG is handled later by the `PNGConverter` post-processor.
//! Self-contained — payload parsing uses the event's `OwnedShmSnapshot`.

use mach2::port::mach_port_t;

use crate::pipeline::{
    CollectedData, Collector, CrashEvent, Plugin, PluginContext, PluginExecution, Priority,
};
use crate::shm::ScreenshotBudgetExhaustion;

const MAX_SCREENSHOT_FRAMES: usize = 8;
const MAX_SCREENSHOT_BYTES: usize = 4 * 1024 * 1024;
#[derive(Default)]
pub struct ScreenshotCollector;

impl ScreenshotCollector {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Plugin for ScreenshotCollector {
    fn name(&self) -> &'static str {
        "ScreenshotCollector"
    }
    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }
    fn priority(&self) -> Priority {
        Priority::Medium
    }
}

impl Collector for ScreenshotCollector {
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
        let outcome =
            snapshot.read_screenshots_bounded(MAX_SCREENSHOT_FRAMES, MAX_SCREENSHOT_BYTES, || {
                !context.is_timed_out()
            });
        context.checkpoint()?;
        if outcome.screenshots.is_empty() {
            eprintln!("[monitor] ScreenshotCollector: no valid screenshots in shm");
        } else {
            eprintln!(
                "[monitor] ScreenshotCollector: {} valid frames from shm",
                outcome.screenshots.len()
            );
        }
        let mut diagnostics = Vec::new();
        if let Some(exhaustion) = outcome.budget_exhaustion {
            diagnostics.push(match exhaustion {
                ScreenshotBudgetExhaustion::FrameLimit => format!(
                    "screenshot frame budget exceeded; retained at most {MAX_SCREENSHOT_FRAMES} frames"
                ),
                ScreenshotBudgetExhaustion::ByteLimit => format!(
                    "screenshot byte budget exceeded; retained at most {MAX_SCREENSHOT_BYTES} bytes"
                ),
                ScreenshotBudgetExhaustion::Deadline => {
                    "screenshot collection deadline exceeded".to_string()
                }
            });
        }
        if !outcome.unreadable_slots.is_empty() {
            diagnostics.push(format!(
                "published screenshot slots could not be read completely: {:?}",
                outcome.unreadable_slots
            ));
        }
        data.raw.screenshots = outcome.screenshots;
        if diagnostics.is_empty() {
            Ok(())
        } else {
            Err(diagnostics.join("; "))
        }
    }
}
