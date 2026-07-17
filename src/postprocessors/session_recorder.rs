//! Post-processor: records crash in sessions.jsonl (crash events only).
//!
//! Self-contained — absorbs `record_crash` from `session.rs`.

use crate::pipeline::{
    CrashEvent, Plugin, PluginContext, PluginExecution, PostProcessor, Priority, ReportResult,
};
use crate::utils::paths;
use chrono::Local;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;

const MAX_SESSION_RECORD_BYTES: usize = 16 * 1024;

pub struct SessionRecorder;

impl Plugin for SessionRecorder {
    fn name(&self) -> &'static str {
        "SessionRecorder"
    }
    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }
    fn priority(&self) -> Priority {
        Priority::Low
    }
}

impl PostProcessor for SessionRecorder {
    fn process(
        &self,
        event: &CrashEvent,
        result: &mut ReportResult,
        context: &PluginContext,
    ) -> Result<(), String> {
        context.checkpoint()?;
        if !event.is_crash() {
            return Ok(());
        }
        if let Some(session) = &result.session
            && let Some(json_path) = &result.json_path
        {
            let filename = json_path
                .file_name()
                .and_then(|f| f.to_str())
                .unwrap_or("unknown");
            record_crash(session, filename, context)?;
        }
        context.checkpoint()?;
        Ok(())
    }
}

// ═══════════════════════════════════════════════════
//  Session recording (absorbed from session.rs)
// ═══════════════════════════════════════════════════

/// sessions.jsonl entry.
#[derive(Serialize, Deserialize)]
struct SessionRecord {
    id: String,
    start: String,
    end: String,
    status: String,
    duration_s: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    report: Option<String>,
}

/// Record a crash in sessions.jsonl and remove session.lock.
fn record_crash(
    session: &crate::pipeline::report::SessionReport,
    report_filename: &str,
    context: &PluginContext,
) -> Result<(), String> {
    context.checkpoint()?;
    let dir = match paths::data_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[monitor] Failed to get data dir: {e}");
            return Ok(());
        }
    };

    let record = SessionRecord {
        id: session.id.clone(),
        start: session.start.clone(),
        end: Local::now().to_rfc3339(),
        status: "crash".into(),
        duration_s: session.duration_s, // precomputed, no drift
        report: Some(report_filename.to_string()),
    };

    let json = serde_json::to_string(&record)
        .map_err(|error| format!("cannot serialize session record: {error}"))?;
    let jsonl_path = dir.join("sessions.jsonl");
    if let Err(error) = append_session_record(&jsonl_path, &json, context) {
        eprintln!("[monitor] Failed to append session record: {error}");
    }

    let lock_path = dir.join("session.lock");
    let _ = fs::remove_file(lock_path);
    context.checkpoint()?;
    Ok(())
}

fn append_session_record(
    path: &std::path::Path,
    json: &str,
    context: &PluginContext,
) -> Result<(), String> {
    if json.len() > MAX_SESSION_RECORD_BYTES {
        return Err(format!(
            "session record exceeds {MAX_SESSION_RECORD_BYTES} bytes"
        ));
    }
    context.checkpoint()?;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| format!("cannot open '{}': {error}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("cannot inspect '{}': {error}", path.display()))?;
    if !metadata.file_type().is_file() {
        return Err(format!("'{}' is not a regular file", path.display()));
    }
    context.checkpoint()?;
    file.write_all(json.as_bytes())
        .and_then(|()| file.write_all(b"\n"))
        .map_err(|error| format!("cannot append '{}': {error}", path.display()))?;
    context.checkpoint()
}

#[cfg(test)]
#[path = "../../tests/unit/postprocessors/session_recorder_tests.rs"]
mod tests;
