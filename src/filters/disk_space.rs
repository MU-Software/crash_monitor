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
        // Check pending dir first, fall back to root
        let path = crate::utils::paths::pending_dir().unwrap_or_else(|_| "/".into());
        let result = match statvfs(&*path) {
            Ok(stat) => {
                let available = u64::from(stat.blocks_available()) * stat.fragment_size();
                Ok(available >= self.min_free_bytes)
            }
            Err(_) => Ok(true), // default PASS on error
        };
        context.checkpoint()?;
        result
    }
}

#[cfg(test)]
#[path = "../../tests/unit/filters/disk_space_tests.rs"]
mod tests;
