//! Collector: Screenshots from shared memory.
//!
//! Reads RGBA pixel data from the screenshot ring buffer in shared memory.
//! Encoding to PNG is handled later by the `PNGConverter` post-processor.
//! Self-contained — shm reading is in `SharedMemory`.

use std::sync::Arc;

use mach2::port::mach_port_t;

use crate::pipeline::{CollectedData, Collector, CrashEvent, Plugin, Priority};
use crate::shm::SharedMemory;

pub struct ScreenshotCollector {
    shm: Arc<SharedMemory>,
}

impl ScreenshotCollector {
    pub fn new(shm: Arc<SharedMemory>) -> Self {
        Self { shm }
    }
}

impl Plugin for ScreenshotCollector {
    fn name(&self) -> &'static str {
        "ScreenshotCollector"
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
    ) -> Result<(), String> {
        let screenshots = self.shm.read_screenshots();
        if screenshots.is_empty() {
            eprintln!("[monitor] ScreenshotCollector: no valid screenshots in shm");
        } else {
            eprintln!(
                "[monitor] ScreenshotCollector: {} valid frames from shm",
                screenshots.len()
            );
        }
        data.raw.screenshots = screenshots;
        Ok(())
    }
}
