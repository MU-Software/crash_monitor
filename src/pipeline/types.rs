//! Core types for the plugin pipeline.

use mach2::port::mach_port_t;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::artifact::ReportId;
use super::report::SessionReport;
use crate::collectors::RawData;

// ═══════════════════════════════════════════════
//  Plugin dependency metadata
// ═══════════════════════════════════════════════

/// Stable process-wide plugin identity.
///
/// The inner value is restricted to `'static` registration data so runtime
/// display names and user-controlled strings cannot become graph identities.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PluginId(&'static str);

impl PluginId {
    #[must_use]
    pub const fn new(id: &'static str) -> Self {
        Self(id)
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        self.0
    }
}

impl std::fmt::Display for PluginId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl AsRef<str> for PluginId {
    fn as_ref(&self) -> &str {
        self.0
    }
}

impl PartialEq<&str> for PluginId {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

/// A typed dependency edge. Hard data requirements and order-only edges are
/// intentionally distinct values and share one representation in every stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PluginDependency {
    pub plugin: PluginId,
    pub kind: DependencyKind,
}

/// Pipeline stage in which a plugin is registered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PluginCategory {
    Filter,
    Collector,
    PreProcessor,
    PostProcessor,
    Notifier,
}

impl std::fmt::Display for PluginCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Filter => "filter",
            Self::Collector => "collector",
            Self::PreProcessor => "preprocessor",
            Self::PostProcessor => "postprocessor",
            Self::Notifier => "notifier",
        })
    }
}

/// Whether a dependency supplies required data or only constrains ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DependencyKind {
    /// The dependent cannot run correctly unless this plugin is enabled and
    /// completes successfully.
    Hard,
    /// If both plugins are enabled this plugin must run first, but its absence
    /// does not disable or skip the dependent.
    OrderOnly,
}

impl std::fmt::Display for DependencyKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Hard => "hard",
            Self::OrderOnly => "order-only",
        })
    }
}

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

/// Evidence level for a SIGKILL termination. SIGKILL alone cannot establish
/// memory pressure because supervisors and users can send the same signal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TerminationEvidence {
    PossibleOom,
    UnknownSigkill,
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
#[derive(Clone, Serialize, Deserialize)]
pub struct CrashEvent {
    /// Identity allocated once by the trigger and preserved by every clone and
    /// capture/finalization handoff for this logical event.
    pub report_id: ReportId,
    pub report_type: ReportType,
    /// Process termination metadata for exit/signal failure reports.
    pub termination: Option<TerminationReason>,
    pub exception_type: Option<u32>,
    pub exception_code: Option<u64>,
    pub exception_subcode: Option<u64>,
    /// Exact raw Mach exception code array. Empty for non-Mach triggers.
    pub exception_codes: Vec<u64>,
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

/// Explicit timeout policy for a plugin invocation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginTimeout {
    /// Inherit the bounded timeout configured for the plugin category.
    CategoryDefault,
    /// Run without a deadline. This is intentionally distinct from a zero
    /// duration, which would expire immediately.
    Disabled,
    /// Use this plugin-specific deadline.
    Override(Duration),
}

impl PluginTimeout {
    #[must_use]
    pub fn resolve(self, category_default: Duration) -> Option<Duration> {
        match self {
            Self::CategoryDefault => Some(category_default),
            Self::Disabled => None,
            Self::Override(timeout) => Some(timeout),
        }
    }
}

// ═══════════════════════════════════════════════════
//  Diagnostics
// ═══════════════════════════════════════════════════

#[derive(Debug, Serialize, Deserialize)]
pub enum PluginStatus {
    Ok,
    Partial(String),
    Rejected(String),
    Error(String),
    Panic(String),
    /// The plugin's absolute deadline or cancellation token fired.
    TimedOut,
    Skipped(String),
}

/// Bounded evidence captured before any extension point can reject or fail an
/// event. It deliberately contains metadata only: no borrowed SHM views and no
/// unbounded collector payloads.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmergencySnapshot {
    pub report_id: ReportId,
    pub report_type: ReportType,
    pub termination: Option<TerminationReason>,
    pub pid: u32,
    pub captured_unix_ms: u64,
    pub raw_breadcrumb_bytes: usize,
    pub raw_context_bytes: usize,
}

impl EmergencySnapshot {
    fn new(event: &CrashEvent, raw_shm: Option<&RawShmSnapshot>) -> Self {
        let captured_unix_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| {
                u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
            });
        Self {
            report_id: event.report_id.clone(),
            report_type: event.report_type,
            termination: event.termination,
            pid: event.pid,
            captured_unix_ms,
            raw_breadcrumb_bytes: raw_shm.map_or(0, |raw| raw.breadcrumbs.len()),
            raw_context_bytes: raw_shm.map_or(0, |raw| raw.context.len()),
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct PluginDiagnostic {
    pub name: String,
    pub status: PluginStatus,
    pub duration_ms: u64,
    /// Event identity and monotonic offsets make each stage transition
    /// attributable without relying on a path that later processors may move.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_id: Option<ReportId>,
    #[serde(default)]
    pub started_offset_ms: u64,
    #[serde(default)]
    pub finished_offset_ms: u64,
}

pub struct Diagnostics {
    pub plugins: Vec<PluginDiagnostic>,
    /// Minimum immutable evidence survives filter/duplicate/plugin early
    /// returns even when no report artifact is committed.
    pub emergency_snapshot: Option<EmergencySnapshot>,
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
            emergency_snapshot: None,
            report_path: None,
            start: Instant::now(),
        }
    }

    pub fn record(&mut self, name: &str, status: PluginStatus, duration: Duration) {
        #[allow(clippy::cast_possible_truncation)]
        let duration_ms = duration.as_millis() as u64;
        #[allow(clippy::cast_possible_truncation)]
        let finished_offset_ms = self.start.elapsed().as_millis() as u64;
        self.plugins.push(PluginDiagnostic {
            name: name.to_string(),
            status,
            duration_ms,
            report_id: self
                .emergency_snapshot
                .as_ref()
                .map(|snapshot| snapshot.report_id.clone()),
            started_offset_ms: finished_offset_ms.saturating_sub(duration_ms),
            finished_offset_ms,
        });
    }

    pub fn record_immediate(&mut self, name: &str, status: PluginStatus) {
        self.record(name, status, Duration::ZERO);
    }

    pub fn ensure_emergency_snapshot(
        &mut self,
        event: &CrashEvent,
        raw_shm: Option<&RawShmSnapshot>,
    ) {
        if self.emergency_snapshot.is_none() {
            self.emergency_snapshot = Some(EmergencySnapshot::new(event, raw_shm));
        }
    }

    #[must_use]
    pub fn succeeded(&self, name: &str) -> bool {
        self.plugins.iter().any(|d| {
            d.name == name && matches!(d.status, PluginStatus::Ok | PluginStatus::Partial(_))
        })
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
    /// Exact artifact draft for callers that do not run inside an
    /// [`ArtifactTransaction`](super::ArtifactTransaction). Production
    /// finalization mirrors this set in the transaction registry.
    pub artifact_paths: Vec<PathBuf>,
    pub raw_path: Option<PathBuf>,
    pub json_path: Option<PathBuf>,
    pub session: Option<SessionReport>,
}

// ═══════════════════════════════════════════════════
//  Capture / finalize handoff
// ═══════════════════════════════════════════════════

/// Raw shared-memory evidence copied while the target is still suspended.
///
/// The finalizer receives owned bytes rather than a live shared-memory handle,
/// so it cannot observe post-resume mutations.
pub struct RawShmSnapshot {
    pub breadcrumbs: Vec<u8>,
    pub context: Vec<u8>,
}

/// Owned output of the task-facing capture worker.
pub struct CapturePayload {
    pub data: CollectedData,
    pub raw_shm: Option<RawShmSnapshot>,
    pub diagnostics: Diagnostics,
}

/// Immutable handoff from live capture to report finalization.
///
/// Live Mach capabilities are stripped in [`Self::new`]. Thread-port numbers
/// may remain in raw diagnostic data, but their send rights are released before
/// this value leaves the capture worker.
pub struct CapturedEvent {
    pub(crate) event: CrashEvent,
    pub(crate) report_context: Option<std::sync::Arc<super::ReportContext>>,
    pub(crate) data: Box<CollectedData>,
    pub(crate) raw_shm: Option<RawShmSnapshot>,
    /// Immutable SHM bytes for collectors that run only after resume/reply.
    pub(crate) owned_shm_snapshot: Option<std::sync::Arc<crate::shm::OwnedShmSnapshot>>,
    /// Distinguishes deferred collection without SHM from ordinary capture
    /// paths that already ran every collector in-process.
    pub(crate) owned_collectors_deferred: bool,
    pub(crate) diagnostics: Diagnostics,
}

impl CapturedEvent {
    #[must_use]
    pub fn new(mut event: CrashEvent, mut payload: CapturePayload) -> Self {
        payload
            .diagnostics
            .ensure_emergency_snapshot(&event, payload.raw_shm.as_ref());
        event.crashed_thread = None;
        event.bail_on_suspend_failure = false;
        Self {
            event,
            report_context: None,
            data: Box::new(payload.data),
            raw_shm: payload.raw_shm,
            owned_shm_snapshot: None,
            owned_collectors_deferred: false,
            diagnostics: payload.diagnostics,
        }
    }

    #[must_use]
    pub(crate) fn with_report_context(
        mut event: CrashEvent,
        report_context: std::sync::Arc<super::ReportContext>,
        mut payload: CapturePayload,
    ) -> Self {
        payload
            .diagnostics
            .ensure_emergency_snapshot(&event, payload.raw_shm.as_ref());
        event.crashed_thread = None;
        event.bail_on_suspend_failure = false;
        Self {
            event,
            report_context: Some(report_context),
            data: Box::new(payload.data),
            raw_shm: payload.raw_shm,
            owned_shm_snapshot: None,
            owned_collectors_deferred: false,
            diagnostics: payload.diagnostics,
        }
    }

    pub fn set_termination(&mut self, reason: Option<TerminationReason>) {
        self.event.termination = reason;
    }

    pub(crate) fn attach_owned_shm_snapshot(
        &mut self,
        snapshot: Option<std::sync::Arc<crate::shm::OwnedShmSnapshot>>,
    ) {
        self.owned_shm_snapshot = snapshot;
        self.owned_collectors_deferred = true;
    }
}

/// Result of the task-facing capture boundary.
pub enum CaptureOutcome {
    Captured(Box<CapturedEvent>),
    /// Capture was intentionally skipped (for example, a mandatory suspend
    /// failed). Mach reply still proceeds for crash events.
    Skipped(Diagnostics),
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
