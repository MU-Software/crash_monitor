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
    recent: Mutex<HashMap<String, Instant>>,
}

impl DuplicateDetector {
    #[must_use]
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            recent: Mutex::new(HashMap::new()),
        }
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
        _event: &CrashEvent,
        data: &mut CollectedData,
        context: &PluginContext,
    ) -> Result<(), String> {
        context.checkpoint()?;
        let fp = match &data.fingerprint {
            Some(fp) => fp.clone(),
            None => return Ok(()), // No fingerprint → cannot deduplicate → pass through
        };

        let mut recent = self
            .recent
            .try_lock()
            .map_err(|error| format!("duplicate state unavailable: {error}"))?;
        context.checkpoint()?;
        let now = Instant::now();

        // Evict expired entries
        let window = self.window;
        let mut expired = Vec::new();
        for (fingerprint, timestamp) in recent.iter() {
            context.checkpoint()?;
            if now.duration_since(*timestamp) >= window {
                expired.push(fingerprint.clone());
            }
        }
        for fingerprint in expired {
            context.checkpoint()?;
            recent.remove(&fingerprint);
        }

        // Check for duplicate
        if recent.contains_key(&fp) {
            data.duplicate_detected = true;
        }

        // Record this fingerprint (even if duplicate — updates timestamp)
        recent.insert(fp, now);

        context.checkpoint()?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "../../tests/unit/preprocessors/duplicate_tests.rs"]
mod tests;
