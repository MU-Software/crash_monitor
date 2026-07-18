//! Filter: skip report generation when available disk space is too low.

use crate::pipeline::{CrashEvent, Filter, Plugin, PluginContext, PluginExecution, Priority};
use nix::sys::statvfs::statvfs;

pub struct DiskSpaceFilter {
    min_free_bytes: u64,
}

impl DiskSpaceFilter {
    #[must_use]
    pub fn new(min_free_mb: u64) -> Self {
        Self {
            min_free_bytes: min_free_mb.saturating_mul(1024 * 1024),
        }
    }
}

impl Plugin for DiskSpaceFilter {
    fn name(&self) -> &'static str {
        "DiskSpaceFilter"
    }
    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }
    fn priority(&self) -> Priority {
        Priority::Critical
    }
}

impl Filter for DiskSpaceFilter {
    fn should_process(&self, _event: &CrashEvent, context: &PluginContext) -> Result<bool, String> {
        context.checkpoint()?;
        let path = context.artifact_transaction().map_or_else(
            || crate::utils::paths::pending_dir().unwrap_or_else(|_| "/".into()),
            |transaction| transaction.report_context().output_root().to_path_buf(),
        );
        let result = Ok(space_is_sufficient(
            statvfs(&*path)
                .map(|stat| (u64::from(stat.blocks_available()), stat.fragment_size()))
                .map_err(|error| error.to_string()),
            self.min_free_bytes,
        ));
        context.checkpoint()?;
        result
    }
}

fn available_bytes(blocks_available: u64, fragment_size: u64) -> u64 {
    blocks_available.saturating_mul(fragment_size)
}

fn space_is_sufficient(
    stat: Result<(u64, u64), String>,
    minimum_free_bytes: u64,
) -> bool {
    stat.map_or(true, |(blocks, fragment_size)| {
        available_bytes(blocks, fragment_size) >= minimum_free_bytes
    })
}

#[cfg(test)]
#[path = "../../tests/unit/filters/disk_space_tests.rs"]
mod tests;
