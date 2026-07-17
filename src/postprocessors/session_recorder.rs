//! Post-processor: records crash in sessions.jsonl (crash events only).
//!
//! Self-contained — absorbs `record_crash` from `session.rs`.

use crate::pipeline::{CrashEvent, Plugin, PostProcessor, Priority, ReportResult};
use crate::utils::paths;
use chrono::Local;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;

pub struct SessionRecorder;

impl Plugin for SessionRecorder {
    fn name(&self) -> &'static str {
        "SessionRecorder"
    }
    fn priority(&self) -> Priority {
        Priority::Low
    }
}

impl PostProcessor for SessionRecorder {
    fn process(&self, event: &CrashEvent, result: &mut ReportResult) -> Result<(), String> {
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
            record_crash(session, filename);
        }
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
fn record_crash(session: &crate::pipeline::report::SessionReport, report_filename: &str) {
    let dir = match paths::data_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[monitor] Failed to get data dir: {e}");
            return;
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

    let jsonl_path = dir.join("sessions.jsonl");
    if let Ok(json) = serde_json::to_string(&record)
        && let Ok(mut file) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&jsonl_path)
    {
        let _ = writeln!(file, "{json}");
    }

    let lock_path = dir.join("session.lock");
    let _ = fs::remove_file(lock_path);
}

#[cfg(test)]
#[path = "../../tests/unit/postprocessors/session_recorder_tests.rs"]
mod tests;
