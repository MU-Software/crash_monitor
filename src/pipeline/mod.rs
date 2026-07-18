//! Plugin pipeline architecture for crash/snapshot report generation.
//!
//! Design: `docs/plans/crash_reporter.md` L1493-1867

pub mod artifact;
pub mod capture_isolation;
pub mod report;
pub mod safety;
pub mod traits;
pub mod types;
pub mod worker;

use mach2::port::mach_port_t;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::platform::{
    PlatformOps, SuspendFailurePolicy, TaskControlFailureSink, TaskSuspendGuard,
};

pub use crate::config::CollectionPolicy;
pub use artifact::{
    ArtifactKind, ArtifactTransaction, CommittedReport, ReportContext, ReportId, ReportManifest,
    load_manifest, recover_prepared_reports, scavenge_stale_pending,
};
pub use safety::{
    CancellationToken, PluginContext, PluginRunResult, SubprocessOutput,
    run_plugin_catching_panics, run_plugin_cooperative, run_plugin_subprocess,
};
pub use traits::{
    Collector, CollectorAccess, Filter, Notifier, Plugin, PluginExecution, PostProcessor,
    PostProcessorPhase, PreProcessor,
};
pub use types::{
    CaptureOutcome, CapturePayload, CapturedEvent, CollectedData, CrashEvent, DependencyKind,
    Diagnostics, PluginCategory, PluginDependency, PluginDiagnostic, PluginId, PluginStatus,
    PluginTimeout, Priority, RawShmSnapshot, ReportResult, ReportType, TerminationReason,
};

/// Immutable per-trigger report policy installed in a [`Pipeline`].
///
/// The global kill switch is deliberately stored separately on `Pipeline`.
/// This policy distinguishes all report-producing event sources without
/// granting any of them an implicit emergency-evidence exception.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // one explicit switch per external trigger
pub struct TriggerPolicy {
    pub crash: bool,
    pub exit_failure: bool,
    pub signal_failure: bool,
    pub probable_oom: bool,
    pub anr: bool,
    pub snapshot: bool,
}

impl TriggerPolicy {
    /// Policy used by tests and callers that want the historical all-on mode.
    pub const ALL_ENABLED: Self = Self {
        crash: true,
        exit_failure: true,
        signal_failure: true,
        probable_oom: true,
        anr: true,
        snapshot: true,
    };

    #[must_use]
    const fn allows(self, report_type: ReportType) -> bool {
        match report_type {
            ReportType::Crash => self.crash,
            ReportType::Snapshot => self.snapshot,
            ReportType::Anr => self.anr,
            ReportType::Oom => self.probable_oom,
            ReportType::ExitFailure => self.exit_failure,
            ReportType::SignalFailure => self.signal_failure,
        }
    }
}

impl Default for TriggerPolicy {
    fn default() -> Self {
        Self::ALL_ENABLED
    }
}

impl From<crate::config::ValidatedTriggersConfig> for TriggerPolicy {
    fn from(config: crate::config::ValidatedTriggersConfig) -> Self {
        Self {
            crash: config.crash,
            exit_failure: config.exit_failure,
            signal_failure: config.signal_failure,
            probable_oom: config.probable_oom,
            anr: config.anr,
            snapshot: config.snapshot,
        }
    }
}

// ═══════════════════════════════════════════════════
//  Pipeline
// ═══════════════════════════════════════════════════

pub struct Pipeline {
    /// Authoritative process-wide report-generation kill switch.
    pub enabled: bool,
    /// Explicit policy for each event source that can create a report.
    pub triggers: TriggerPolicy,
    /// Immutable privacy decisions shared by capture and finalization.
    pub collection_policy: CollectionPolicy,
    pub filters: Vec<Box<dyn Filter>>,
    pub collectors: Vec<Box<dyn Collector>>,
    pub pre_processors: Vec<Box<dyn PreProcessor>>,
    pub post_processors: Vec<Box<dyn PostProcessor>>,
    #[allow(dead_code)] // Phase 4+
    pub notifiers: Vec<Box<dyn Notifier>>,
    /// Live shared-memory mapping used only to create an owned snapshot while
    /// this monitor owns the task suspension (None if shm is unavailable).
    pub shm: Option<std::sync::Arc<crate::shm::SharedMemory>>,
    /// Platform abstraction for suspend/resume and port deallocation.
    pub platform: Arc<dyn PlatformOps>,
    /// Override for report output directory. If None, uses default `pending_dir()`.
    pub output_dir: Option<PathBuf>,
}

// Category-specific cooperative deadlines.
const FILTER_TIMEOUT: Duration = Duration::from_secs(1);
const COLLECTOR_TIMEOUT: Duration = Duration::from_secs(5);
const PREPROC_TIMEOUT: Duration = Duration::from_secs(2);
const POSTPROC_TIMEOUT: Duration = Duration::from_secs(30);
const NOTIFIER_TIMEOUT: Duration = Duration::from_secs(5);
const STAGE_TIMEOUT: Option<Duration> = Some(Duration::from_secs(5));

#[cfg(test)]
fn run_stage<T>(
    name: &str,
    execution: PluginExecution,
    timeout: Option<Duration>,
    f: impl FnOnce(&PluginContext) -> Result<T, String>,
) -> PluginRunResult<T> {
    let context = PluginContext::from_timeout(timeout);
    enforce_execution_boundary(
        name,
        execution,
        &context,
        run_plugin_cooperative(name, &context, f),
    )
}

fn run_transaction_stage<T>(
    transaction: &Arc<ArtifactTransaction>,
    name: &str,
    execution: PluginExecution,
    timeout: Option<Duration>,
    f: impl FnOnce(&PluginContext) -> Result<T, String>,
) -> PluginRunResult<T> {
    let context =
        PluginContext::from_timeout(timeout).with_artifact_transaction(transaction.clone());
    enforce_execution_boundary(
        name,
        execution,
        &context,
        run_plugin_cooperative(name, &context, f),
    )
}

fn run_cancellable_stage<T>(
    name: &str,
    execution: PluginExecution,
    timeout: Option<Duration>,
    cancellation: CancellationToken,
    shm_snapshot: Option<Arc<crate::shm::OwnedShmSnapshot>>,
    report_context: Arc<ReportContext>,
    f: impl FnOnce(&PluginContext) -> Result<T, String>,
) -> PluginRunResult<T> {
    let context = PluginContext::from_timeout_and_cancellation(timeout, cancellation)
        .with_shm_snapshot(shm_snapshot)
        .with_report_context(report_context);
    enforce_execution_boundary(
        name,
        execution,
        &context,
        run_plugin_cooperative(name, &context, f),
    )
}

fn enforce_execution_boundary<T>(
    name: &str,
    execution: PluginExecution,
    context: &PluginContext,
    result: PluginRunResult<T>,
) -> PluginRunResult<T> {
    if execution == PluginExecution::Subprocess
        && !context.subprocess_boundary_satisfied()
        && matches!(
            result,
            PluginRunResult::Completed(_) | PluginRunResult::Failed(_)
        )
    {
        let error =
            format!("plugin {name} declared Subprocess but did not use the subprocess supervisor");
        eprintln!("[monitor] {error}");
        PluginRunResult::Failed(error)
    } else {
        result
    }
}

fn plugin_status<T>(result: &PluginRunResult<T>) -> PluginStatus {
    match result {
        PluginRunResult::Completed(_) => PluginStatus::Ok,
        PluginRunResult::Failed(error) => PluginStatus::Error(error.clone()),
        PluginRunResult::Panicked(message) => PluginStatus::Panic(message.clone()),
        PluginRunResult::TimedOut => PluginStatus::TimedOut,
    }
}

fn filter_status(name: &str, result: &PluginRunResult<bool>) -> PluginStatus {
    match result {
        PluginRunResult::Completed(true) => PluginStatus::Ok,
        PluginRunResult::Completed(false) => {
            PluginStatus::Rejected(format!("{name} rejected the event"))
        }
        PluginRunResult::Failed(error) => PluginStatus::Error(error.clone()),
        PluginRunResult::Panicked(message) => PluginStatus::Panic(message.clone()),
        PluginRunResult::TimedOut => PluginStatus::TimedOut,
    }
}

/// Stable capture contract for a failed suspend. Snapshot-like triggers need
/// a coherent view and skip capture; fatal crashes retain best-effort evidence
/// while owning no suspension count.
fn suspend_failure_policy(event: &CrashEvent) -> SuspendFailurePolicy {
    if event.bail_on_suspend_failure {
        SuspendFailurePolicy::SkipCapture
    } else {
        SuspendFailurePolicy::BestEffort
    }
}

impl Pipeline {
    /// Finalize plugin registration by applying deterministic execution order
    /// and rejecting an ambiguous runtime registry.
    ///
    /// Priority is used only among plugins whose registered dependencies have
    /// already been scheduled. Equal-priority plugins retain insertion order.
    ///
    /// # Errors
    /// Returns a structured validation error for duplicate IDs or an invalid
    /// dependency graph.
    pub fn finish_registration(mut self) -> Result<Self, crate::config::ConfigValidationError> {
        stable_plugin_order(&mut self.filters, |_| 0);
        stable_plugin_order(&mut self.collectors, |_| 0);
        stable_plugin_order(&mut self.pre_processors, |_| 0);
        stable_plugin_order(&mut self.post_processors, |plugin| {
            post_processor_phase_rank(plugin.phase())
        });
        stable_plugin_order(&mut self.notifiers, |_| 0);
        self.validate_dependencies()?;
        Ok(self)
    }

    fn resolved_output_root(&self) -> Result<PathBuf, String> {
        self.output_dir
            .clone()
            .map_or_else(crate::utils::paths::pending_dir_path, Ok)
    }

    /// Recover manifest-complete transactions once during monitor startup.
    /// This must not run from per-event finalizers because doing so could race
    /// another live transaction between manifest prepare and directory publish.
    ///
    /// # Errors
    /// Returns an error if the output root cannot be resolved or safely
    /// scanned for prepared transactions.
    pub fn recover_prepared_artifacts(&self) -> Result<usize, String> {
        if !self.enabled {
            return Ok(0);
        }
        let output_root = self.resolved_output_root()?;
        recover_prepared_reports(&output_root)
    }

    /// Recover manifest-complete transactions, then delete only old,
    /// unlocked transactions that never reached the manifest commit marker.
    /// Raw and temporary artifacts are transaction-local, so this one policy
    /// covers their orphaned forms without matching arbitrary user files.
    ///
    /// # Errors
    /// Returns an error when recovery or the bounded stale scan cannot safely
    /// complete. A live owner lock always leaves its transaction untouched.
    pub fn recover_startup_artifacts(&self) -> Result<StartupRecovery, String> {
        if !self.enabled {
            return Ok(StartupRecovery::default());
        }
        let output_root = self.resolved_output_root()?;
        let recovered = recover_prepared_reports(&output_root)?;
        let scavenged = scavenge_stale_pending(&output_root, STARTUP_STALE_ARTIFACT_AGE)?;
        Ok(StartupRecovery {
            recovered,
            scavenged,
        })
    }

    pub(super) fn create_report_context(
        &self,
        event: &CrashEvent,
    ) -> Result<Arc<ReportContext>, String> {
        Ok(Arc::new(ReportContext::new(
            event,
            &self.resolved_output_root()?,
        )))
    }

    /// Return whether this pipeline may process the given report type.
    ///
    /// The global switch is always checked first and has no exception path.
    #[must_use]
    pub fn report_enabled(&self, report_type: ReportType) -> bool {
        self.enabled && self.triggers.allows(report_type)
    }

    /// Copy the live mapping into an immutable payload while the caller owns
    /// task suspension. Callers must skip this method when suspension failed.
    pub(super) fn snapshot_shm_while_suspended(
        &self,
        deadline: Option<Instant>,
    ) -> Result<Option<Arc<crate::shm::OwnedShmSnapshot>>, String> {
        let has_collector = |name: &str| {
            self.collectors
                .iter()
                .any(|collector| collector.name() == name)
        };
        let raw_shm = self.collection_policy.persist_raw_shm;
        let snapshot_policy = crate::shm::ShmSnapshotPolicy {
            breadcrumbs: raw_shm || has_collector("BreadcrumbCollector"),
            context: raw_shm || has_collector("ContextCollector"),
            attachments: self.collection_policy.capture_shm_attachments
                && has_collector("AttachmentCollector"),
            screenshots: self.collection_policy.capture_shm_screenshots
                && has_collector("ScreenshotCollector"),
        };
        self.shm
            .as_ref()
            .map(|shm| {
                shm.snapshot_owned_until_with_policy(deadline, snapshot_policy)
                    .map(Arc::new)
                    .map_err(|error| format!("shared-memory snapshot failed: {error}"))
            })
            .transpose()
    }

    /// Describe a successful snapshot whose publication checks rejected one
    /// or more units. The snapshot remains usable because its reader has
    /// already sanitized only the affected byte ranges.
    pub(super) fn snapshot_consistency_error(
        snapshot: Option<&crate::shm::OwnedShmSnapshot>,
    ) -> Option<String> {
        let issues = snapshot?.consistency_issues();
        let first = issues.first()?;
        Some(format!(
            "shared-memory snapshot contained {} unstable publication unit(s); affected bytes were sanitized; first issue: {first:?}",
            issues.len()
        ))
    }

    /// Process a crash/snapshot event synchronously.
    ///
    /// Production Mach events use [`worker`] so finalization cannot delay
    /// target resume or the exception reply. This wrapper remains useful for
    /// callers and unit tests that explicitly want synchronous completion.
    #[must_use]
    pub fn handle_event(&self, event: &CrashEvent, task: mach_port_t) -> Diagnostics {
        if !self.report_enabled(event.report_type) {
            return Diagnostics::new();
        }
        match self.capture_event(event, task) {
            CaptureOutcome::Captured(captured) => self.finalize_captured(*captured),
            CaptureOutcome::Skipped(diagnostics) => diagnostics,
        }
    }

    /// Suspend, collect task-facing state, release thread rights, and resume.
    fn capture_event(&self, event: &CrashEvent, task: mach_port_t) -> CaptureOutcome {
        if !self.report_enabled(event.report_type) {
            return CaptureOutcome::Skipped(Diagnostics::new());
        }
        let report_context = match self.create_report_context(event) {
            Ok(context) => context,
            Err(error) => {
                let mut diagnostics = Diagnostics::new();
                diagnostics.record_immediate("ReportContext", PluginStatus::Error(error));
                return CaptureOutcome::Skipped(diagnostics);
            }
        };
        let mut diagnostics = Diagnostics::new();
        diagnostics.ensure_emergency_snapshot(event, None);
        let failure_sink = TaskControlFailureSink::new();
        let suspend_guard = match TaskSuspendGuard::acquire(
            self.platform.clone(),
            task,
            failure_sink.clone(),
        ) {
            Ok(guard) => Some(guard),
            Err(error) => {
                diagnostics.record_immediate("CaptureSuspend", PluginStatus::Error(error.clone()));
                match suspend_failure_policy(event) {
                    SuspendFailurePolicy::SkipCapture => {
                        eprintln!("[monitor] suspend_task failed; capture skipped: {error}");
                        return CaptureOutcome::Skipped(diagnostics);
                    }
                    SuspendFailurePolicy::BestEffort => {
                        eprintln!(
                            "[monitor] suspend_task failed (proceeding with best-effort): {error}"
                        );
                        None
                    }
                }
            }
        };

        let shm_snapshot = if suspend_guard.is_some() {
            match self.snapshot_shm_while_suspended(None) {
                Ok(snapshot) => {
                    if let Some(error) = Self::snapshot_consistency_error(snapshot.as_deref()) {
                        diagnostics.record_immediate("ShmSnapshot", PluginStatus::Error(error));
                    }
                    snapshot
                }
                Err(error) => {
                    diagnostics.record_immediate("ShmSnapshot", PluginStatus::Error(error));
                    None
                }
            }
        } else {
            None
        };

        let cancelled = Arc::new(AtomicBool::new(false));
        let mut payload = self.collect_snapshot(
            event,
            task,
            &cancelled,
            shm_snapshot.as_ref(),
            &report_context,
        );

        if let Some(guard) = suspend_guard {
            guard.finish();
        }
        failure_sink.drain_into(&mut payload.diagnostics);

        diagnostics.plugins.append(&mut payload.diagnostics.plugins);
        CaptureOutcome::Captured(Box::new(CapturedEvent::with_report_context(
            event.clone(),
            report_context,
            CapturePayload {
                data: payload.data,
                raw_shm: payload.raw_shm,
                diagnostics,
            },
        )))
    }

    /// Collect only data that requires access to the live task.
    ///
    /// The caller owns suspension and resume. Worker callers pass the shared
    /// absolute-deadline cancellation flag into every cooperative collector.
    fn collect_snapshot(
        &self,
        event: &CrashEvent,
        task: mach_port_t,
        cancelled: &Arc<AtomicBool>,
        shm_snapshot: Option<&Arc<crate::shm::OwnedShmSnapshot>>,
        report_context: &Arc<ReportContext>,
    ) -> CapturePayload {
        self.collect_snapshot_by_access(event, task, cancelled, shm_snapshot, report_context, None)
    }

    fn collect_snapshot_by_access(
        &self,
        event: &CrashEvent,
        task: mach_port_t,
        cancelled: &Arc<AtomicBool>,
        shm_snapshot: Option<&Arc<crate::shm::OwnedShmSnapshot>>,
        report_context: &Arc<ReportContext>,
        access: Option<CollectorAccess>,
    ) -> CapturePayload {
        if !self.report_enabled(event.report_type) {
            return CapturePayload {
                data: CollectedData::default(),
                raw_shm: None,
                diagnostics: Diagnostics::new(),
            };
        }
        let mut diagnostics = Diagnostics::new();

        // ── Collectors ──
        let mut data = CollectedData::default();
        for c in &self.collectors {
            if access.is_some_and(|access| c.access() != access) {
                continue;
            }
            if cancelled.load(Ordering::Acquire) {
                diagnostics.record_immediate("CaptureDeadline", PluginStatus::TimedOut);
                break;
            }
            if !c.is_available() {
                diagnostics
                    .record_immediate(c.name(), PluginStatus::Skipped("not available".into()));
                continue;
            }
            if !deps_satisfied(c.hard_dependencies(), &diagnostics) {
                diagnostics
                    .record_immediate(c.name(), PluginStatus::Skipped("dependency not met".into()));
                continue;
            }
            let start = Instant::now();
            let timeout = c.timeout().resolve(COLLECTOR_TIMEOUT);
            let cancellation = CancellationToken::from_atomic(cancelled.clone());
            let outcome = run_cancellable_stage(
                c.name(),
                c.execution(),
                timeout,
                cancellation,
                shm_snapshot.cloned(),
                report_context.clone(),
                |context| c.collect(event, task, &mut data, context),
            );
            let status = plugin_status(&outcome);
            diagnostics.record(c.name(), status, start.elapsed());
        }

        let raw_shm =
            if cancelled.load(Ordering::Acquire) || !self.collection_policy.persist_raw_shm {
                None
            } else {
                shm_snapshot.map(|snapshot| RawShmSnapshot {
                    breadcrumbs: snapshot.raw_breadcrumb_bytes_owned(),
                    context: snapshot.raw_context_bytes_owned(),
                })
            };

        // Thread rights cannot cross the immutable capture/finalize boundary.
        let thread_ports: Vec<u32> = data.raw.threads.iter().map(|t| t.thread_port).collect();
        drop(safety::PortGuard::new(thread_ports, self.platform.clone()));

        CapturePayload {
            data,
            raw_shm,
            diagnostics,
        }
    }

    pub(super) fn collect_snapshot_for_worker(
        &self,
        event: &CrashEvent,
        task: mach_port_t,
        cancelled: &Arc<AtomicBool>,
        shm_snapshot: Option<&Arc<crate::shm::OwnedShmSnapshot>>,
        report_context: &Arc<ReportContext>,
    ) -> CapturePayload {
        self.collect_snapshot(event, task, cancelled, shm_snapshot, report_context)
    }

    pub(super) fn collect_owned_snapshot_for_worker(
        &self,
        event: &CrashEvent,
        cancelled: &Arc<AtomicBool>,
        shm_snapshot: Option<&Arc<crate::shm::OwnedShmSnapshot>>,
        report_context: &Arc<ReportContext>,
    ) -> CapturePayload {
        self.collect_snapshot_by_access(
            event,
            mach2::port::MACH_PORT_NULL,
            cancelled,
            shm_snapshot,
            report_context,
            Some(CollectorAccess::OwnedSnapshot),
        )
    }

    pub(super) fn finalize_captured_for_worker(&self, captured: CapturedEvent) -> Diagnostics {
        self.finalize_captured(captured)
    }

    /// Finalize owned capture data without a task port or live SHM view.
    #[allow(clippy::too_many_lines)]
    fn finalize_captured(&self, mut captured: CapturedEvent) -> Diagnostics {
        if !self.report_enabled(captured.event.report_type) {
            return Diagnostics::new();
        }
        let report_context = captured.report_context.take();
        let owned_shm_snapshot = captured.owned_shm_snapshot.take();
        let owned_collectors_deferred = captured.owned_collectors_deferred;
        let event = &captured.event;
        let data = &mut captured.data;
        let diagnostics = &mut captured.diagnostics;

        let report_context = if let Some(report_context) = report_context {
            report_context
        } else {
            let pending = match &self.output_dir {
                Some(dir) => dir.clone(),
                None => match crate::utils::paths::pending_dir() {
                    Ok(dir) => dir,
                    Err(error) => {
                        eprintln!("[monitor] Failed to get pending dir: {error}");
                        return std::mem::take(diagnostics);
                    }
                },
            };
            Arc::new(ReportContext::new(event, &pending))
        };
        if owned_collectors_deferred {
            let cancelled = Arc::new(AtomicBool::new(false));
            let mut owned = self.collect_owned_snapshot_for_worker(
                event,
                &cancelled,
                owned_shm_snapshot.as_ref(),
                &report_context,
            );
            merge_owned_collected_data(data, &mut owned.data);
            diagnostics.plugins.append(&mut owned.diagnostics.plugins);
        }
        let transaction = match ArtifactTransaction::begin_shared(report_context) {
            Ok(transaction) => transaction,
            Err(error) => {
                diagnostics.record_immediate("ArtifactBegin", PluginStatus::Error(error));
                return std::mem::take(diagnostics);
            }
        };

        // Stage 1 depends on collector-owned thread data, but not on any
        // preprocessor. Write it before preprocessor/duplicate short-circuits;
        // ArtifactTransaction::drop removes it on every uncommitted return.
        let raw_path: Option<PathBuf> = run_transaction_stage(
            &transaction,
            "Stage1Raw",
            PluginExecution::Cooperative,
            STAGE_TIMEOUT,
            |_| safety::write_raw_stage1(&transaction, &data.raw.threads),
        )
        .into_option();

        if let Some(raw_shm) = &captured.raw_shm {
            let _ = run_transaction_stage(
                &transaction,
                "Stage1Shm",
                PluginExecution::Cooperative,
                STAGE_TIMEOUT,
                |_| safety::write_raw_shm_stage1(&transaction, raw_shm),
            );
        }

        // Filters run after resume so filesystem and lock contention cannot
        // extend the Mach critical section.
        for filter in &self.filters {
            if !filter.is_available() {
                diagnostics
                    .record_immediate(filter.name(), PluginStatus::Skipped("not available".into()));
                continue;
            }
            if !deps_satisfied(filter.hard_dependencies(), diagnostics) {
                diagnostics.record_immediate(
                    filter.name(),
                    PluginStatus::Skipped("hard dependency not met".into()),
                );
                continue;
            }
            let start = Instant::now();
            let timeout = filter.timeout().resolve(FILTER_TIMEOUT);
            let outcome = run_transaction_stage(
                &transaction,
                filter.name(),
                filter.execution(),
                timeout,
                |context| filter.should_process(event, context),
            );
            let status = filter_status(filter.name(), &outcome);
            let pass = outcome.into_option().unwrap_or(true);
            diagnostics.record(filter.name(), status, start.elapsed());
            if !pass {
                return std::mem::take(diagnostics);
            }
        }

        // ── Pre-processors ──
        for pp in &self.pre_processors {
            // AttachmentCollector records bounded metadata during capture.
            // Copy only after duplicate policy has decided to publish.
            if pp.name() == "AttachmentCopier" {
                continue;
            }
            if !pp.is_available() {
                diagnostics
                    .record_immediate(pp.name(), PluginStatus::Skipped("not available".into()));
                continue;
            }
            if !deps_satisfied(pp.hard_dependencies(), diagnostics) {
                diagnostics.record_immediate(
                    pp.name(),
                    PluginStatus::Skipped("dependency not met".into()),
                );
                continue;
            }
            let start = Instant::now();
            let timeout = pp.timeout().resolve(PREPROC_TIMEOUT);
            let outcome = run_transaction_stage(
                &transaction,
                pp.name(),
                pp.execution(),
                timeout,
                |context| pp.process(event, data, context),
            );
            let status = plugin_status(&outcome);
            diagnostics.record(pp.name(), status, start.elapsed());
        }

        // ── Duplicate check (set by DuplicateDetector pre-processor) ──
        if data.duplicate_detected {
            eprintln!("[monitor] Duplicate event detected, skipping report");
            return std::mem::take(diagnostics);
        }

        for pp in self
            .pre_processors
            .iter()
            .filter(|processor| processor.name() == "AttachmentCopier")
        {
            let start = Instant::now();
            let timeout = pp.timeout().resolve(PREPROC_TIMEOUT);
            let outcome = run_transaction_stage(
                &transaction,
                pp.name(),
                pp.execution(),
                timeout,
                |context| pp.process(event, data, context),
            );
            diagnostics.record(pp.name(), plugin_status(&outcome), start.elapsed());
        }

        // ── Stage 2: Full JSON report + screenshot PNGs ──
        let screenshots = std::mem::take(&mut data.raw.screenshots);
        let json_path: Option<PathBuf> = run_transaction_stage(
            &transaction,
            "Stage2Json",
            PluginExecution::Cooperative,
            STAGE_TIMEOUT,
            |_| {
                let mut crash_report = report::build_report(event, data, diagnostics);
                report::write_report(&transaction, &mut crash_report, &screenshots)
            },
        )
        .into_option();

        let mut result = ReportResult {
            artifact_paths: transaction.artifact_paths(),
            raw_path,
            json_path,
            session: data.session.clone(),
        };

        // ── Post-processors ──
        for pp in &self.post_processors {
            if pp.phase() != PostProcessorPhase::BeforeCommit {
                continue;
            }
            if !pp.is_available() {
                diagnostics
                    .record_immediate(pp.name(), PluginStatus::Skipped("not available".into()));
                continue;
            }
            if !deps_satisfied(pp.hard_dependencies(), diagnostics) {
                diagnostics.record_immediate(
                    pp.name(),
                    PluginStatus::Skipped("hard dependency not met".into()),
                );
                continue;
            }
            let start = Instant::now();
            let timeout = pp.timeout().resolve(POSTPROC_TIMEOUT);
            let outcome = run_transaction_stage(
                &transaction,
                pp.name(),
                pp.execution(),
                timeout,
                |context| pp.process(event, &mut result, context),
            );
            let status = plugin_status(&outcome);
            diagnostics.record(pp.name(), status, start.elapsed());
        }

        let committed = match transaction.commit() {
            Ok(committed) => committed,
            Err(error) => {
                diagnostics.record_immediate("ArtifactCommit", PluginStatus::Error(error));
                return std::mem::take(diagnostics);
            }
        };
        for warning in &committed.durability_warnings {
            diagnostics
                .record_immediate("ArtifactDurability", PluginStatus::Error(warning.clone()));
        }
        result.raw_path = result.raw_path.as_deref().and_then(|path| {
            remap_committed_path(path, transaction.staging_dir(), &committed.report_dir)
        });
        result.json_path = result.json_path.as_deref().and_then(|path| {
            remap_committed_path(path, transaction.staging_dir(), &committed.report_dir)
        });
        result.artifact_paths = result
            .artifact_paths
            .iter()
            .filter_map(|path| {
                remap_committed_path(path, transaction.staging_dir(), &committed.report_dir)
            })
            .collect();

        for pp in &self.post_processors {
            if pp.phase() != PostProcessorPhase::AfterCommit {
                continue;
            }
            if !pp.is_available() {
                diagnostics
                    .record_immediate(pp.name(), PluginStatus::Skipped("not available".into()));
                continue;
            }
            if !deps_satisfied(pp.hard_dependencies(), diagnostics) {
                diagnostics.record_immediate(
                    pp.name(),
                    PluginStatus::Skipped("hard dependency not met".into()),
                );
                continue;
            }
            let start = Instant::now();
            let timeout = pp.timeout().resolve(POSTPROC_TIMEOUT);
            let outcome = run_transaction_stage(
                &transaction,
                pp.name(),
                pp.execution(),
                timeout,
                |context| pp.process(event, &mut result, context),
            );
            diagnostics.record(pp.name(), plugin_status(&outcome), start.elapsed());
        }

        // ── Notifiers (fire-and-forget) ──
        if let Some(ref path) = result.json_path {
            for n in &self.notifiers {
                if !n.is_available() {
                    diagnostics
                        .record_immediate(n.name(), PluginStatus::Skipped("not available".into()));
                    continue;
                }
                if !deps_satisfied(n.hard_dependencies(), diagnostics) {
                    diagnostics.record_immediate(
                        n.name(),
                        PluginStatus::Skipped("hard dependency not met".into()),
                    );
                    continue;
                }
                let start = Instant::now();
                let timeout = n.timeout().resolve(NOTIFIER_TIMEOUT);
                let outcome = run_transaction_stage(
                    &transaction,
                    n.name(),
                    n.execution(),
                    timeout,
                    |context| n.notify(path, context),
                );
                let status = plugin_status(&outcome);
                diagnostics.record(n.name(), status, start.elapsed());
            }
        } else {
            for notifier in &self.notifiers {
                diagnostics.record_immediate(
                    notifier.name(),
                    PluginStatus::Skipped("no final report artifact".into()),
                );
            }
        }

        for pp in &self.post_processors {
            if pp.phase() != PostProcessorPhase::AfterNotify {
                continue;
            }
            if !pp.is_available() {
                diagnostics
                    .record_immediate(pp.name(), PluginStatus::Skipped("not available".into()));
                continue;
            }
            if !deps_satisfied(pp.hard_dependencies(), diagnostics) {
                diagnostics.record_immediate(
                    pp.name(),
                    PluginStatus::Skipped("hard dependency not met".into()),
                );
                continue;
            }
            let start = Instant::now();
            let timeout = pp.timeout().resolve(POSTPROC_TIMEOUT);
            let outcome = run_transaction_stage(
                &transaction,
                pp.name(),
                pp.execution(),
                timeout,
                |context| pp.process(event, &mut result, context),
            );
            diagnostics.record(pp.name(), plugin_status(&outcome), start.elapsed());
        }

        for pp in &self.post_processors {
            if pp.phase() != PostProcessorPhase::FinalCleanup {
                continue;
            }
            if !pp.is_available() {
                diagnostics
                    .record_immediate(pp.name(), PluginStatus::Skipped("not available".into()));
                continue;
            }
            if !deps_satisfied(pp.hard_dependencies(), diagnostics) {
                diagnostics.record_immediate(
                    pp.name(),
                    PluginStatus::Skipped("hard dependency not met".into()),
                );
                continue;
            }
            let start = Instant::now();
            let timeout = pp.timeout().resolve(POSTPROC_TIMEOUT);
            let outcome = run_transaction_stage(
                &transaction,
                pp.name(),
                pp.execution(),
                timeout,
                |context| pp.process(event, &mut result, context),
            );
            diagnostics.record(pp.name(), plugin_status(&outcome), start.elapsed());
        }
        persist_final_diagnostics(&committed, diagnostics);
        transaction.release_publication_lease();

        diagnostics.report_path = result.json_path;

        std::mem::take(diagnostics)
    }

    /// Write and finalize a report for a child that has already terminated.
    ///
    /// Task suspension and task-port collectors are intentionally skipped: a
    /// dead task is not introspectable, and attempting VM enumeration against
    /// an invalid task can prevent the termination report from ever being
    /// finalized. The immutable `TerminationReason` remains the authoritative
    /// payload for this path.
    #[must_use]
    pub fn handle_termination_event(&self, event: &CrashEvent) -> Diagnostics {
        if !self.report_enabled(event.report_type) {
            return Diagnostics::new();
        }
        self.finalize_termination_event(event)
    }

    pub(super) fn finalize_termination_event_for_worker(&self, event: &CrashEvent) -> Diagnostics {
        self.finalize_termination_event(event)
    }

    #[allow(clippy::too_many_lines)]
    fn finalize_termination_event(&self, event: &CrashEvent) -> Diagnostics {
        debug_assert!(event.termination.is_some());
        if !self.report_enabled(event.report_type) {
            return Diagnostics::new();
        }
        let mut diagnostics = Diagnostics::new();

        let pending = match &self.output_dir {
            Some(dir) => dir.clone(),
            None => match crate::utils::paths::pending_dir() {
                Ok(dir) => dir,
                Err(e) => {
                    eprintln!("[monitor] Failed to get pending dir: {e}");
                    return diagnostics;
                }
            },
        };
        let transaction = match ArtifactTransaction::begin(ReportContext::new(event, &pending)) {
            Ok(transaction) => transaction,
            Err(error) => {
                diagnostics.record_immediate("ArtifactBegin", PluginStatus::Error(error));
                return diagnostics;
            }
        };

        for filter in &self.filters {
            if !filter.is_available() {
                diagnostics
                    .record_immediate(filter.name(), PluginStatus::Skipped("not available".into()));
                continue;
            }
            if !deps_satisfied(filter.hard_dependencies(), &diagnostics) {
                diagnostics.record_immediate(
                    filter.name(),
                    PluginStatus::Skipped("hard dependency not met".into()),
                );
                continue;
            }
            let start = Instant::now();
            let timeout = filter.timeout().resolve(FILTER_TIMEOUT);
            let outcome = run_transaction_stage(
                &transaction,
                filter.name(),
                filter.execution(),
                timeout,
                |context| filter.should_process(event, context),
            );
            let status = filter_status(filter.name(), &outcome);
            let pass = outcome.into_option().unwrap_or(true);
            diagnostics.record(filter.name(), status, start.elapsed());
            if !pass {
                return diagnostics;
            }
        }

        for collector in &self.collectors {
            diagnostics.record_immediate(
                collector.name(),
                PluginStatus::Skipped("child already terminated".into()),
            );
        }
        for pre_processor in &self.pre_processors {
            diagnostics.record_immediate(
                pre_processor.name(),
                PluginStatus::Skipped("child already terminated".into()),
            );
        }

        let data = CollectedData::default();
        let json_path: Option<PathBuf> = run_transaction_stage(
            &transaction,
            "Stage2Json",
            PluginExecution::Cooperative,
            STAGE_TIMEOUT,
            |_| {
                let mut crash_report = report::build_report(event, &data, &diagnostics);
                report::write_report(&transaction, &mut crash_report, &[])
            },
        )
        .into_option();
        let mut result = ReportResult {
            artifact_paths: transaction.artifact_paths(),
            raw_path: None,
            json_path,
            session: None,
        };

        for post_processor in &self.post_processors {
            if post_processor.phase() != PostProcessorPhase::BeforeCommit {
                continue;
            }
            if !post_processor.is_available() {
                diagnostics.record_immediate(
                    post_processor.name(),
                    PluginStatus::Skipped("not available".into()),
                );
                continue;
            }
            if !deps_satisfied(post_processor.hard_dependencies(), &diagnostics) {
                diagnostics.record_immediate(
                    post_processor.name(),
                    PluginStatus::Skipped("hard dependency not met".into()),
                );
                continue;
            }
            let start = Instant::now();
            let timeout = post_processor.timeout().resolve(POSTPROC_TIMEOUT);
            let outcome = run_transaction_stage(
                &transaction,
                post_processor.name(),
                post_processor.execution(),
                timeout,
                |context| post_processor.process(event, &mut result, context),
            );
            let status = plugin_status(&outcome);
            diagnostics.record(post_processor.name(), status, start.elapsed());
        }

        let committed = match transaction.commit() {
            Ok(committed) => committed,
            Err(error) => {
                diagnostics.record_immediate("ArtifactCommit", PluginStatus::Error(error));
                return diagnostics;
            }
        };
        for warning in &committed.durability_warnings {
            diagnostics
                .record_immediate("ArtifactDurability", PluginStatus::Error(warning.clone()));
        }
        result.raw_path = result.raw_path.as_deref().and_then(|path| {
            remap_committed_path(path, transaction.staging_dir(), &committed.report_dir)
        });
        result.json_path = result.json_path.as_deref().and_then(|path| {
            remap_committed_path(path, transaction.staging_dir(), &committed.report_dir)
        });
        result.artifact_paths = result
            .artifact_paths
            .iter()
            .filter_map(|path| {
                remap_committed_path(path, transaction.staging_dir(), &committed.report_dir)
            })
            .collect();

        for post_processor in &self.post_processors {
            if post_processor.phase() != PostProcessorPhase::AfterCommit {
                continue;
            }
            if !post_processor.is_available() {
                diagnostics.record_immediate(
                    post_processor.name(),
                    PluginStatus::Skipped("not available".into()),
                );
                continue;
            }
            if !deps_satisfied(post_processor.hard_dependencies(), &diagnostics) {
                diagnostics.record_immediate(
                    post_processor.name(),
                    PluginStatus::Skipped("hard dependency not met".into()),
                );
                continue;
            }
            let start = Instant::now();
            let timeout = post_processor.timeout().resolve(POSTPROC_TIMEOUT);
            let outcome = run_transaction_stage(
                &transaction,
                post_processor.name(),
                post_processor.execution(),
                timeout,
                |context| post_processor.process(event, &mut result, context),
            );
            diagnostics.record(
                post_processor.name(),
                plugin_status(&outcome),
                start.elapsed(),
            );
        }

        if let Some(ref path) = result.json_path {
            for notifier in &self.notifiers {
                if !notifier.is_available() {
                    diagnostics.record_immediate(
                        notifier.name(),
                        PluginStatus::Skipped("not available".into()),
                    );
                    continue;
                }
                if !deps_satisfied(notifier.hard_dependencies(), &diagnostics) {
                    diagnostics.record_immediate(
                        notifier.name(),
                        PluginStatus::Skipped("hard dependency not met".into()),
                    );
                    continue;
                }
                let start = Instant::now();
                let timeout = notifier.timeout().resolve(NOTIFIER_TIMEOUT);
                let outcome = run_transaction_stage(
                    &transaction,
                    notifier.name(),
                    notifier.execution(),
                    timeout,
                    |context| notifier.notify(path, context),
                );
                let status = plugin_status(&outcome);
                diagnostics.record(notifier.name(), status, start.elapsed());
            }
        }

        for post_processor in &self.post_processors {
            if post_processor.phase() != PostProcessorPhase::AfterNotify {
                continue;
            }
            if !post_processor.is_available() {
                diagnostics.record_immediate(
                    post_processor.name(),
                    PluginStatus::Skipped("not available".into()),
                );
                continue;
            }
            if !deps_satisfied(post_processor.hard_dependencies(), &diagnostics) {
                diagnostics.record_immediate(
                    post_processor.name(),
                    PluginStatus::Skipped("hard dependency not met".into()),
                );
                continue;
            }
            let start = Instant::now();
            let timeout = post_processor.timeout().resolve(POSTPROC_TIMEOUT);
            let outcome = run_transaction_stage(
                &transaction,
                post_processor.name(),
                post_processor.execution(),
                timeout,
                |context| post_processor.process(event, &mut result, context),
            );
            diagnostics.record(
                post_processor.name(),
                plugin_status(&outcome),
                start.elapsed(),
            );
        }

        for post_processor in &self.post_processors {
            if post_processor.phase() != PostProcessorPhase::FinalCleanup {
                continue;
            }
            if !post_processor.is_available() {
                diagnostics.record_immediate(
                    post_processor.name(),
                    PluginStatus::Skipped("not available".into()),
                );
                continue;
            }
            if !deps_satisfied(post_processor.hard_dependencies(), &diagnostics) {
                diagnostics.record_immediate(
                    post_processor.name(),
                    PluginStatus::Skipped("hard dependency not met".into()),
                );
                continue;
            }
            let start = Instant::now();
            let timeout = post_processor.timeout().resolve(POSTPROC_TIMEOUT);
            let outcome = run_transaction_stage(
                &transaction,
                post_processor.name(),
                post_processor.execution(),
                timeout,
                |context| post_processor.process(event, &mut result, context),
            );
            diagnostics.record(
                post_processor.name(),
                plugin_status(&outcome),
                start.elapsed(),
            );
        }
        persist_final_diagnostics(&committed, &mut diagnostics);
        transaction.release_publication_lease();

        diagnostics.report_path = result.json_path;

        diagnostics
    }

    /// Validate global plugin identity and every stage's dependency graph.
    ///
    /// # Errors
    /// Returns a structured error for duplicate IDs, missing hard
    /// dependencies, cycles, or invalid registration order.
    pub fn validate_dependencies(&self) -> Result<(), crate::config::ConfigValidationError> {
        let categories = vec![
            (PluginCategory::Filter, plugin_graph_nodes(&self.filters)),
            (
                PluginCategory::Collector,
                plugin_graph_nodes(&self.collectors),
            ),
            (
                PluginCategory::PreProcessor,
                plugin_graph_nodes(&self.pre_processors),
            ),
            (
                PluginCategory::PostProcessor,
                plugin_graph_nodes(&self.post_processors),
            ),
            (
                PluginCategory::Notifier,
                plugin_graph_nodes(&self.notifiers),
            ),
        ];
        crate::config::validate_runtime_plugin_registry(&categories)?;

        let phases = self
            .post_processors
            .iter()
            .map(|plugin| (plugin.id(), plugin.phase()))
            .collect::<std::collections::BTreeMap<_, _>>();
        for plugin in &self.post_processors {
            for dependency in plugin.dependencies() {
                if phases
                    .get(&dependency.plugin)
                    .is_some_and(|dependency_phase| {
                        post_processor_phase_rank(*dependency_phase)
                            > post_processor_phase_rank(plugin.phase())
                    })
                {
                    return Err(
                        crate::config::ConfigValidationError::InvalidDependencyOrder {
                            category: PluginCategory::PostProcessor,
                            plugin_id: plugin.id().to_string(),
                            dependency: dependency.plugin.to_string(),
                            kind: dependency.kind,
                        },
                    );
                }
            }
        }
        Ok(())
    }
}

const STARTUP_STALE_ARTIFACT_AGE: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct StartupRecovery {
    pub recovered: usize,
    pub scavenged: usize,
}

fn merge_owned_collected_data(target: &mut CollectedData, owned: &mut CollectedData) {
    target.raw.breadcrumbs.append(&mut owned.raw.breadcrumbs);
    if target.raw.crash_context.is_none() {
        target.raw.crash_context = owned.raw.crash_context.take();
    }
    if target.raw.settings_snapshot.is_none() {
        target.raw.settings_snapshot = owned.raw.settings_snapshot.take();
    }
    target.raw.attachments.append(&mut owned.raw.attachments);
    target
        .raw
        .attachment_registrations
        .append(&mut owned.raw.attachment_registrations);
    target.raw.screenshots.append(&mut owned.raw.screenshots);
    if target.raw.environment.is_none() {
        target.raw.environment = owned.raw.environment.take();
    }
    if target.raw.process_output.is_none() {
        target.raw.process_output = owned.raw.process_output.take();
    }
}

fn persist_final_diagnostics(committed: &CommittedReport, diagnostics: &mut Diagnostics) {
    let Some(final_diagnostics) =
        crate::preprocessors::report_formatter::build_diagnostics_json(diagnostics)
    else {
        return;
    };
    if let Err(error) = committed.persist_final_diagnostics(final_diagnostics) {
        eprintln!("[monitor] Failed to persist final report diagnostics: {error}");
        diagnostics.record_immediate("FinalDiagnostics", PluginStatus::Error(error));
    }
}

const fn post_processor_phase_rank(phase: PostProcessorPhase) -> u8 {
    match phase {
        PostProcessorPhase::BeforeCommit => 0,
        PostProcessorPhase::AfterCommit => 1,
        PostProcessorPhase::AfterNotify => 2,
        PostProcessorPhase::FinalCleanup => 3,
    }
}

/// Stable dependency-aware priority order. A dependency always wins over the
/// dependent's priority; otherwise lower enum values run first.
fn stable_plugin_order<T: Plugin + ?Sized>(
    plugins: &mut Vec<Box<T>>,
    phase_rank: impl Fn(&T) -> u8,
) {
    let mut remaining: Vec<(usize, Box<T>)> =
        std::mem::take(plugins).into_iter().enumerate().collect();

    while !remaining.is_empty() {
        let candidate = remaining
            .iter()
            .enumerate()
            .filter(|(_, (_, plugin))| {
                plugin.dependencies().iter().all(|dependency| {
                    !remaining
                        .iter()
                        .any(|(_, pending)| pending.id() == dependency.plugin)
                })
            })
            .min_by_key(|(_, (insertion_index, plugin))| {
                (
                    phase_rank(plugin.as_ref()),
                    plugin.priority(),
                    *insertion_index,
                )
            })
            .map(|(index, _)| index);

        let Some(candidate) = candidate else {
            // Validation below reports cycles and duplicate IDs precisely.
            plugins.extend(remaining.into_iter().map(|(_, plugin)| plugin));
            break;
        };
        plugins.push(remaining.remove(candidate).1);
    }
}

fn deps_satisfied(deps: &[&str], diagnostics: &Diagnostics) -> bool {
    deps.iter().all(|dep| diagnostics.succeeded(dep))
}

fn remap_committed_path(
    path: &std::path::Path,
    staging: &std::path::Path,
    committed: &std::path::Path,
) -> Option<PathBuf> {
    let relative = path.strip_prefix(staging).ok()?;
    (relative.components().count() == 1).then(|| committed.join(relative))
}

/// Validate one enabled runtime category without panicking. Missing order-only
/// dependencies are permitted; hard dependencies must exist and precede the
/// dependent.
///
/// # Errors
/// Returns a structured configuration error for duplicate plugin IDs, missing
/// hard dependencies, cycles, or invalid registration order.
pub fn validate_plugin_order<T: Plugin + ?Sized>(
    category: PluginCategory,
    plugins: &[Box<T>],
) -> Result<(), crate::config::ConfigValidationError> {
    crate::config::validate_runtime_plugin_graph(category, &plugin_graph_nodes(plugins))
}

fn plugin_graph_nodes<T: Plugin + ?Sized>(
    plugins: &[Box<T>],
) -> Vec<crate::config::PluginGraphNode> {
    plugins
        .iter()
        .map(|plugin| crate::config::PluginGraphNode {
            id: plugin.id().to_string(),
            hard_dependencies: plugin
                .dependencies()
                .iter()
                .filter(|dependency| dependency.kind == DependencyKind::Hard)
                .map(|dependency| dependency.plugin.to_string())
                .collect(),
            order_dependencies: plugin
                .dependencies()
                .iter()
                .filter(|dependency| dependency.kind == DependencyKind::OrderOnly)
                .map(|dependency| dependency.plugin.to_string())
                .collect(),
        })
        .collect()
}

#[cfg(target_os = "macos")]
const fn should_register_attachment_copier(
    attachment_collector_registered: bool,
    pre_processors_enabled: bool,
) -> bool {
    attachment_collector_registered && pre_processors_enabled
}

// ═══════════════════════════════════════════════════
//  Pipeline factory
// ═══════════════════════════════════════════════════

#[cfg(target_os = "macos")]
#[allow(clippy::too_many_lines)] // pipeline factory — splitting would scatter registration logic
/// Load configuration and build the validated default macOS pipeline.
///
/// # Errors
/// Returns a structured configuration error when plugin enablement or the
/// assembled runtime dependency graph is invalid.
pub fn default_macos_pipeline(
    shm: Option<std::sync::Arc<crate::shm::SharedMemory>>,
) -> Result<Pipeline, crate::config::ConfigLoadError> {
    let config = crate::config::load_validated_config()?;
    default_macos_pipeline_from_config(shm, &config).map_err(crate::config::ConfigLoadError::from)
}

/// Build the default macOS pipeline from one already-loaded configuration.
///
/// Keeping loading outside this constructor lets startup, event dispatch, and
/// plugin registration share exactly the same immutable enablement snapshot.
///
/// # Errors
/// Returns a structured configuration error when the assembled runtime plugin
/// IDs or dependency graph are invalid.
#[cfg(target_os = "macos")]
#[allow(clippy::too_many_lines)] // pipeline factory — splitting would scatter registration logic
pub fn default_macos_pipeline_from_config(
    shm: Option<std::sync::Arc<crate::shm::SharedMemory>>,
    validated: &crate::config::ValidatedConfig,
) -> Result<Pipeline, crate::config::ConfigValidationError> {
    default_macos_pipeline_from_config_with_environment(shm, validated, None)
}

/// Build the default pipeline while injecting the exact environment that will
/// be supplied to the monitored child.
///
/// # Errors
/// Returns a structured configuration error when the assembled runtime plugin
/// registry or dependency graph is invalid.
#[cfg(target_os = "macos")]
#[allow(clippy::too_many_lines)]
pub fn default_macos_pipeline_from_config_with_environment(
    shm: Option<std::sync::Arc<crate::shm::SharedMemory>>,
    validated: &crate::config::ValidatedConfig,
    child_environment: Option<std::sync::Arc<crate::collectors::ChildEnvironmentSnapshot>>,
) -> Result<Pipeline, crate::config::ConfigValidationError> {
    default_macos_pipeline_from_config_with_runtime(shm, validated, child_environment, None)
}

/// Build the default pipeline with monitor-owned spawn-time inputs.
///
/// # Errors
/// Returns a structured configuration error when the assembled runtime plugin
/// registry or dependency graph is invalid.
#[cfg(target_os = "macos")]
#[allow(clippy::too_many_lines)]
pub fn default_macos_pipeline_from_config_with_runtime(
    shm: Option<std::sync::Arc<crate::shm::SharedMemory>>,
    validated: &crate::config::ValidatedConfig,
    child_environment: Option<std::sync::Arc<crate::collectors::ChildEnvironmentSnapshot>>,
    child_output: Option<std::sync::Arc<crate::platform::ChildOutputCapture>>,
) -> Result<Pipeline, crate::config::ConfigValidationError> {
    use crate::collectors::{
        DylibCollector, EnvironmentCollector, MemoryCollector, ThreadCollector,
    };
    use crate::filters::{DiskSpaceFilter, RateLimiter};
    use crate::notifiers::{ConsoleNotifier, SystemNotification};
    use crate::platform::MacOsPlatform;
    use crate::postprocessors::{
        FeedbackPostProcessor, LogRotator, MoveToSent, PNGConverter, RawCleanup, RetentionManager,
        SessionRecorder, ZIPArchiver,
    };
    use crate::preprocessors::{
        BuildInfoEnricher, DuplicateDetector, Fingerprinter, Sanitizer, SessionEnricher,
        SymbolResolver,
    };
    use std::time::Duration;

    let cfg = validated.config();
    let output_dir = cfg.report_dir.as_ref().map(PathBuf::from);
    let triggers = TriggerPolicy::from(validated.triggers);
    let collection_policy = validated.collection_policy();

    // ── Early out: global kill switch ──
    if !validated.enabled {
        let platform: Arc<dyn PlatformOps> = Arc::new(MacOsPlatform::default());
        return Ok(Pipeline {
            enabled: false,
            triggers,
            collection_policy,
            filters: vec![],
            collectors: vec![],
            shm,
            platform,
            pre_processors: vec![],
            post_processors: vec![],
            notifiers: vec![],
            output_dir: None,
        });
    }

    let platform: Arc<dyn PlatformOps> = Arc::new(MacOsPlatform::default());

    // Dependency closure and category switches were resolved at config load.
    let on = |plugin_id: &'static str| validated.plugin_enabled(PluginId::new(plugin_id));

    // ── Filters ──
    let mut filters: Vec<Box<dyn Filter>> = vec![];
    if on("DiskSpaceFilter") {
        filters.push(Box::new(DiskSpaceFilter::new(
            cfg.filters.disk_space.min_free_mb,
        )));
    }
    if on("RateLimiter") {
        let rate_state_root = output_dir
            .clone()
            .or_else(|| crate::utils::paths::pending_dir_path().ok());
        let limiter = rate_state_root.map_or_else(
            || {
                RateLimiter::new(
                    cfg.filters.rate_limiter.max_events,
                    Duration::from_secs(cfg.filters.rate_limiter.window_secs),
                )
            },
            |root| {
                RateLimiter::with_state_path(
                    cfg.filters.rate_limiter.max_events,
                    Duration::from_secs(cfg.filters.rate_limiter.window_secs),
                    root.join(".rate-limit-state.json"),
                )
            },
        );
        filters.push(Box::new(limiter));
    }

    // ── Collectors ──
    let mut collectors: Vec<Box<dyn Collector>> = vec![];
    let mut attachment_copy_enabled = false;

    if on("ProcessOutputCollector")
        && let Some(child_output) = child_output
    {
        collectors.push(Box::new(crate::collectors::ProcessOutputCollector::new(
            child_output,
        )));
    }

    if on("ThreadCollector") {
        collectors.push(Box::new(ThreadCollector::new(
            platform.clone(),
            collection_policy.capture_stack_memory,
        )));
    }

    if shm.is_some() {
        use crate::collectors::{BreadcrumbCollector, ContextCollector};
        if on("BreadcrumbCollector") {
            collectors.push(Box::new(BreadcrumbCollector::new()));
        }
        if on("ContextCollector") {
            collectors.push(Box::new(ContextCollector::new()));
        }
    }

    if on("MemoryCollector") {
        collectors.push(Box::new(MemoryCollector::new(platform.clone())));
    }
    if on("DylibCollector") {
        collectors.push(Box::new(DylibCollector::new(platform.clone())));
    }

    if shm.is_some() {
        use crate::collectors::{AttachmentCollector, ScreenshotCollector};
        if on("ScreenshotCollector") {
            collectors.push(Box::new(ScreenshotCollector::new()));
        }
        if on("AttachmentCollector") {
            collectors.push(Box::new(AttachmentCollector::new()));
            attachment_copy_enabled = true;
        }
    }

    if on("EnvironmentCollector") {
        collectors.push(Box::new(child_environment.map_or_else(
            EnvironmentCollector::new,
            EnvironmentCollector::with_child_environment,
        )));
    }

    // ── Pre-processors (order matters: dependencies must come first) ──
    let mut pre_processors: Vec<Box<dyn PreProcessor>> = vec![];

    if should_register_attachment_copier(attachment_copy_enabled, cfg.pre_processors.enabled) {
        pre_processors.push(Box::new(crate::collectors::AttachmentCopier::new()));
    }

    if on("SessionEnricher") {
        pre_processors.push(Box::new(SessionEnricher));
    }
    if on("SymbolResolver") {
        pre_processors.push(Box::new(SymbolResolver::new()));
    }
    if on("Fingerprinter") {
        pre_processors.push(Box::new(Fingerprinter::new(
            cfg.pre_processors.fingerprint.top_frames,
        )));
    }
    if on("BuildInfoEnricher") {
        pre_processors.push(Box::new(BuildInfoEnricher));
    }
    if on("DuplicateDetector") {
        pre_processors.push(Box::new(DuplicateDetector::new(Duration::from_secs(
            cfg.pre_processors.duplicate.window_secs,
        ))));
    }
    if on("Sanitizer") {
        pre_processors.push(Box::new(Sanitizer::new()));
    }

    // ── Post-processors ──
    // Before commit: RawCleanup/PNG/Feedback/ZIP/Move mutate only staging.
    // After commit: SessionRecorder/LogRotator observe publication.
    // Final cleanup: Retention runs after notifiers and AfterNotify consumers.
    let mut post_processors: Vec<Box<dyn PostProcessor>> = vec![];

    if on("RawCleanup") {
        post_processors.push(Box::new(RawCleanup));
    }
    if on("SessionRecorder") {
        post_processors.push(Box::new(SessionRecorder));
    }
    // PNG conversion must run BEFORE FeedbackPostProcessor (which can block for
    // 5 minutes waiting on a user dialog) and BEFORE ZIPArchiver (so the zip
    // contains converted PNGs, not raw RGBA dumps).
    if on("PNGConverter") {
        post_processors.push(Box::new(PNGConverter));
    }

    // Production accepts only the signed sibling helper. An arbitrary
    // environment override exists solely in explicitly test-enabled builds.
    if on("FeedbackDialog") {
        let sibling = || {
            std::env::current_exe()
                .ok()
                .and_then(|path| path.parent().map(|dir| dir.join("crash_dialog_macos")))
        };
        #[cfg(feature = "test-support")]
        let dialog = std::env::var_os("CRASH_MONITOR_DIALOG_BIN")
            .map(std::path::PathBuf::from)
            .map(FeedbackPostProcessor::for_test)
            .or_else(|| sibling().map(FeedbackPostProcessor::new));
        #[cfg(not(feature = "test-support"))]
        let dialog = sibling().map(FeedbackPostProcessor::new);
        if let Some(dialog) = dialog
            && dialog.is_available()
        {
            post_processors.push(Box::new(dialog));
        }
    }

    if on("ZIPArchiver") {
        post_processors.push(Box::new(ZIPArchiver));
    }
    // Relocate finished reports pending/ → sent/ before retention scans sent/.
    if on("MoveToSent") {
        post_processors.push(Box::new(MoveToSent::new()));
    }
    if on("LogRotator") {
        post_processors.push(Box::new(LogRotator::new(
            cfg.post_processors.log_rotator.max_size_mb,
        )));
    }
    if on("RetentionManager") {
        post_processors.push(Box::new(RetentionManager::new(
            cfg.post_processors.retention.max_reports,
            cfg.post_processors.retention.max_size_mb,
            cfg.post_processors.retention.max_age_days,
        )));
    }

    // ── Notifiers ──
    let mut notifiers: Vec<Box<dyn Notifier>> = vec![];

    if on("ConsoleNotifier") {
        notifiers.push(Box::new(ConsoleNotifier));
    }
    if on("SystemNotification") {
        notifiers.push(Box::new(SystemNotification::new(true)));
    }

    Pipeline {
        enabled: true,
        triggers,
        collection_policy,
        filters,
        collectors,
        shm,
        platform,
        pre_processors,
        post_processors,
        notifiers,
        output_dir,
    }
    .finish_registration()
}

#[cfg(test)]
#[path = "../../tests/unit/pipeline/mod_tests.rs"]
mod tests;
