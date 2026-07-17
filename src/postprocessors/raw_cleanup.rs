//! Post-processor: deletes Stage 1 raw file after Stage 2 JSON succeeds.

use crate::pipeline::{
    CrashEvent, Plugin, PluginContext, PluginExecution, PostProcessor, Priority, ReportResult,
};
use std::fs;

pub struct RawCleanup;

impl Plugin for RawCleanup {
    fn name(&self) -> &'static str {
        "RawCleanup"
    }
    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }
    fn priority(&self) -> Priority {
        Priority::Low
    }
}

impl PostProcessor for RawCleanup {
    fn process(
        &self,
        _event: &CrashEvent,
        result: &mut ReportResult,
        context: &PluginContext,
    ) -> Result<(), String> {
        context.checkpoint()?;
        if result.json_path.is_some()
            && let Some(raw) = result.raw_path.clone()
        {
            match fs::remove_file(&raw) {
                Ok(()) => {
                    if let Some(transaction) = context.artifact_transaction() {
                        transaction.unregister_file(&raw)?;
                    }
                    result.artifact_paths.retain(|path| path != &raw);
                    result.raw_path = None;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    if let Some(transaction) = context.artifact_transaction() {
                        transaction.unregister_file(&raw)?;
                    }
                    result.artifact_paths.retain(|path| path != &raw);
                    result.raw_path = None;
                }
                Err(error) => {
                    eprintln!(
                        "[monitor] RawCleanup: failed to remove {}: {error}",
                        raw.display()
                    );
                }
            }
        }
        context.checkpoint()?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "../../tests/unit/postprocessors/raw_cleanup_tests.rs"]
mod tests;
