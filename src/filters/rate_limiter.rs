//! Filter: cap report rate per `report_type` within a sliding time window.
//!
//! Independent of `DuplicateDetector` (which gates by fingerprint).
//! `RateLimiter` blocks any burst regardless of cause — protects disk/CPU
//! from runaway report generation. State is in-memory only; monitor restart
//! resets the window.

use crate::pipeline::{
    CrashEvent, Filter, Plugin, PluginContext, PluginExecution, Priority, ReportType,
};
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
}

impl Filter for RateLimiter {
    fn should_process(&self, event: &CrashEvent, context: &PluginContext) -> Result<bool, String> {
        self.check_and_record(event.report_type, Instant::now(), context)
    }
}

#[cfg(test)]
#[path = "../../tests/unit/filters/rate_limiter_tests.rs"]
mod tests;
