//! Pre-processor: detect duplicate crashes using the Fingerprinter's hash.
//!
//! Maintains a time-windowed history of recent fingerprints. If the current
//! event's fingerprint was already seen within the window, sets
//! `data.duplicate_detected = true` so the pipeline skips report writing.

use crate::pipeline::{
    CollectedData, CrashEvent, Plugin, PluginContext, PluginExecution, PreProcessor, Priority,
};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct DuplicateDetector {
    window: Duration,
    recent: Mutex<HashMap<DuplicateKey, DuplicateEntry>>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct DuplicateKey {
    fingerprint: String,
    report_type: crate::pipeline::ReportType,
    severity: &'static str,
    process_name: String,
    build_identity: String,
}

#[derive(Clone, Debug)]
struct DuplicateEntry {
    /// Suppression windows are anchored to the first accepted occurrence.
    first_seen: Instant,
    /// Observation metadata does not influence window expiry.
    last_observed: Instant,
    occurrences: u64,
}

impl DuplicateDetector {
    #[must_use]
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            recent: Mutex::new(HashMap::new()),
        }
    }

    fn key(event: &CrashEvent, data: &CollectedData, fingerprint: String) -> DuplicateKey {
        let severity = if event.is_crash() {
            "fatal"
        } else {
            "non_fatal"
        };
        let build_identity = data.build_info.as_ref().map_or_else(
            || {
                data.raw.crash_context.as_ref().map_or_else(
                    || "unknown-build".to_string(),
                    |build| {
                        format!(
                            "{}:{}:{}:{}",
                            build.app_version, build.build_number, build.git_hash, build.build_type
                        )
                    },
                )
            },
            |build| {
                format!(
                    "{}:{}:{}:{}",
                    build.app_version, build.build_number, build.git_hash, build.build_type
                )
            },
        );
        DuplicateKey {
            fingerprint,
            report_type: event.report_type,
            severity,
            process_name: event.process_name.clone(),
            build_identity,
        }
    }

    fn process_at(
        &self,
        event: &CrashEvent,
        data: &mut CollectedData,
        context: &PluginContext,
        now: Instant,
    ) -> Result<(), String> {
        context.checkpoint()?;
        let Some(fingerprint) = data.fingerprint.clone() else {
            return Ok(());
        };
        let key = Self::key(event, data, fingerprint);
        let mut recent = self
            .recent
            .try_lock()
            .map_err(|error| format!("duplicate state unavailable: {error}"))?;
        context.checkpoint()?;

        let window = self.window;
        recent.retain(|_, entry| now.saturating_duration_since(entry.first_seen) < window);
        context.checkpoint()?;

        if let Some(entry) = recent.get_mut(&key) {
            data.duplicate_detected = true;
            entry.last_observed = now;
            entry.occurrences = entry.occurrences.saturating_add(1);
        } else {
            recent.insert(
                key,
                DuplicateEntry {
                    first_seen: now,
                    last_observed: now,
                    occurrences: 1,
                },
            );
        }
        context.checkpoint()?;
        Ok(())
    }
}

impl Plugin for DuplicateDetector {
    fn name(&self) -> &'static str {
        "DuplicateDetector"
    }

    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }

    fn priority(&self) -> Priority {
        Priority::High
    }

    fn hard_dependencies(&self) -> &'static [&'static str] {
        &["Fingerprinter"]
    }
}

impl PreProcessor for DuplicateDetector {
    fn process(
        &self,
        event: &CrashEvent,
        data: &mut CollectedData,
        context: &PluginContext,
    ) -> Result<(), String> {
        self.process_at(event, data, context, Instant::now())
    }
}

#[cfg(test)]
#[path = "../../tests/unit/preprocessors/duplicate_tests.rs"]
mod tests;
