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
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const RATE_STATE_VERSION: u32 = 1;
const MAX_PERSISTED_EVENTS_PER_TYPE: usize = 1_024;
const MAX_RATE_STATE_BYTES: u64 = 128 * 1024;

#[derive(Default)]
struct PersistentState {
    loaded: bool,
    recent: HashMap<ReportType, VecDeque<u64>>,
}

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
    persistent: Mutex<PersistentState>,
}

impl RateLimiter {
    #[must_use]
    pub fn new(max_events: usize, window: Duration) -> Self {
        Self {
            window,
            max_events,
            recent: Mutex::new(HashMap::new()),
            state_path: None,
            persistent: Mutex::new(PersistentState::default()),
        }
    }

    #[must_use]
    pub fn with_state_path(max_events: usize, window: Duration, state_path: PathBuf) -> Self {
        Self {
            window,
            max_events: max_events.min(MAX_PERSISTED_EVENTS_PER_TYPE),
            recent: Mutex::new(HashMap::new()),
            state_path: Some(state_path),
            persistent: Mutex::new(PersistentState::default()),
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
        let mut state = self
            .persistent
            .try_lock()
            .map_err(|error| format!("persistent rate-limit state unavailable: {error}"))?;
        context.checkpoint()?;
        if !state.loaded {
            state.recent = load_state(path)?;
            state.loaded = true;
        }

        let window_ms = u64::try_from(self.window.as_millis()).unwrap_or(u64::MAX);
        let cutoff = now_ms.saturating_sub(window_ms);
        let entry = state.recent.entry(report_type).or_default();
        while entry.front().is_some_and(|timestamp| *timestamp <= cutoff) {
            entry.pop_front();
        }
        if entry.len() >= self.max_events {
            return Ok(false);
        }
        entry.push_back(now_ms);
        persist_state(path, &state.recent)?;
        context.checkpoint()?;
        Ok(true)
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

fn load_state(path: &Path) -> Result<HashMap<ReportType, VecDeque<u64>>, String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(error) => return Err(format!("cannot inspect rate-limit state: {error}")),
    };
    if metadata.len() > MAX_RATE_STATE_BYTES {
        return Err(format!(
            "rate-limit state exceeds {MAX_RATE_STATE_BYTES} bytes"
        ));
    }
    let file = crate::utils::paths::open_private_file(path)?;
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    file.take(MAX_RATE_STATE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read rate-limit state: {error}"))?;
    if bytes.len() as u64 > MAX_RATE_STATE_BYTES {
        return Err(format!(
            "rate-limit state grew beyond {MAX_RATE_STATE_BYTES} bytes"
        ));
    }
    let persisted: PersistedRateState = serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid rate-limit state: {error}"))?;
    if persisted.version != RATE_STATE_VERSION {
        return Err(format!(
            "unsupported rate-limit state version {}",
            persisted.version
        ));
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
