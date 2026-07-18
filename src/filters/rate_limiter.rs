//! Filter: cap report rate per `report_type` within a sliding time window.
//!
//! Independent of `DuplicateDetector` (which gates by fingerprint).
//! `RateLimiter` blocks any burst regardless of cause — protects disk/CPU
//! from runaway report generation. Production construction supplies a bounded
//! state file so a monitor restart cannot reset crash-loop protection.

use crate::pipeline::{
    CrashEvent, Filter, Plugin, PluginContext, PluginExecution, Priority, ReportType,
};
use std::collections::{HashMap, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, TryLockError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};
use serde::{Deserialize, Serialize};

const RATE_STATE_VERSION: u32 = 1;
const MAX_PERSISTED_EVENTS_PER_TYPE: usize = 1_024;
const MAX_RATE_STATE_BYTES: u64 = 128 * 1024;
const RATE_STATE_LOCK_FILE: &str = ".rate-limit-state.lock";
const RATE_STATE_LOCK_POLL: Duration = Duration::from_millis(2);

static RATE_STATE_PROCESS_LOCK: Mutex<()> = Mutex::new(());

#[derive(Serialize, Deserialize)]
struct PersistedRateState {
    version: u32,
    entries: Vec<PersistedRateEntry>,
}

#[derive(Serialize, Deserialize)]
struct PersistedRateEntry {
    report_type: ReportType,
    timestamps_ms: Vec<u64>,
}

pub struct RateLimiter {
    window: Duration,
    max_events: usize,
    recent: Mutex<HashMap<ReportType, VecDeque<Instant>>>,
    state_path: Option<PathBuf>,
}

impl RateLimiter {
    #[must_use]
    pub fn new(max_events: usize, window: Duration) -> Self {
        Self {
            window,
            max_events,
            recent: Mutex::new(HashMap::new()),
            state_path: None,
        }
    }

    #[must_use]
    pub fn with_state_path(max_events: usize, window: Duration, state_path: PathBuf) -> Self {
        Self {
            window,
            max_events: max_events.min(MAX_PERSISTED_EVENTS_PER_TYPE),
            recent: Mutex::new(HashMap::new()),
            state_path: Some(state_path),
        }
    }

    fn check_and_record(
        &self,
        report_type: ReportType,
        now: Instant,
        context: &PluginContext,
    ) -> Result<bool, String> {
        context.checkpoint()?;
        let Ok(mut recent) = self.recent.try_lock() else {
            // A deadline cannot interrupt Mutex::lock. This state is only
            // advisory, so contention or poisoning fails open rather than
            // waiting behind another plugin invocation.
            return Ok(true);
        };
        context.checkpoint()?;
        let entry = recent.entry(report_type).or_default();
        while let Some(&front) = entry.front() {
            context.checkpoint()?;
            if now.duration_since(front) >= self.window {
                entry.pop_front();
            } else {
                break;
            }
        }
        if entry.len() >= self.max_events {
            return Ok(false);
        }
        entry.push_back(now);
        context.checkpoint()?;
        Ok(true)
    }

    fn check_and_record_persistent(
        &self,
        report_type: ReportType,
        now_ms: u64,
        context: &PluginContext,
    ) -> Result<bool, String> {
        let path = self
            .state_path
            .as_deref()
            .ok_or_else(|| "persistent rate-limit path is unavailable".to_string())?;
        let _lock = acquire_rate_state_lock(path, context)?;
        context.checkpoint()?;
        let (mut recent, recovery_reason) = match load_state(path) {
            Ok(recent) => (recent, None),
            Err(StateLoadError::Recoverable(error)) => {
                quarantine_invalid_state(path)?;
                (HashMap::new(), Some(error))
            }
            Err(StateLoadError::Fatal(error)) => return Err(error),
        };

        let window_ms = u64::try_from(self.window.as_millis()).unwrap_or(u64::MAX);
        let cutoff = now_ms.saturating_sub(window_ms);
        let entry = recent.entry(report_type).or_default();
        while entry.front().is_some_and(|timestamp| *timestamp <= cutoff) {
            entry.pop_front();
        }
        if entry.len() >= self.max_events {
            return Ok(false);
        }
        entry.push_back(now_ms);
        persist_state(path, &recent)?;
        context.checkpoint()?;
        match recovery_reason {
            Some(reason) => Err(format!(
                "{reason}; the invalid state was quarantined and reset, and the current event was recorded in the replacement state"
            )),
            None => Ok(true),
        }
    }
}

impl Plugin for RateLimiter {
    fn name(&self) -> &'static str {
        "RateLimiter"
    }
    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }
    fn priority(&self) -> Priority {
        Priority::Critical
    }
    fn order_after(&self) -> &'static [&'static str] {
        &["DiskSpaceFilter"]
    }
}

impl Filter for RateLimiter {
    fn should_process(&self, event: &CrashEvent, context: &PluginContext) -> Result<bool, String> {
        if self.state_path.is_some() {
            let now_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|error| format!("system clock precedes Unix epoch: {error}"))?
                .as_millis();
            let now_ms = u64::try_from(now_ms).unwrap_or(u64::MAX);
            self.check_and_record_persistent(event.report_type, now_ms, context)
        } else {
            self.check_and_record(event.report_type, Instant::now(), context)
        }
    }
}

enum StateLoadError {
    Recoverable(String),
    Fatal(String),
}

struct RateStateLock {
    _process: MutexGuard<'static, ()>,
    _file: Flock<fs::File>,
}

fn acquire_rate_state_lock(
    state_path: &Path,
    context: &PluginContext,
) -> Result<RateStateLock, String> {
    let parent = state_path
        .parent()
        .ok_or_else(|| "rate-limit state has no parent directory".to_string())?;
    crate::utils::paths::ensure_private_directory(parent)?;

    let process = loop {
        context.checkpoint()?;
        match RATE_STATE_PROCESS_LOCK.try_lock() {
            Ok(lock) => break lock,
            Err(TryLockError::Poisoned(poisoned)) => break poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => thread::sleep(RATE_STATE_LOCK_POLL),
        }
    };

    let lock_path = parent.join(RATE_STATE_LOCK_FILE);
    let (mut file, created) = open_rate_state_lock(&lock_path)?;
    if created {
        crate::utils::paths::open_private_directory(parent)?
            .sync_all()
            .map_err(|error| format!("cannot sync rate-limit state directory: {error}"))?;
    }
    loop {
        context.checkpoint()?;
        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(file_lock) => {
                return Ok(RateStateLock {
                    _process: process,
                    _file: file_lock,
                });
            }
            Err((returned_file, Errno::EWOULDBLOCK)) => {
                file = returned_file;
                thread::sleep(RATE_STATE_LOCK_POLL);
            }
            Err((_returned_file, error)) => {
                return Err(format!(
                    "cannot acquire rate-limit state lock '{}': {error}",
                    lock_path.display()
                ));
            }
        }
    }
}

fn open_rate_state_lock(path: &Path) -> Result<(fs::File, bool), String> {
    let open_existing = || {
        OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
            .open(path)
    };
    let (file, created) = match open_existing() {
        Ok(file) => (file, false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match crate::utils::paths::create_private_file(path) {
                Ok(file) => (file, true),
                Err(create_error) => (
                    open_existing().map_err(|open_error| {
                        format!(
                            "cannot create private rate-limit lock '{}': {create_error}; retry open failed: {open_error}",
                            path.display()
                        )
                    })?,
                    false,
                ),
            }
        }
        Err(error) => {
            return Err(format!(
                "cannot open rate-limit state lock '{}': {error}",
                path.display()
            ));
        }
    };
    crate::utils::paths::validate_private_file(&file, path).map_err(|error| {
        format!(
            "cannot validate rate-limit state lock '{}': {error}",
            path.display()
        )
    })?;
    Ok((file, created))
}

fn load_state(path: &Path) -> Result<HashMap<ReportType, VecDeque<u64>>, StateLoadError> {
    let Some(file) =
        crate::utils::paths::open_private_file_optional(path).map_err(StateLoadError::Fatal)?
    else {
        return Ok(HashMap::new());
    };
    let metadata = file.metadata().map_err(|error| {
        StateLoadError::Fatal(format!("cannot inspect rate-limit state: {error}"))
    })?;
    if metadata.len() > MAX_RATE_STATE_BYTES {
        return Err(StateLoadError::Recoverable(format!(
            "rate-limit state exceeds {MAX_RATE_STATE_BYTES} bytes"
        )));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    file.take(MAX_RATE_STATE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| StateLoadError::Fatal(format!("cannot read rate-limit state: {error}")))?;
    if bytes.len() as u64 > MAX_RATE_STATE_BYTES {
        return Err(StateLoadError::Recoverable(format!(
            "rate-limit state grew beyond {MAX_RATE_STATE_BYTES} bytes"
        )));
    }
    let persisted: PersistedRateState = serde_json::from_slice(&bytes).map_err(|error| {
        StateLoadError::Recoverable(format!("invalid rate-limit state: {error}"))
    })?;
    if persisted.version != RATE_STATE_VERSION {
        return Err(StateLoadError::Recoverable(format!(
            "unsupported rate-limit state version {}",
            persisted.version
        )));
    }
    let mut recent = HashMap::new();
    for entry in persisted.entries {
        let timestamps = entry
            .timestamps_ms
            .into_iter()
            .rev()
            .take(MAX_PERSISTED_EVENTS_PER_TYPE)
            .collect::<Vec<_>>();
        recent.insert(
            entry.report_type,
            timestamps.into_iter().rev().collect::<VecDeque<_>>(),
        );
    }
    Ok(recent)
}

fn quarantine_invalid_state(path: &Path) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "rate-limit state has no parent directory".to_string())?;
    let quarantine = parent.join(format!(
        ".rate-limit-state.corrupt.{}.json",
        uuid::Uuid::new_v4().simple()
    ));
    fs::rename(path, &quarantine)
        .map_err(|error| format!("cannot quarantine invalid rate-limit state: {error}"))?;
    crate::utils::paths::open_private_directory(parent)?
        .sync_all()
        .map_err(|error| format!("cannot sync rate-limit state directory: {error}"))
}

fn persist_state(path: &Path, recent: &HashMap<ReportType, VecDeque<u64>>) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "rate-limit state has no parent directory".to_string())?;
    crate::utils::paths::ensure_private_directory(parent)?;
    let mut entries = recent
        .iter()
        .map(|(report_type, timestamps)| PersistedRateEntry {
            report_type: *report_type,
            timestamps_ms: timestamps.iter().copied().collect(),
        })
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.report_type.as_str());
    let bytes = serde_json::to_vec(&PersistedRateState {
        version: RATE_STATE_VERSION,
        entries,
    })
    .map_err(|error| format!("cannot serialize rate-limit state: {error}"))?;
    if bytes.len() as u64 > MAX_RATE_STATE_BYTES {
        return Err(format!(
            "serialized rate-limit state exceeds {MAX_RATE_STATE_BYTES} bytes"
        ));
    }
    let temp = parent.join(format!(
        ".rate-limit.{}.{}.tmp",
        std::process::id(),
        uuid::Uuid::new_v4().simple()
    ));
    let mut file = crate::utils::paths::create_private_file(&temp)?;
    file.write_all(&bytes)
        .map_err(|error| format!("cannot write rate-limit state: {error}"))?;
    file.sync_all()
        .map_err(|error| format!("cannot sync rate-limit state: {error}"))?;
    drop(file);
    std::fs::rename(&temp, path).map_err(|error| {
        let _ = std::fs::remove_file(&temp);
        format!("cannot publish rate-limit state: {error}")
    })?;
    crate::utils::paths::open_private_directory(parent)?
        .sync_all()
        .map_err(|error| format!("cannot sync rate-limit state directory: {error}"))
}

#[cfg(test)]
#[path = "../../tests/unit/filters/rate_limiter_tests.rs"]
mod tests;
