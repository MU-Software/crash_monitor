//! Pre-processor: detect duplicate crashes using the Fingerprinter's hash.
//!
//! Maintains a time-windowed history of recent fingerprints. If the current
//! event's fingerprint was already seen within the window, sets
//! `data.duplicate_detected = true` so the pipeline skips report writing.

use crate::pipeline::{CollectedData, CrashEvent, Plugin, PreProcessor, Priority};
use mach2::port::mach_port_t;
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

    fn priority(&self) -> Priority {
        Priority::High
    }

    fn depends_on(&self) -> &'static [&'static str] {
        &["Fingerprinter"]
    }
}

impl PreProcessor for DuplicateDetector {
    fn process(
        &self,
        _event: &CrashEvent,
        _task: mach_port_t,
        data: &mut CollectedData,
    ) -> Result<(), String> {
        let fp = match &data.fingerprint {
            Some(fp) => fp.clone(),
            None => return Ok(()), // No fingerprint → cannot deduplicate → pass through
        };

        let mut recent = self
            .recent
            .lock()
            .map_err(|e| format!("lock poisoned: {e}"))?;
        let now = Instant::now();

        // Evict expired entries
        let window = self.window;
        recent.retain(|_, ts| now.duration_since(*ts) < window);

        // Check for duplicate
        if recent.contains_key(&fp) {
            data.duplicate_detected = true;
        }

        // Record this fingerprint (even if duplicate — updates timestamp)
        recent.insert(fp, now);

        Ok(())
    }
}

#[cfg(test)]
#[path = "../../tests/unit/preprocessors/duplicate_tests.rs"]
mod tests;
