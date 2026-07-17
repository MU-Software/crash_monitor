//! Pre-processor: reads session.lock and populates `data.session`.
//!
//! Self-contained — absorbs `read_session_lock` from `session.rs`.

use crate::pipeline::report::SessionReport;
use crate::pipeline::{CollectedData, CrashEvent, Plugin, PreProcessor, Priority};
use crate::utils::paths;
use chrono::Local;
use std::fs;
use std::path::Path;

pub struct SessionEnricher;

impl Plugin for SessionEnricher {
    fn name(&self) -> &'static str {
        "SessionEnricher"
    }
    fn priority(&self) -> Priority {
        Priority::Low
    }
}

impl PreProcessor for SessionEnricher {
    fn process(&self, _event: &CrashEvent, data: &mut CollectedData) -> Result<(), String> {
        data.session = read_session_lock();
        Ok(())
    }
}

// ═══════════════════════════════════════════════════
//  Session lock reading (absorbed from session.rs)
// ═══════════════════════════════════════════════════

/// Read session info from session.lock file.
/// Duration is computed once here to avoid time drift between callers.
fn read_session_lock() -> Option<SessionReport> {
    let dir = paths::data_dir().ok()?;
    read_session_lock_from(&dir)
}

/// Read session info from a specific data directory.
/// Pure function — pass an explicit path to avoid global env mutation in tests.
fn read_session_lock_from(dir: &Path) -> Option<SessionReport> {
    let lock_path = dir.join("session.lock");

    let content = fs::read_to_string(&lock_path).ok()?;
    let mut lines = content.lines();

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
