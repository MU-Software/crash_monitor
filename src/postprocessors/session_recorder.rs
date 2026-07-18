//! Post-processor: records crash in sessions.jsonl (crash events only).
//!
//! Self-contained — absorbs `record_crash` from `session.rs`.

use crate::pipeline::{
    CommittedReport, CrashEvent, Plugin, PluginContext, PluginExecution, PostProcessor,
    PostProcessorPhase, Priority, ReportResult,
};
use crate::utils::paths;
use crate::utils::paths::{
    create_private_file, ensure_private_directory, open_private_directory, open_private_file,
    validate_private_file,
};
use chrono::Local;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

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
        ensure_private_directory(&path)
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
    let dir = match context
        .artifact_transaction()
        .map(|transaction| transaction.report_context().output_root().to_path_buf())
        .map_or_else(session_data_dir, Ok)
    {
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
    ensure_private_directory(dir)
        .map_err(|error| format!("cannot prepare private session directory: {error}"))?;
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
    remove_private_session_lock(&lock_path)?;
    context.checkpoint()?;
    Ok(())
}

fn remove_private_session_lock(path: &std::path::Path) -> Result<(), String> {
    let named = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "cannot inspect committed session lock '{}': {error}",
                path.display()
            ));
        }
    };
    let file = open_private_file(path)
        .map_err(|error| format!("cannot validate session lock '{}': {error}", path.display()))?;
    let opened = file
        .metadata()
        .map_err(|error| format!("cannot inspect opened session lock: {error}"))?;
    if named.dev() != opened.dev() || named.ino() != opened.ino() {
        return Err(format!(
            "committed session lock changed before removal: '{}'",
            path.display()
        ));
    }
    fs::remove_file(path).map_err(|error| {
        format!(
            "cannot remove committed session lock '{}': {error}",
            path.display()
        )
    })?;
    sync_parent_directory(path)
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
    let parent = path
        .parent()
        .ok_or_else(|| format!("session log has no parent: '{}'", path.display()))?;
    open_private_directory(parent)
        .map_err(|error| format!("cannot prepare private session directory: {error}"))?;
    let (mut file, created) = open_private_session_log(path)?;
    context.checkpoint()?;
    file.write_all(json.as_bytes())
        .and_then(|()| file.write_all(b"\n"))
        .map_err(|error| format!("cannot append '{}': {error}", path.display()))?;
    file.sync_all()
        .map_err(|error| format!("cannot sync '{}': {error}", path.display()))?;
    if created {
        sync_parent_directory(path)?;
    }
    context.checkpoint()
}

fn open_private_session_log(path: &std::path::Path) -> Result<(fs::File, bool), String> {
    match open_existing_session_log(path) {
        Ok(file) => Ok((file, false)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let created = match create_private_file(path) {
                Ok(file) => {
                    drop(file);
                    true
                }
                Err(create_error) => {
                    let file = open_existing_session_log(path).map_err(|open_error| {
                        format!(
                            "cannot create private session log '{}': {create_error}; retry open failed: {open_error}",
                            path.display()
                        )
                    })?;
                    validate_private_file(&file, path).map_err(|validation_error| {
                        format!(
                            "cannot validate raced session log '{}': {validation_error}",
                            path.display()
                        )
                    })?;
                    return Ok((file, false));
                }
            };
            let file = open_existing_session_log(path).map_err(|open_error| {
                format!(
                    "cannot reopen private session log '{}': {open_error}",
                    path.display()
                )
            })?;
            validate_private_file(&file, path).map_err(|validation_error| {
                format!(
                    "cannot validate private session log '{}': {validation_error}",
                    path.display()
                )
            })?;
            Ok((file, created))
        }
        Err(error) => Err(format!("cannot open '{}': {error}", path.display())),
    }
    .and_then(|(file, created)| {
        validate_private_file(&file, path)
            .map_err(|error| format!("cannot validate '{}': {error}", path.display()))?;
        Ok((file, created))
    })
}

fn open_existing_session_log(path: &std::path::Path) -> std::io::Result<fs::File> {
    fs::OpenOptions::new()
        .append(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)
}

fn sync_parent_directory(path: &std::path::Path) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("session log has no parent: '{}'", path.display()))?;
    open_private_directory(parent)?.sync_all().map_err(|error| {
        format!(
            "cannot sync session directory '{}': {error}",
            parent.display()
        )
    })
}

#[cfg(test)]
#[path = "../../tests/unit/postprocessors/session_recorder_tests.rs"]
mod tests;
