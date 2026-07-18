//! Collector: Screenshots from shared memory.
//!
//! Reads RGBA pixel data from the event's owned shared-memory snapshot.
//! Encoding to PNG is handled later by the `PNGConverter` post-processor.
//! Self-contained — payload parsing uses the event's `OwnedShmSnapshot`.

use mach2::port::mach_port_t;

use crate::pipeline::{
    CollectedData, Collector, CrashEvent, Plugin, PluginContext, PluginExecution, Priority,
};

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
        let (screenshots, truncated) =
            snapshot.read_screenshots_bounded(MAX_SCREENSHOT_FRAMES, MAX_SCREENSHOT_BYTES, || {
                !context.is_timed_out()
            });
        context.checkpoint()?;
        if screenshots.is_empty() {
            eprintln!("[monitor] ScreenshotCollector: no valid screenshots in shm");
        } else {
            eprintln!(
                "[monitor] ScreenshotCollector: {} valid frames from shm",
                screenshots.len()
            );
        }
        data.raw.screenshots = screenshots;
        if truncated {
            Err(format!(
                "screenshot budget exceeded; retained at most {MAX_SCREENSHOT_FRAMES} frames and {MAX_SCREENSHOT_BYTES} bytes"
            ))
        } else {
            Ok(())
        }
    }
}
