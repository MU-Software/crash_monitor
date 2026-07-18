//! Post-processor: rotate `sessions.jsonl` when it exceeds a size threshold.
//!
//! Keeps the most recent 50% of lines to preserve recent session history.

use crate::pipeline::{
    CrashEvent, Plugin, PluginContext, PluginExecution, PostProcessor, PostProcessorPhase,
    Priority, ReportResult,
};
use crate::utils::paths::{
    create_private_file, ensure_private_directory, open_private_directory, open_private_file,
};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, Write};
use std::path::{Path, PathBuf};

// Rotation may read at most this much beyond the configured trigger. This keeps
// the operation bounded without making thresholds at or above 8 MiB impossible
// to satisfy.
const MAX_LOG_ROTATION_OVERAGE_BYTES: usize = 8 * 1024 * 1024;
const MAX_LOG_LINE_BYTES: usize = 64 * 1024;
const MAX_LOG_LINES: usize = 100_000;
const LOG_IO_CHUNK_BYTES: usize = 16 * 1024;

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

    fn log_path(&self, context: &PluginContext) -> Result<PathBuf, String> {
        if let Some(p) = &self.log_path_override {
            return Ok(p.clone());
        }
        let dir = context
            .artifact_transaction()
            .map(|transaction| transaction.report_context().output_root().to_path_buf())
            .map_or_else(
                || crate::utils::paths::data_dir().map_err(|e| format!("data_dir: {e}")),
                Ok,
            )?;
        Ok(dir.join("sessions.jsonl"))
    }
}

impl Plugin for LogRotator {
    fn name(&self) -> &'static str {
        "LogRotator"
    }
    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }
    fn priority(&self) -> Priority {
        Priority::Low
    }
    fn order_after(&self) -> &'static [&'static str] {
        &["SessionRecorder"]
    }
}

impl PostProcessor for LogRotator {
    fn phase(&self) -> PostProcessorPhase {
        PostProcessorPhase::AfterCommit
    }

    fn process(
        &self,
        _event: &CrashEvent,
        _result: &mut ReportResult,
        context: &PluginContext,
    ) -> Result<(), String> {
        context.checkpoint()?;
        let log_path = self.log_path(context)?;
        let log_dir = log_path
            .parent()
            .ok_or_else(|| format!("sessions.jsonl has no parent: '{}'", log_path.display()))?;
        ensure_private_directory(log_dir)
            .map_err(|error| format!("cannot prepare private session directory: {error}"))?;
        let _log_lock = super::session_recorder::acquire_session_log_lock(log_dir, context)?;

        let file = match fs::symlink_metadata(&log_path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(format!("cannot inspect sessions.jsonl: {error}")),
            Ok(_) => open_private_file(&log_path)
                .map_err(|error| format!("cannot safely open sessions.jsonl: {error}"))?,
        };
        let metadata = file
            .metadata()
            .map_err(|error| format!("cannot stat sessions.jsonl: {error}"))?;
        if !metadata.file_type().is_file() {
            return Err("sessions.jsonl is not a regular file".to_string());
        }

        if metadata.len() <= self.max_size_bytes {
            return Ok(());
        }
        let max_rotation_overage = u64::try_from(MAX_LOG_ROTATION_OVERAGE_BYTES)
            .map_err(|_| "log rotation byte limit does not fit u64".to_string())?;
        let max_rotation_bytes = self.max_size_bytes.saturating_add(max_rotation_overage);
        if metadata.len() > max_rotation_bytes {
            return Err(format!(
                "sessions.jsonl exceeds configured threshold plus rotation overage ({max_rotation_bytes} bytes)"
            ));
        }
        let mut reader = BufReader::with_capacity(LOG_IO_CHUNK_BYTES, file);
        let valid_lines = scan_valid_lines(&mut reader, context)?;
        if valid_lines == 0 {
            return Err("sessions.jsonl contains no recoverable records".to_string());
        }
        reader
            .rewind()
            .map_err(|error| format!("cannot rewind sessions.jsonl: {error}"))?;
        replace_log_atomically(&log_path, &mut reader, valid_lines / 2, context)
    }
}

enum BoundedLine {
    Valid(Vec<u8>),
    Corrupt,
}

fn scan_valid_lines(
    reader: &mut BufReader<File>,
    context: &PluginContext,
) -> Result<usize, String> {
    let mut valid = 0_usize;
    let mut observed = 0_usize;
    loop {
        context.checkpoint()?;
        let Some(line) = read_bounded_line(reader, context)? else {
            return Ok(valid);
        };
        if observed == MAX_LOG_LINES {
            return Err(format!(
                "sessions.jsonl exceeds line-count limit ({MAX_LOG_LINES})"
            ));
        }
        observed += 1;
        if matches!(line, BoundedLine::Valid(_)) {
            valid += 1;
        }
    }
}

fn read_bounded_line(
    reader: &mut BufReader<File>,
    context: &PluginContext,
) -> Result<Option<BoundedLine>, String> {
    let mut bytes = Vec::new();
    let mut corrupt = false;
    let mut saw_bytes = false;
    loop {
        context.checkpoint()?;
        let available = reader
            .fill_buf()
            .map_err(|error| format!("cannot read sessions.jsonl: {error}"))?;
        if available.is_empty() {
            return if saw_bytes {
                Ok(Some(if corrupt {
                    BoundedLine::Corrupt
                } else {
                    BoundedLine::Valid(bytes)
                }))
            } else {
                Ok(None)
            };
        }
        let take = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |position| position + 1);
        let terminated = available[take - 1] == b'\n';
        saw_bytes = true;
        if !corrupt {
            if bytes.len().saturating_add(take) > MAX_LOG_LINE_BYTES {
                bytes.clear();
                corrupt = true;
            } else {
                bytes.extend_from_slice(&available[..take]);
            }
        }
        reader.consume(take);
        if terminated {
            return Ok(Some(if corrupt {
                BoundedLine::Corrupt
            } else {
                BoundedLine::Valid(bytes)
            }));
        }
    }
}

fn replace_log_atomically(
    log_path: &Path,
    reader: &mut BufReader<File>,
    keep_from: usize,
    context: &PluginContext,
) -> Result<(), String> {
    let file_name = log_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("log path has no valid filename: '{}'", log_path.display()))?;
    let tmp_path = log_path.with_file_name(format!(
        ".{file_name}.log-rotate-{}.tmp",
        uuid::Uuid::new_v4()
    ));
    context.checkpoint()?;
    let mut tmp = create_private_file(&tmp_path)
        .map_err(|error| format!("cannot create rotation tmp: {error}"))?;
    let write_result = (|| {
        let mut valid_index = 0_usize;
        while let Some(line) = read_bounded_line(reader, context)? {
            if let BoundedLine::Valid(bytes) = line {
                if valid_index >= keep_from {
                    for chunk in bytes.chunks(LOG_IO_CHUNK_BYTES) {
                        context.checkpoint()?;
                        tmp.write_all(chunk)
                            .map_err(|error| format!("rotation write failed: {error}"))?;
                    }
                }
                valid_index += 1;
            }
        }
        tmp.flush()
            .map_err(|error| format!("rotation flush failed: {error}"))?;
        tmp.sync_all()
            .map_err(|error| format!("rotation sync failed: {error}"))?;
        context.checkpoint()?;
        Ok::<(), String>(())
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_file(&tmp_path);
        return Err(error);
    }

    if let Err(error) = fs::rename(&tmp_path, log_path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(format!("rotation rename failed: {error}"));
    }
    let parent = log_path
        .parent()
        .ok_or_else(|| format!("sessions.jsonl has no parent: '{}'", log_path.display()))?;
    open_private_directory(parent)?
        .sync_all()
        .map_err(|error| format!("session directory sync failed: {error}"))?;
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/unit/postprocessors/log_rotator_tests.rs"]
mod tests;
