//! Post-processor: deletes Stage 1 raw file after Stage 2 JSON succeeds.

use crate::pipeline::{
    ArtifactKind, CrashEvent, Plugin, PluginContext, PluginExecution, PostProcessor, Priority,
    ReportResult,
};
use std::fs;
use std::path::PathBuf;

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
        if result.json_path.is_some() {
            let raw_artifacts: Vec<PathBuf> = context.artifact_transaction().map_or_else(
                || result.raw_path.clone().into_iter().collect(),
                |transaction| {
                    transaction
                        .artifacts()
                        .into_iter()
                        .filter_map(|(path, kind)| {
                            matches!(
                                kind,
                                ArtifactKind::ThreadRaw
                                    | ArtifactKind::BreadcrumbsRaw
                                    | ArtifactKind::ContextRaw
                            )
                            .then_some(path)
                        })
                        .collect()
                },
            );
            for raw in raw_artifacts {
                match fs::remove_file(&raw) {
                    Ok(()) => {
                        if let Some(transaction) = context.artifact_transaction() {
                            transaction.unregister_file(&raw)?;
                        }
                        result.artifact_paths.retain(|path| path != &raw);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        if let Some(transaction) = context.artifact_transaction() {
                            transaction.unregister_file(&raw)?;
                        }
                        result.artifact_paths.retain(|path| path != &raw);
                    }
                    Err(error) => {
                        return Err(format!(
                            "RawCleanup failed to remove sensitive raw artifact '{}': {error}",
                            raw.display()
                        ));
                    }
                }
            }
            result.raw_path = None;
        }
        context.checkpoint()?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "../../tests/unit/postprocessors/raw_cleanup_tests.rs"]
mod tests;
