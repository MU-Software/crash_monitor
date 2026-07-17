//! Post-processor: enforce retention policy on archived reports.
//!
//! Operates on the `sent/` directory (the archive), pruning oldest reports
//! when any threshold is exceeded:
//! - count > `max_count`
//! - total size > `max_total_bytes`
//! - age > `max_age_days`

use crate::pipeline::{
    CrashEvent, Plugin, PluginContext, PluginExecution, PostProcessor, Priority, ReportResult,
};
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

const MAX_RETENTION_SCAN_ENTRIES: usize = 10_000;

pub struct RetentionManager {
    max_count: usize,
    max_total_bytes: u64,
    max_age: Duration,
    /// Override target directory path (for testing).
    dir_override: Option<PathBuf>,
}

impl RetentionManager {
    #[must_use]
    pub fn new(max_count: usize, max_size_mb: u64, max_age_days: u64) -> Self {
        Self {
            max_count,
            max_total_bytes: max_size_mb.saturating_mul(1024 * 1024),
            max_age: Duration::from_secs(max_age_days.saturating_mul(86400)),
            dir_override: None,
        }
    }

    /// Create with explicit directory (for testing). Same units as `new()`.
    #[cfg(test)]
    #[must_use]
    pub fn with_dir(max_count: usize, max_size_mb: u64, max_age_days: u64, dir: PathBuf) -> Self {
        Self {
            max_count,
            max_total_bytes: max_size_mb.saturating_mul(1024 * 1024),
            max_age: Duration::from_secs(max_age_days.saturating_mul(86400)),
            dir_override: Some(dir),
        }
    }

    fn target_dir(&self) -> Result<PathBuf, String> {
        match &self.dir_override {
            Some(p) => Ok(p.clone()),
            None => crate::utils::paths::sent_dir().map_err(|e| format!("sent_dir: {e}")),
        }
    }
}

impl Plugin for RetentionManager {
    fn name(&self) -> &'static str {
        "RetentionManager"
    }
    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }
    fn priority(&self) -> Priority {
        Priority::Low
    }
}

struct ReportEntry {
    path: PathBuf,
    size: u64,
    modified: SystemTime,
}

impl PostProcessor for RetentionManager {
    fn process(
        &self,
        _event: &CrashEvent,
        _result: &mut ReportResult,
        context: &PluginContext,
    ) -> Result<(), String> {
        context.checkpoint()?;
        let dir = self.target_dir()?;

        let mut entries = collect_entries(&dir, context)?;
        if entries.is_empty() {
            return Ok(());
        }

        // Sort oldest first (by modification time)
        entries.sort_by_key(|e| e.modified);

        let now = SystemTime::now();

        // Pass 1: delete files older than max_age
        let mut retained = Vec::with_capacity(entries.len());
        for entry in entries {
            context.checkpoint()?;
            let age = now.duration_since(entry.modified).unwrap_or(Duration::ZERO);
            if age > self.max_age {
                let _ = fs::remove_file(&entry.path);
            } else {
                retained.push(entry);
            }
        }
        entries = retained;

        // Pass 2: delete oldest while count exceeds limit
        if entries.len() > self.max_count {
            for e in entries.drain(..entries.len() - self.max_count) {
                context.checkpoint()?;
                let _ = fs::remove_file(&e.path);
            }
        }

        // Pass 3: delete oldest while total size exceeds limit
        let mut total = 0_u64;
        for entry in &entries {
            context.checkpoint()?;
            total = total.saturating_add(entry.size);
        }
        let mut remove_count = 0;
        while total > self.max_total_bytes && remove_count < entries.len() {
            context.checkpoint()?;
            total -= entries[remove_count].size;
            remove_count += 1;
        }
        if remove_count > 0 {
            for e in entries.drain(..remove_count) {
                context.checkpoint()?;
                let _ = fs::remove_file(&e.path);
            }
        }

        context.checkpoint()?;
        Ok(())
    }
}

fn collect_entries(
    dir: &std::path::Path,
    context: &PluginContext,
) -> Result<Vec<ReportEntry>, String> {
    collect_entries_bounded(dir, context, MAX_RETENTION_SCAN_ENTRIES)
}

fn collect_entries_bounded(
    dir: &std::path::Path,
    context: &PluginContext,
    max_entries: usize,
) -> Result<Vec<ReportEntry>, String> {
    context.checkpoint()?;
    let read_dir =
        fs::read_dir(dir).map_err(|e| format!("cannot read '{}': {e}", dir.display()))?;

    let mut entries = Vec::new();
    for entry in read_dir {
        context.checkpoint()?;
        let Ok(entry) = entry else { continue };
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        if entries.len() >= max_entries {
            eprintln!("[monitor] RetentionManager: scan truncated at {max_entries} regular files");
            break;
        }
        let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
        entries.push(ReportEntry {
            path: entry.path(),
            size: metadata.len(),
            modified,
        });
    }
    Ok(entries)
}

#[cfg(test)]
#[path = "../../tests/unit/postprocessors/retention_tests.rs"]
mod tests;
