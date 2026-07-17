//! Filter: cap report rate per `report_type` within a sliding time window.
//!
//! Independent of `DuplicateDetector` (which gates by fingerprint).
//! `RateLimiter` blocks any burst regardless of cause — protects disk/CPU
//! from runaway report generation. State is in-memory only; monitor restart
//! resets the window.

use crate::pipeline::{CrashEvent, Filter, Plugin, Priority, ReportType};
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub struct RateLimiter {
    window: Duration,
    max_events: usize,
    recent: Mutex<HashMap<ReportType, VecDeque<Instant>>>,
}

impl RateLimiter {
    #[must_use]
    pub fn new(max_events: usize, window: Duration) -> Self {
        Self {
            window,
            max_events,
            recent: Mutex::new(HashMap::new()),
        }
    }

    fn check_and_record(&self, report_type: ReportType, now: Instant) -> bool {
        let Ok(mut recent) = self.recent.lock() else {
            // Poisoned mutex: fail open (allow the report through).
            return true;
        };
        let entry = recent.entry(report_type).or_default();
        while let Some(&front) = entry.front() {
            if now.duration_since(front) >= self.window {
                entry.pop_front();
            } else {
                break;
            }
        }
        if entry.len() >= self.max_events {
            return false;
        }
        entry.push_back(now);
        true
    }
}

impl Plugin for RateLimiter {
    fn name(&self) -> &'static str {
        "RateLimiter"
    }
    fn priority(&self) -> Priority {
        Priority::Critical
    }
}

impl Filter for RateLimiter {
    fn should_process(&self, event: &CrashEvent) -> Result<bool, String> {
        Ok(self.check_and_record(event.report_type, Instant::now()))
    }
}

#[cfg(test)]
#[path = "../../tests/unit/filters/rate_limiter_tests.rs"]
mod tests;
