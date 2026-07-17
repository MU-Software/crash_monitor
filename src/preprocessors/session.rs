//! Pre-processor: reads session.lock and populates `data.session`.
//!
//! Self-contained — absorbs `read_session_lock` from `session.rs`.

use crate::pipeline::report::SessionReport;
use crate::pipeline::{
    CollectedData, CrashEvent, Plugin, PluginContext, PluginExecution, PreProcessor, Priority,
};
use crate::utils::paths;
use chrono::Local;
use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

const MAX_SESSION_LOCK_BYTES: usize = 4 * 1024;
const MAX_SESSION_LOCK_LINE_BYTES: usize = 1024;
const SESSION_READ_CHUNK_BYTES: usize = 512;

pub struct SessionEnricher;

impl Plugin for SessionEnricher {
    fn name(&self) -> &'static str {
        "SessionEnricher"
    }
    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }
    fn priority(&self) -> Priority {
        Priority::Low
    }
}

impl PreProcessor for SessionEnricher {
    fn process(
        &self,
        _event: &CrashEvent,
        data: &mut CollectedData,
        context: &PluginContext,
    ) -> Result<(), String> {
        context.checkpoint()?;
        data.session = read_session_lock(context);
        context.checkpoint()?;
        Ok(())
    }
}

// ═══════════════════════════════════════════════════
//  Session lock reading (absorbed from session.rs)
// ═══════════════════════════════════════════════════

/// Read session info from session.lock file.
/// Duration is computed once here to avoid time drift between callers.
fn read_session_lock(context: &PluginContext) -> Option<SessionReport> {
    let dir = paths::data_dir().ok()?;
    read_session_lock_from(&dir, context)
}

/// Read session info from a specific data directory.
/// Pure function — pass an explicit path to avoid global env mutation in tests.
fn read_session_lock_from(dir: &Path, context: &PluginContext) -> Option<SessionReport> {
    let lock_path = dir.join("session.lock");

    // O_NONBLOCK prevents a raced-in FIFO/device from blocking the cooperative
    // pipeline thread, while O_NOFOLLOW rejects symlink substitution.
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NONBLOCK | nix::libc::O_NOFOLLOW)
        .open(&lock_path)
        .ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.file_type().is_file()
        || metadata.len() > u64::try_from(MAX_SESSION_LOCK_BYTES).ok()?
    {
        return None;
    }

    // Do not trust the pre-read size: the file may grow after metadata(). Read
    // at most cap + 1 in small chunks and reject the extra byte.
    let mut bytes = Vec::with_capacity(
        usize::try_from(metadata.len())
            .ok()?
            .min(MAX_SESSION_LOCK_BYTES),
    );
    let mut chunk = [0_u8; SESSION_READ_CHUNK_BYTES];
    loop {
        context.checkpoint().ok()?;
        let remaining = (MAX_SESSION_LOCK_BYTES + 1).saturating_sub(bytes.len());
        if remaining == 0 {
            return None;
        }
        let slice_len = remaining.min(chunk.len());
        match file.read(&mut chunk[..slice_len]) {
            Ok(0) => break,
            Ok(read) => {
                bytes.extend_from_slice(&chunk[..read]);
                if bytes.len() > MAX_SESSION_LOCK_BYTES {
                    return None;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return None,
        }
    }
    if bytes
        .split(|byte| *byte == b'\n')
        .any(|line| line.len() > MAX_SESSION_LOCK_LINE_BYTES)
    {
        return None;
    }

    let text = std::str::from_utf8(&bytes).ok()?;
    let mut lines = text.lines();

    let id = lines.next()?.to_string();
    let start = lines.next()?.to_string();

    let start_timestamp = chrono::DateTime::parse_from_rfc3339(&start)
        .map(|dt| dt.timestamp())
        .or_else(|_| start.trim().parse::<i64>())
        .unwrap_or(0);

    #[allow(clippy::cast_sign_loss)] // .max(0) guarantees non-negative
    let duration_s = if start_timestamp > 0 {
        (Local::now().timestamp() - start_timestamp).max(0) as u64
    } else {
        0
    };

    Some(SessionReport {
        id,
        start,
        duration_s,
    })
}

#[cfg(test)]
#[path = "../../tests/unit/preprocessors/session_tests.rs"]
mod tests;
