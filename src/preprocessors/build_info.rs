//! Pre-processor: extract build metadata from crash context into a structured field.
//!
//! The build info is already serialized into the report JSON by `report_formatter`,
//! but this pre-processor makes it available as structured data on `CollectedData`
//! for other pre-processors to inspect (e.g., for fingerprint enrichment).

use crate::pipeline::{
    CollectedData, CrashEvent, Plugin, PluginContext, PluginExecution, PreProcessor, Priority,
};

/// Structured build metadata extracted from `RawCrashContext`.
#[derive(Debug, Clone)]
pub struct BuildInfo {
    pub app_version: String,
    pub build_number: u32,
    pub git_hash: String,
    pub build_type: String,
    pub build_preset: String,
    pub annotations: Vec<(String, String)>,
}

pub struct BuildInfoEnricher;

impl Plugin for BuildInfoEnricher {
    fn name(&self) -> &'static str {
        "BuildInfoEnricher"
    }
    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }
    fn priority(&self) -> Priority {
        Priority::Medium
    }
}

impl PreProcessor for BuildInfoEnricher {
    fn process(
        &self,
        _event: &CrashEvent,
        data: &mut CollectedData,
        context: &PluginContext,
    ) -> Result<(), String> {
        context.checkpoint()?;
        let Some(ctx) = &data.raw.crash_context else {
            return Ok(()); // No crash context → nothing to extract
        };

        data.build_info = Some(BuildInfo {
            app_version: ctx.app_version.clone(),
            build_number: ctx.build_number,
            git_hash: ctx.git_hash.clone(),
            build_type: ctx.build_type.clone(),
            build_preset: ctx.build_preset.clone(),
            annotations: ctx.annotations.clone(),
        });

        context.checkpoint()?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "../../tests/unit/preprocessors/build_info_tests.rs"]
mod tests;
