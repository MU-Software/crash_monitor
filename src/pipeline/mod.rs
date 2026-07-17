//! Plugin pipeline architecture for crash/snapshot report generation.
//!
//! Design: `docs/plans/crash_reporter.md` L1493-1867

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

pub use safety::{
    CancellationToken, PluginContext, PluginRunResult, SubprocessOutput,
    run_plugin_catching_panics, run_plugin_cooperative, run_plugin_subprocess,
};
pub use traits::{
    Collector, Filter, Notifier, Plugin, PluginExecution, PostProcessor, PreProcessor,
};
pub use types::{
    CaptureOutcome, CapturePayload, CapturedEvent, CollectedData, CrashEvent, DependencyKind,
    Diagnostics, PluginCategory, PluginStatus, Priority, RawShmSnapshot, ReportResult, ReportType,
    TerminationReason,
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

// Category-specific cooperative deadlines (seconds).
const FILTER_TIMEOUT: u32 = 1;
const COLLECTOR_TIMEOUT: u32 = 5;
const PREPROC_TIMEOUT: u32 = 2;
const POSTPROC_TIMEOUT: u32 = 30;
const NOTIFIER_TIMEOUT: u32 = 5;
const STAGE_TIMEOUT: u32 = 5;

fn run_stage<T>(
    name: &str,
    execution: PluginExecution,
    timeout_secs: u32,
    f: impl FnOnce(&PluginContext) -> Result<T, String>,
) -> PluginRunResult<T> {
    let timeout = (timeout_secs != 0).then(|| Duration::from_secs(u64::from(timeout_secs)));
    let context = PluginContext::from_timeout(timeout);
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
    timeout_secs: u32,
    cancellation: CancellationToken,
    shm_snapshot: Option<Arc<crate::shm::OwnedShmSnapshot>>,
    f: impl FnOnce(&PluginContext) -> Result<T, String>,
) -> PluginRunResult<T> {
    let timeout = (timeout_secs != 0).then(|| Duration::from_secs(u64::from(timeout_secs)));
    let context = PluginContext::from_timeout_and_cancellation(timeout, cancellation)
        .with_shm_snapshot(shm_snapshot);
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
        PluginRunResult::Panicked => PluginStatus::Error("panicked".to_string()),
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
        self.shm
            .as_ref()
            .map(|shm| {
                shm.snapshot_owned_until(deadline)
                    .map(Arc::new)
                    .map_err(|error| format!("shared-memory snapshot failed: {error}"))
            })
            .transpose()
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
            CaptureOutcome::Captured(captured) => self.finalize_captured(captured),
            CaptureOutcome::Skipped(diagnostics) => diagnostics,
        }
    }

    /// Suspend, collect task-facing state, release thread rights, and resume.
    fn capture_event(&self, event: &CrashEvent, task: mach_port_t) -> CaptureOutcome {
        if !self.report_enabled(event.report_type) {
            return CaptureOutcome::Skipped(Diagnostics::new());
        }
        let mut diagnostics = Diagnostics::new();
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
                Ok(snapshot) => snapshot,
                Err(error) => {
                    diagnostics.record_immediate("ShmSnapshot", PluginStatus::Error(error));
                    None
                }
            }
        } else {
            None
        };

        let cancelled = Arc::new(AtomicBool::new(false));
        let mut payload = self.collect_snapshot(event, task, &cancelled, shm_snapshot.as_ref());

        if let Some(guard) = suspend_guard {
            guard.finish();
        }
        failure_sink.drain_into(&mut payload.diagnostics);

        diagnostics.plugins.append(&mut payload.diagnostics.plugins);
        CaptureOutcome::Captured(CapturedEvent::new(
            event.clone(),
            CapturePayload {
                data: payload.data,
                raw_shm: payload.raw_shm,
                diagnostics,
            },
        ))
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
            let timeout = plugin_timeout(c.timeout_secs(), COLLECTOR_TIMEOUT);
            let cancellation = CancellationToken::from_atomic(cancelled.clone());
            let outcome = run_cancellable_stage(
                c.name(),
                c.execution(),
                timeout,
                cancellation,
                shm_snapshot.cloned(),
                |context| c.collect(event, task, &mut data, context),
            );
            let status = plugin_status(&outcome);
            diagnostics.record(c.name(), status, start.elapsed());
        }

        let raw_shm = if cancelled.load(Ordering::Acquire) {
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
    ) -> CapturePayload {
        self.collect_snapshot(event, task, cancelled, shm_snapshot)
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
        let event = &captured.event;
        let data = &mut captured.data;
        let diagnostics = &mut captured.diagnostics;

        let pending = match &self.output_dir {
            Some(dir) => dir.clone(),
            None => match crate::utils::paths::pending_dir() {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("[monitor] Failed to get pending dir: {e}");
                    return std::mem::take(diagnostics);
                }
            },
        };

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
            let timeout = plugin_timeout(filter.timeout_secs(), FILTER_TIMEOUT);
            let outcome = run_stage(filter.name(), filter.execution(), timeout, |context| {
                filter.should_process(event, context)
            });
            let status = plugin_status(&outcome);
            let pass = outcome.into_option().unwrap_or(true);
            diagnostics.record(filter.name(), status, start.elapsed());
            if !pass {
                return std::mem::take(diagnostics);
            }
        }

        // ── Pre-processors ──
        for pp in &self.pre_processors {
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
            let timeout = plugin_timeout(pp.timeout_secs(), PREPROC_TIMEOUT);
            let outcome = run_stage(pp.name(), pp.execution(), timeout, |context| {
                pp.process(event, data, context)
            });
            let status = plugin_status(&outcome);
            diagnostics.record(pp.name(), status, start.elapsed());
        }

        // ── Duplicate check (set by DuplicateDetector pre-processor) ──
        if data.duplicate_detected {
            eprintln!("[monitor] Duplicate event detected, skipping report");
            return std::mem::take(diagnostics);
        }

        // ── Stage 1: Raw data (fail-safe) ──
        let raw_path: Option<PathBuf> = run_stage(
            "Stage1Raw",
            PluginExecution::Cooperative,
            STAGE_TIMEOUT,
            |_| safety::write_raw_stage1(&pending, event.report_type, event.pid, &data.raw.threads),
        )
        .into_option();

        // Stage 1 shm dump (breadcrumbs + context raw bytes)
        if let Some(raw_shm) = &captured.raw_shm {
            let _ = run_stage(
                "Stage1Shm",
                PluginExecution::Cooperative,
                STAGE_TIMEOUT,
                |_| safety::write_raw_shm_stage1(&pending, event.report_type, event.pid, raw_shm),
            );
        }

        // ── Stage 2: Full JSON report + screenshot PNGs ──
        let screenshots = std::mem::take(&mut data.raw.screenshots);
        let json_path: Option<PathBuf> = run_stage(
            "Stage2Json",
            PluginExecution::Cooperative,
            STAGE_TIMEOUT,
            |_| {
                let mut crash_report = report::build_report(event, data, diagnostics);
                report::write_report(&pending, &mut crash_report, &screenshots)
            },
        )
        .into_option();

        let result = ReportResult {
            raw_path,
            json_path,
            session: data.session.clone(),
        };

        // ── Post-processors ──
        let mut result = result;
        for pp in &self.post_processors {
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
            let timeout = plugin_timeout(pp.timeout_secs(), POSTPROC_TIMEOUT);
            let outcome = run_stage(pp.name(), pp.execution(), timeout, |context| {
                pp.process(event, &mut result, context)
            });
            let status = plugin_status(&outcome);
            diagnostics.record(pp.name(), status, start.elapsed());
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
                let timeout = plugin_timeout(n.timeout_secs(), NOTIFIER_TIMEOUT);
                let outcome = run_stage(n.name(), n.execution(), timeout, |context| {
                    n.notify(path, context)
                });
                let status = plugin_status(&outcome);
                diagnostics.record(n.name(), status, start.elapsed());
            }
        }

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
            let timeout = plugin_timeout(filter.timeout_secs(), FILTER_TIMEOUT);
            let outcome = run_stage(filter.name(), filter.execution(), timeout, |context| {
                filter.should_process(event, context)
            });
            let status = plugin_status(&outcome);
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
        let json_path: Option<PathBuf> = run_stage(
            "Stage2Json",
            PluginExecution::Cooperative,
            STAGE_TIMEOUT,
            |_| {
                let mut crash_report = report::build_report(event, &data, &diagnostics);
                report::write_report(&pending, &mut crash_report, &[])
            },
        )
        .into_option();
        let mut result = ReportResult {
            raw_path: None,
            json_path,
            session: None,
        };

        for post_processor in &self.post_processors {
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
            let timeout = plugin_timeout(post_processor.timeout_secs(), POSTPROC_TIMEOUT);
            let outcome = run_stage(
                post_processor.name(),
                post_processor.execution(),
                timeout,
                |context| post_processor.process(event, &mut result, context),
            );
            let status = plugin_status(&outcome);
            diagnostics.record(post_processor.name(), status, start.elapsed());
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
                let timeout = plugin_timeout(notifier.timeout_secs(), NOTIFIER_TIMEOUT);
                let outcome =
                    run_stage(notifier.name(), notifier.execution(), timeout, |context| {
                        notifier.notify(path, context)
                    });
                let status = plugin_status(&outcome);
                diagnostics.record(notifier.name(), status, start.elapsed());
            }
        }

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
        crate::config::validate_runtime_plugin_registry(&categories)
    }
}

/// Resolve per-plugin timeout: `u32::MAX` → category default, otherwise plugin override.
fn plugin_timeout(plugin_override: u32, category_default: u32) -> u32 {
    if plugin_override == u32::MAX {
        category_default
    } else {
        plugin_override
    }
}

fn deps_satisfied(deps: &[&str], diagnostics: &Diagnostics) -> bool {
    deps.iter().all(|dep| diagnostics.succeeded(dep))
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
            id: plugin.name().to_string(),
            hard_dependencies: plugin
                .hard_dependencies()
                .iter()
                .map(|dependency| (*dependency).to_string())
                .collect(),
            order_dependencies: plugin
                .order_after()
                .iter()
                .map(|dependency| (*dependency).to_string())
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
) -> Result<Pipeline, crate::config::ConfigValidationError> {
    let config = crate::config::load_validated_config()?;
    default_macos_pipeline_from_config(shm, &config)
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
    let triggers = TriggerPolicy::from(validated.triggers);

    // ── Early out: global kill switch ──
    if !validated.enabled {
        let platform: Arc<dyn PlatformOps> = Arc::new(MacOsPlatform::default());
        return Ok(Pipeline {
            enabled: false,
            triggers,
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
    let on = |plugin_id: &str| validated.plugin_enabled(plugin_id);

    // ── Filters ──
    let mut filters: Vec<Box<dyn Filter>> = vec![];
    if on("DiskSpaceFilter") {
        filters.push(Box::new(DiskSpaceFilter::new(
            cfg.filters.disk_space.min_free_mb,
        )));
    }
    if on("RateLimiter") {
        filters.push(Box::new(RateLimiter::new(
            cfg.filters.rate_limiter.max_events,
            Duration::from_secs(cfg.filters.rate_limiter.window_secs),
        )));
    }

    // ── Collectors ──
    let mut collectors: Vec<Box<dyn Collector>> = vec![];
    let mut attachment_copy_enabled = false;

    if on("ThreadCollector") {
        collectors.push(Box::new(ThreadCollector::new(platform.clone())));
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
        collectors.push(Box::new(EnvironmentCollector::new()));
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

    // ── Post-processors (order matters: RawCleanup → Session → Feedback → ZIP → LogRotator → Retention) ──
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

    // Feedback dialog: CRASH_MONITOR_DIALOG_BIN overrides the default path — used by
    // E2E tests to substitute the mock dialog (no UI, fixed stdout output).
    if on("FeedbackDialog") {
        let dialog_bin = std::env::var_os("CRASH_MONITOR_DIALOG_BIN")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::current_exe()
                    .ok()
                    .and_then(|p| p.parent().map(|d| d.join("crash_dialog_macos")))
            });
        if let Some(bin) = dialog_bin
            && bin.exists()
        {
            post_processors.push(Box::new(FeedbackPostProcessor::new(bin)));
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

    let pipeline = Pipeline {
        enabled: true,
        triggers,
        filters,
        collectors,
        shm,
        platform,
        pre_processors,
        post_processors,
        notifiers,
        output_dir: None,
    };
    pipeline.validate_dependencies()?;
    Ok(pipeline)
}

#[cfg(test)]
#[path = "../../tests/unit/pipeline/mod_tests.rs"]
mod tests;
