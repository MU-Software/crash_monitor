//! Core types for the plugin pipeline.

use mach2::port::mach_port_t;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::report::SessionReport;
use crate::collectors::RawData;

// ═══════════════════════════════════════════════════
//  ReportType
// ═══════════════════════════════════════════════════

/// Category of pipeline event, distinguishing the data shape and the
/// downstream policy (rate limit quota, fingerprint scheme, feedback message).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportType {
    Crash,
    Snapshot,
    Anr,
    Oom,
    ExitFailure,
    SignalFailure,
}

impl ReportType {
    /// Stable string representation used in filenames, CLI args, and JSON.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ReportType::Crash => "crash",
            ReportType::Snapshot => "snapshot",
            ReportType::Anr => "anr",
            ReportType::Oom => "oom",
            ReportType::ExitFailure => "exit_failure",
            ReportType::SignalFailure => "signal_failure",
        }
    }
}

impl std::fmt::Display for ReportType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ═══════════════════════════════════════════════════
//  CrashEvent
// ═══════════════════════════════════════════════════

/// How a monitored child process terminated.
///
/// The internally tagged representation keeps the reason and its metadata in
/// one unambiguously self-describing object.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TerminationReason {
    Exited {
        exit_code: i32,
        runtime_ms: u64,
    },
    Signaled {
        signal: i32,
        core_dumped: bool,
        runtime_ms: u64,
    },
}

/// Event data produced by a trigger. Owns all data (no lifetimes).
pub struct CrashEvent {
    pub report_type: ReportType,
    /// Process termination metadata for exit/signal failure reports.
    pub termination: Option<TerminationReason>,
    pub exception_type: Option<u32>,
    pub exception_code: Option<u64>,
    pub exception_subcode: Option<u64>,
    pub crashed_thread: Option<mach_port_t>,
    pub bail_on_suspend_failure: bool,
    pub pid: u32,
    pub process_name: String,
    /// ANR-specific: how long the app was unresponsive before triggering.
    pub hang_duration_ms: Option<u64>,
}

impl CrashEvent {
    #[must_use]
    pub fn is_crash(&self) -> bool {
        self.report_type == ReportType::Crash
    }
}

// ═══════════════════════════════════════════════════
//  Priority
// ═══════════════════════════════════════════════════

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[allow(dead_code)] // Phase 4+: used for plugin ordering by priority
pub enum Priority {
    Critical,
    High,
    Medium,
    Low,
}

// ═══════════════════════════════════════════════════
//  Diagnostics
// ═══════════════════════════════════════════════════

#[derive(Debug)]
pub enum PluginStatus {
    Ok,
    Error(String),
    Skipped(String),
}

pub struct PluginDiagnostic {
    pub name: String,
    pub status: PluginStatus,
    pub duration_ms: u64,
}

pub struct Diagnostics {
    pub plugins: Vec<PluginDiagnostic>,
    /// Final report artifact after post-processing (JSON or ZIP, possibly
    /// relocated). `None` means the event was filtered or report creation
    /// failed.
    pub report_path: Option<PathBuf>,
    start: Instant,
}

impl Default for Diagnostics {
    fn default() -> Self {
        Self::new()
    }
}

impl Diagnostics {
    #[must_use]
    pub fn new() -> Self {
        Self {
            plugins: Vec::new(),
            report_path: None,
            start: Instant::now(),
        }
    }

    pub fn record(&mut self, name: &str, status: PluginStatus, duration: Duration) {
        self.plugins.push(PluginDiagnostic {
            name: name.to_string(),
            status,
            #[allow(clippy::cast_possible_truncation)]
            duration_ms: duration.as_millis() as u64,
        });
    }

    pub fn record_immediate(&mut self, name: &str, status: PluginStatus) {
        self.record(name, status, Duration::ZERO);
    }

    #[must_use]
    pub fn succeeded(&self, name: &str) -> bool {
        self.plugins
            .iter()
            .any(|d| d.name == name && matches!(d.status, PluginStatus::Ok))
    }

    #[must_use]
    pub fn pipeline_duration_ms(&self) -> u64 {
        #[allow(clippy::cast_possible_truncation)]
        {
            self.start.elapsed().as_millis() as u64
        }
    }
}

// ═══════════════════════════════════════════════════
//  ReportResult
// ═══════════════════════════════════════════════════

pub struct ReportResult {
    pub raw_path: Option<PathBuf>,
    pub json_path: Option<PathBuf>,
    pub session: Option<SessionReport>,
}

// ═══════════════════════════════════════════════════
//  CollectedData
// ═══════════════════════════════════════════════════

/// Aggregated introspection data, populated by collectors, enriched by pre-processors.
#[derive(Default)]
pub struct CollectedData {
    pub raw: RawData,
    pub session: Option<SessionReport>,
    /// Crash fingerprint hash (set by `Fingerprinter` pre-processor, Phase 5).
    pub fingerprint: Option<String>,
    /// Structured build metadata (set by `BuildInfoEnricher` pre-processor).
    pub build_info: Option<crate::preprocessors::build_info::BuildInfo>,
    /// Set by `DuplicateDetector` when this event's fingerprint matches a recent one.
    /// When true, the pipeline skips Stage 2 report writing.
    pub duplicate_detected: bool,
}

#[cfg(test)]
#[path = "../../tests/unit/pipeline/types_tests.rs"]
mod tests;
