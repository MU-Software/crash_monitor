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
use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::sync::{LazyLock, Mutex, MutexGuard, TryLockError};
use std::thread;
use std::time::Duration;

#[cfg(test)]
thread_local! {
    static TEST_DATA_DIR_OVERRIDE: std::cell::RefCell<Option<std::path::PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

const MAX_SESSION_RECORD_BYTES: usize = 16 * 1024;
const MAX_SESSION_LOCK_BYTES: u64 = 4 * 1024;
const SESSION_LOG_LOCK_FILE: &str = ".sessions.lock";
const SESSION_LOG_LOCK_POLL: Duration = Duration::from_millis(5);
static SESSION_LOG_PROCESS_LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

pub(super) struct SessionLogLock {
    _process: MutexGuard<'static, ()>,
    _file: Flock<fs::File>,
}

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
    let _log_lock = acquire_session_log_lock(dir, context)?;
    let jsonl_path = dir.join("sessions.jsonl");
    append_session_record_locked(&jsonl_path, &json, context)?;

    let lock_path = dir.join("session.lock");
    remove_owned_session_lock(&lock_path, &session.id)?;
    context.checkpoint()?;
    Ok(())
}

fn remove_owned_session_lock(path: &std::path::Path, expected_owner: &str) -> Result<(), String> {
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
    if opened.len() > MAX_SESSION_LOCK_BYTES {
        return Err(format!(
            "committed session lock exceeds {MAX_SESSION_LOCK_BYTES} bytes: '{}'",
            path.display()
        ));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(opened.len()).unwrap_or(0));
    file.take(MAX_SESSION_LOCK_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read committed session lock: {error}"))?;
    if bytes.len() as u64 > MAX_SESSION_LOCK_BYTES {
        return Err(format!(
            "committed session lock grew beyond {MAX_SESSION_LOCK_BYTES} bytes"
        ));
    }
    let owner = bytes
        .split(|byte| *byte == b'\n')
        .next()
        .unwrap_or_default();
    if owner != expected_owner.as_bytes() {
        return Ok(());
    }
    fs::remove_file(path).map_err(|error| {
        format!(
            "cannot remove committed session lock '{}': {error}",
            path.display()
        )
    })?;
    sync_parent_directory(path)
}

#[cfg(test)]
pub(super) fn append_session_record(
    path: &std::path::Path,
    json: &str,
    context: &PluginContext,
) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("session log has no parent: '{}'", path.display()))?;
    ensure_private_directory(parent)
        .map_err(|error| format!("cannot prepare private session directory: {error}"))?;
    let _log_lock = acquire_session_log_lock(parent, context)?;
    append_session_record_locked(path, json, context)
}

fn append_session_record_locked(
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

pub(super) fn acquire_session_log_lock(
    dir: &std::path::Path,
    context: &PluginContext,
) -> Result<SessionLogLock, String> {
    let process = loop {
        context.checkpoint()?;
        match SESSION_LOG_PROCESS_LOCK.try_lock() {
            Ok(lock) => break lock,
            Err(TryLockError::Poisoned(poisoned)) => break poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => thread::sleep(SESSION_LOG_LOCK_POLL),
        }
    };

    let lock_path = dir.join(SESSION_LOG_LOCK_FILE);
    let (mut file, created) = open_session_log_lock_file(&lock_path)?;
    if created {
        sync_parent_directory(&lock_path)?;
    }
    loop {
        context.checkpoint()?;
        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(file_lock) => {
                return Ok(SessionLogLock {
                    _process: process,
                    _file: file_lock,
                });
            }
            Err((returned_file, Errno::EWOULDBLOCK)) => {
                file = returned_file;
                thread::sleep(SESSION_LOG_LOCK_POLL);
            }
            Err((_returned_file, error)) => {
                return Err(format!(
                    "cannot acquire session log lock '{}': {error}",
                    lock_path.display()
                ));
            }
        }
    }
}

fn open_session_log_lock_file(path: &std::path::Path) -> Result<(fs::File, bool), String> {
    let open_existing = || {
        fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
            .open(path)
    };
    let (file, created) = match open_existing() {
        Ok(file) => (file, false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match create_private_file(path) {
                Ok(file) => (file, true),
                Err(create_error) => (
                    open_existing().map_err(|open_error| {
                        format!(
                            "cannot create private session log lock '{}': {create_error}; retry open failed: {open_error}",
                            path.display()
                        )
                    })?,
                    false,
                ),
            }
        }
        Err(error) => {
            return Err(format!(
                "cannot open session log lock '{}': {error}",
                path.display()
            ));
        }
    };
    validate_private_file(&file, path).map_err(|error| {
        format!(
            "cannot validate session log lock '{}': {error}",
            path.display()
        )
    })?;
    Ok((file, created))
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
