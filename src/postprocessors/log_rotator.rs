//! Post-processor: rotate `sessions.jsonl` when it exceeds a size threshold.
//!
//! Keeps the most recent 50% of lines to preserve recent session history.

use crate::pipeline::{CrashEvent, Plugin, PostProcessor, Priority, ReportResult};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;

pub struct LogRotator {
    max_size_bytes: u64,
    /// Override path for sessions.jsonl. None = use default `data_dir`.
    log_path_override: Option<PathBuf>,
}

impl LogRotator {
    #[must_use]
    pub fn new(max_size_mb: u64) -> Self {
        Self {
            max_size_bytes: max_size_mb.saturating_mul(1024 * 1024),
            log_path_override: None,
        }
    }

    /// Create with explicit log path (for testing). `max_size_mb` uses the same
    /// unit as `new()` for consistency.
    #[cfg(test)]
    #[must_use]
    pub fn with_path(max_size_mb: u64, log_path: PathBuf) -> Self {
        Self {
            max_size_bytes: max_size_mb.saturating_mul(1024 * 1024),
            log_path_override: Some(log_path),
        }
    }

    fn log_path(&self) -> Result<PathBuf, String> {
        if let Some(p) = &self.log_path_override {
            return Ok(p.clone());
        }
        let dir = crate::utils::paths::data_dir().map_err(|e| format!("data_dir: {e}"))?;
        Ok(dir.join("sessions.jsonl"))
    }
}

impl Plugin for LogRotator {
    fn name(&self) -> &'static str {
        "LogRotator"
    }
    fn priority(&self) -> Priority {
        Priority::Low
    }
}

impl PostProcessor for LogRotator {
    fn process(&self, _event: &CrashEvent, _result: &mut ReportResult) -> Result<(), String> {
        let log_path = self.log_path()?;

        let Ok(metadata) = fs::metadata(&log_path) else {
            return Ok(()); // File doesn't exist — nothing to rotate
        };

        if metadata.len() <= self.max_size_bytes {
            return Ok(());
        }

        // Read all lines, keep the most recent 50%
        let file =
            fs::File::open(&log_path).map_err(|e| format!("cannot open sessions.jsonl: {e}"))?;
        let lines: Vec<String> = BufReader::new(file).lines().map_while(Result::ok).collect();

        let keep_from = lines.len() / 2;
        let kept = &lines[keep_from..];

        // Write to tmp, then rename
        let tmp_path = log_path.with_extension("jsonl.tmp");
        let mut tmp = fs::File::create(&tmp_path).map_err(|e| format!("cannot create tmp: {e}"))?;
        for line in kept {
            writeln!(tmp, "{line}").map_err(|e| format!("write failed: {e}"))?;
        }
        drop(tmp);

        fs::rename(&tmp_path, &log_path).map_err(|e| format!("rename failed: {e}"))?;

        Ok(())
    }
}

#[cfg(test)]
#[path = "../../tests/unit/postprocessors/log_rotator_tests.rs"]
mod tests;
