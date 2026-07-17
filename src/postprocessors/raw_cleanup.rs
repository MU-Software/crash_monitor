//! Post-processor: deletes Stage 1 raw file after Stage 2 JSON succeeds.

use crate::pipeline::{CrashEvent, Plugin, PostProcessor, Priority, ReportResult};
use std::fs;

pub struct RawCleanup;

impl Plugin for RawCleanup {
    fn name(&self) -> &'static str {
        "RawCleanup"
    }
    fn priority(&self) -> Priority {
        Priority::Low
    }
}

impl PostProcessor for RawCleanup {
    fn process(&self, _event: &CrashEvent, result: &mut ReportResult) -> Result<(), String> {
        if let (Some(raw), Some(_json)) = (&result.raw_path, &result.json_path) {
            let _ = fs::remove_file(raw);
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "../../tests/unit/postprocessors/raw_cleanup_tests.rs"]
mod tests;
