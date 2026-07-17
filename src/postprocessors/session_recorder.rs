//! Post-processor: records crash in sessions.jsonl (crash events only).
//!
//! Self-contained — absorbs `record_crash` from `session.rs`.

use crate::pipeline::{
    CommittedReport, CrashEvent, Plugin, PluginContext, PluginExecution, PostProcessor,
    PostProcessorPhase, Priority, ReportResult,
};
use crate::utils::paths;
use chrono::Local;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;

#[cfg(test)]
thread_local! {
    static TEST_DATA_DIR_OVERRIDE: std::cell::RefCell<Option<std::path::PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

const MAX_SESSION_RECORD_BYTES: usize = 16 * 1024;

pub struct SessionRecorder;

#[cfg(test)]
fn with_test_data_dir<T>(path: &std::path::Path, action: impl FnOnce() -> T) -> T {
    struct Reset(Option<std::path::PathBuf>);

    impl Drop for Reset {
        fn drop(&mut self) {
            TEST_DATA_DIR_OVERRIDE.with(|override_path| {
                override_path.replace(self.0.take());
            });
        }
    }

    let previous = TEST_DATA_DIR_OVERRIDE
        .with(|override_path| override_path.replace(Some(path.to_path_buf())));
    let _reset = Reset(previous);
    action()
}

fn session_data_dir() -> Result<std::path::PathBuf, String> {
    #[cfg(test)]
    if let Some(path) = TEST_DATA_DIR_OVERRIDE.with(|override_path| override_path.borrow().clone())
    {
        fs::create_dir_all(&path)
            .map_err(|error| format!("Failed to create test data dir: {error}"))?;
        return Ok(path);
    }

    paths::data_dir()
}

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
    fn phase(&self) -> PostProcessorPhase {
        PostProcessorPhase::AfterCommit
    }

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
        if let Some(session) = &result.session {
            let committed = context
                .committed_report()
                .ok_or_else(|| "session record requires a committed report".to_string())?;
            let report_relative_path = result
                .json_path
                .as_deref()
                .and_then(|path| path.strip_prefix(&committed.report_dir).ok())
                .or_else(|| {
                    result
                        .artifact_paths
                        .iter()
                        .find(|path| {
                            path.file_name()
                                .and_then(std::ffi::OsStr::to_str)
                                .is_some_and(|name| name == "report.json" || name == "report.zip")
                        })
                        .and_then(|path| path.strip_prefix(&committed.report_dir).ok())
                })
                .and_then(std::path::Path::to_str);
            record_crash(session, &committed, report_relative_path, context)?;
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
    /// Stable event identity. Consumers should resolve this ID through a
    /// committed report manifest instead of treating `report` as a path.
    ///
    /// `default` keeps records written before `ReportId` was introduced
    /// readable, while `skip_serializing_if` preserves the legacy shape for
    /// callers that do not execute inside an artifact transaction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    report_id: Option<String>,
    /// Final artifact path relative to the committed report directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    report: Option<String>,
    /// Exact committed manifest path. Legacy rows omit this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    manifest: Option<String>,
}

/// Record a crash in sessions.jsonl and remove session.lock.
fn record_crash(
    session: &crate::pipeline::report::SessionReport,
    committed: &CommittedReport,
    report_relative_path: Option<&str>,
    context: &PluginContext,
) -> Result<(), String> {
    context.checkpoint()?;
    let dir = match session_data_dir() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[monitor] Failed to get data dir: {e}");
            return Ok(());
        }
    };

    record_crash_in_dir(session, committed, report_relative_path, &dir, context)
}

fn record_crash_in_dir(
    session: &crate::pipeline::report::SessionReport,
    committed: &CommittedReport,
    report_relative_path: Option<&str>,
    dir: &std::path::Path,
    context: &PluginContext,
) -> Result<(), String> {
    context.checkpoint()?;
    let record = SessionRecord {
        id: session.id.clone(),
        start: session.start.clone(),
        end: Local::now().to_rfc3339(),
        status: "crash".into(),
        duration_s: session.duration_s, // precomputed, no drift
        report_id: Some(committed.report_id.as_str().to_string()),
        report: report_relative_path.map(str::to_string),
        manifest: Some(committed.manifest_path.to_string_lossy().into_owned()),
    };

    let json = serde_json::to_string(&record)
        .map_err(|error| format!("cannot serialize session record: {error}"))?;
    let jsonl_path = dir.join("sessions.jsonl");
    append_session_record(&jsonl_path, &json, context)?;

    let lock_path = dir.join("session.lock");
    match fs::remove_file(&lock_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "cannot remove committed session lock '{}': {error}",
                lock_path.display()
            ));
        }
    }
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
    file.sync_all()
        .map_err(|error| format!("cannot sync '{}': {error}", path.display()))?;
    context.checkpoint()
}

#[cfg(test)]
#[path = "../../tests/unit/postprocessors/session_recorder_tests.rs"]
mod tests;
