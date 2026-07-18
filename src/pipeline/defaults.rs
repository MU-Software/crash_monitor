//! Default plugin composition root.
//!
//! Report types, formatting, and orchestration do not decide which concrete
//! plugins ship in the default monitor. This module is the sole public entry
//! point for that policy.

#[cfg(target_os = "macos")]
use std::sync::Arc;

#[cfg(target_os = "macos")]
use super::Pipeline;

#[cfg(target_os = "macos")]
/// Assemble the validated default macOS pipeline.
///
/// # Errors
/// Returns dependency or identity validation failures.
pub fn default_macos_pipeline_from_config(
    shm: Option<Arc<crate::shm::SharedMemory>>,
    validated: &crate::config::ValidatedConfig,
) -> Result<Pipeline, crate::config::ConfigValidationError> {
    super::assemble_macos_pipeline_from_config(shm, validated)
}

#[cfg(target_os = "macos")]
/// Assemble the default pipeline with the child's exact environment snapshot.
///
/// # Errors
/// Returns dependency or identity validation failures.
pub fn default_macos_pipeline_from_config_with_environment(
    shm: Option<Arc<crate::shm::SharedMemory>>,
    validated: &crate::config::ValidatedConfig,
    child_environment: Option<Arc<crate::collectors::ChildEnvironmentSnapshot>>,
) -> Result<Pipeline, crate::config::ConfigValidationError> {
    super::assemble_macos_pipeline_from_config_with_environment(shm, validated, child_environment)
}

#[cfg(target_os = "macos")]
/// Assemble the default pipeline with all monitor-owned runtime inputs.
///
/// # Errors
/// Returns dependency or identity validation failures.
pub fn default_macos_pipeline_from_config_with_runtime(
    shm: Option<Arc<crate::shm::SharedMemory>>,
    validated: &crate::config::ValidatedConfig,
    child_environment: Option<Arc<crate::collectors::ChildEnvironmentSnapshot>>,
    child_output: Option<Arc<crate::platform::ChildOutputCapture>>,
) -> Result<Pipeline, crate::config::ConfigValidationError> {
    super::assemble_macos_pipeline_from_config_with_runtime(
        shm,
        validated,
        child_environment,
        child_output,
    )
}
