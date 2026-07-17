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

use crate::platform::PlatformOps;

pub use safety::{
    CancellationToken, PluginContext, PluginRunResult, SubprocessOutput,
    run_plugin_catching_panics, run_plugin_cooperative, run_plugin_subprocess,
};
pub use traits::{
    Collector, Filter, Notifier, Plugin, PluginExecution, PostProcessor, PreProcessor,
};
pub use types::{
    CaptureOutcome, CapturePayload, CapturedEvent, CollectedData, CrashEvent, Diagnostics,
    PluginStatus, Priority, RawShmSnapshot, ReportResult, ReportType, TerminationReason,
};

// ═══════════════════════════════════════════════════
//  Pipeline
// ═══════════════════════════════════════════════════

pub struct Pipeline {
    pub filters: Vec<Box<dyn Filter>>,
    pub collectors: Vec<Box<dyn Collector>>,
    pub pre_processors: Vec<Box<dyn PreProcessor>>,
    pub post_processors: Vec<Box<dyn PostProcessor>>,
    #[allow(dead_code)] // Phase 4+
    pub notifiers: Vec<Box<dyn Notifier>>,
    /// Shared memory handle for Stage 1 raw dump (None if shm unavailable).
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
    f: impl FnOnce(&PluginContext) -> Result<T, String>,
) -> PluginRunResult<T> {
    let timeout = (timeout_secs != 0).then(|| Duration::from_secs(u64::from(timeout_secs)));
    let context = PluginContext::from_timeout_and_cancellation(timeout, cancellation);
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

impl Pipeline {
    /// Process a crash/snapshot event synchronously.
    ///
    /// Production Mach events use [`worker`] so finalization cannot delay
    /// target resume or the exception reply. This wrapper remains useful for
    /// callers and unit tests that explicitly want synchronous completion.
    #[must_use]
    pub fn handle_event(&self, event: &CrashEvent, task: mach_port_t) -> Diagnostics {
        match self.capture_event(event, task) {
            CaptureOutcome::Captured(captured) => self.finalize_captured(captured),
            CaptureOutcome::Skipped(diagnostics) => diagnostics,
        }
    }

    /// Suspend, collect task-facing state, release thread rights, and resume.
    fn capture_event(&self, event: &CrashEvent, task: mach_port_t) -> CaptureOutcome {
        let mut diagnostics = Diagnostics::new();

        let suspended = if let Err(e) = self.platform.suspend_task(task) {
            if event.bail_on_suspend_failure {
                eprintln!("[monitor] {e}");
                return CaptureOutcome::Skipped(diagnostics);
            }
            eprintln!("[monitor] suspend_task failed (proceeding with best-effort): {e}");
            false
        } else {
            true
        };

        let cancelled = Arc::new(AtomicBool::new(false));
        let payload = self.collect_snapshot(event, task, &cancelled);

        if suspended && let Err(e) = self.platform.resume_task(task) {
            eprintln!("[monitor] resume_task failed: {e}");
        }

        diagnostics = payload.diagnostics;
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
    ) -> CapturePayload {
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
            if !deps_satisfied(c.depends_on(), &diagnostics) {
                diagnostics
                    .record_immediate(c.name(), PluginStatus::Skipped("dependency not met".into()));
                continue;
            }
            let start = Instant::now();
            let timeout = plugin_timeout(c.timeout_secs(), COLLECTOR_TIMEOUT);
            let cancellation = CancellationToken::from_atomic(cancelled.clone());
            let outcome =
                run_cancellable_stage(c.name(), c.execution(), timeout, cancellation, |context| {
                    c.collect(event, task, &mut data, context)
                });
            let status = plugin_status(&outcome);
            diagnostics.record(c.name(), status, start.elapsed());
        }

        let raw_shm = if cancelled.load(Ordering::Acquire) {
            None
        } else {
            self.shm.as_ref().map(|shm| RawShmSnapshot {
                breadcrumbs: shm.raw_breadcrumb_bytes().to_vec(),
                context: shm.raw_context_bytes().to_vec(),
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
    ) -> CapturePayload {
        self.collect_snapshot(event, task, cancelled)
    }

    pub(super) fn finalize_captured_for_worker(&self, captured: CapturedEvent) -> Diagnostics {
        self.finalize_captured(captured)
    }

    /// Finalize owned capture data without a task port or live SHM view.
    #[allow(clippy::too_many_lines)]
    fn finalize_captured(&self, mut captured: CapturedEvent) -> Diagnostics {
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
            if !deps_satisfied(pp.depends_on(), diagnostics) {
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
        self.finalize_termination_event(event)
    }

    pub(super) fn finalize_termination_event_for_worker(&self, event: &CrashEvent) -> Diagnostics {
        self.finalize_termination_event(event)
    }

    #[allow(clippy::too_many_lines)]
    fn finalize_termination_event(&self, event: &CrashEvent) -> Diagnostics {
        debug_assert!(event.termination.is_some());
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

    /// Validate that plugin registration order respects `depends_on`.
    pub fn validate_dependencies(&self) {
        validate_plugin_order("Collector", &self.collectors);
        validate_plugin_order("PreProcessor", &self.pre_processors);
        validate_plugin_order_soft("PostProcessor", &self.post_processors);
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

/// Validate that plugins within a category are registered in dependency order.
///
/// # Panics
/// Panics if a plugin depends on another that is not registered or is registered after it.
pub fn validate_plugin_order<T: Plugin + ?Sized>(category: &str, plugins: &[Box<T>]) {
    validate_order_impl(category, plugins, true);
}

/// Like `validate_plugin_order`, but missing dependencies are warnings, not panics.
/// Used for `PostProcessor`s where plugins may be conditionally registered
/// (e.g., `FeedbackPostProcessor` requires the dialog binary to exist).
pub fn validate_plugin_order_soft<T: Plugin + ?Sized>(category: &str, plugins: &[Box<T>]) {
    validate_order_impl(category, plugins, false);
}

fn validate_order_impl<T: Plugin + ?Sized>(category: &str, plugins: &[Box<T>], strict: bool) {
    let names: Vec<&str> = plugins.iter().map(|p| p.name()).collect();
    for (i, p) in plugins.iter().enumerate() {
        for dep in p.depends_on() {
            match names.iter().position(|&n| n == *dep) {
                None if strict => panic!(
                    "{category} '{}' depends on '{dep}' which is not registered",
                    p.name()
                ),
                Some(j) if j >= i => panic!(
                    "{category} '{}' depends on '{dep}' which is registered after it",
                    p.name()
                ),
                // None (soft mode): dep not registered, acceptable for optional plugins.
                // Some(j < i): dep registered before us, valid order.
                _ => {}
            }
        }
    }
}

// ═══════════════════════════════════════════════════
//  Pipeline factory
// ═══════════════════════════════════════════════════

#[cfg(target_os = "macos")]
#[must_use]
#[allow(clippy::too_many_lines)] // pipeline factory — splitting would scatter registration logic
pub fn default_macos_pipeline(shm: Option<std::sync::Arc<crate::shm::SharedMemory>>) -> Pipeline {
    use crate::collectors::{
        DylibCollector, EnvironmentCollector, MemoryCollector, ThreadCollector,
    };
    use crate::config;
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

    let cfg = config::load_config();

    // ── Early out: global kill switch ──
    if !cfg.enabled {
        let platform: Arc<dyn PlatformOps> = Arc::new(MacOsPlatform);
        return Pipeline {
            filters: vec![],
            collectors: vec![],
            shm,
            platform,
            pre_processors: vec![],
            post_processors: vec![],
            notifiers: vec![],
            output_dir: None,
        };
    }

    let platform: Arc<dyn PlatformOps> = Arc::new(MacOsPlatform);

    // After the early-out above, cfg.enabled is guaranteed true.
    // Use 2-arg check: category_enabled && plugin_enabled.
    let on = |cat: bool, plugin: bool| cat && plugin;

    // ── Filters ──
    let mut filters: Vec<Box<dyn Filter>> = vec![];
    if on(cfg.filters.enabled, cfg.filters.disk_space.enabled) {
        filters.push(Box::new(DiskSpaceFilter::new(
            cfg.filters.disk_space.min_free_mb,
        )));
    }
    if on(cfg.filters.enabled, cfg.filters.rate_limiter.enabled) {
        filters.push(Box::new(RateLimiter::new(
            cfg.filters.rate_limiter.max_events,
            Duration::from_secs(cfg.filters.rate_limiter.window_secs),
        )));
    }

    // ── Collectors ──
    let mut collectors: Vec<Box<dyn Collector>> = vec![];
    let mut attachment_copy_enabled = false;

    if on(cfg.collectors.enabled, cfg.collectors.thread.enabled) {
        collectors.push(Box::new(ThreadCollector::new(platform.clone())));
    }

    if let Some(ref shm) = shm {
        use crate::collectors::{BreadcrumbCollector, ContextCollector};
        if on(cfg.collectors.enabled, cfg.collectors.breadcrumb.enabled) {
            collectors.push(Box::new(BreadcrumbCollector::new(shm.clone())));
        }
        if on(cfg.collectors.enabled, cfg.collectors.context.enabled) {
            collectors.push(Box::new(ContextCollector::new(shm.clone())));
        }
    }

    if on(cfg.collectors.enabled, cfg.collectors.memory.enabled) {
        collectors.push(Box::new(MemoryCollector::new(platform.clone())));
    }
    if on(cfg.collectors.enabled, cfg.collectors.dylib.enabled) {
        collectors.push(Box::new(DylibCollector::new(platform.clone())));
    }

    if let Some(ref shm) = shm {
        use crate::collectors::{AttachmentCollector, ScreenshotCollector};
        if on(cfg.collectors.enabled, cfg.collectors.screenshot.enabled) {
            collectors.push(Box::new(ScreenshotCollector::new(shm.clone())));
        }
        if on(cfg.collectors.enabled, cfg.collectors.attachment.enabled) {
            collectors.push(Box::new(AttachmentCollector::new(shm.clone())));
            attachment_copy_enabled = true;
        }
    }

    if on(cfg.collectors.enabled, cfg.collectors.environment.enabled) {
        collectors.push(Box::new(EnvironmentCollector::new()));
    }

    // ── Pre-processors (order matters: dependencies must come first) ──
    let mut pre_processors: Vec<Box<dyn PreProcessor>> = vec![];

    if attachment_copy_enabled {
        pre_processors.push(Box::new(crate::collectors::AttachmentCopier::new()));
    }

    if on(
        cfg.pre_processors.enabled,
        cfg.pre_processors.session.enabled,
    ) {
        pre_processors.push(Box::new(SessionEnricher));
    }
    if on(
        cfg.pre_processors.enabled,
        cfg.pre_processors.symbolizer.enabled,
    ) {
        pre_processors.push(Box::new(SymbolResolver::new()));
    }
    if on(
        cfg.pre_processors.enabled,
        cfg.pre_processors.fingerprint.enabled,
    ) {
        pre_processors.push(Box::new(Fingerprinter::new(
            cfg.pre_processors.fingerprint.top_frames,
        )));
    }
    if on(
        cfg.pre_processors.enabled,
        cfg.pre_processors.build_info.enabled,
    ) {
        pre_processors.push(Box::new(BuildInfoEnricher));
    }
    if on(
        cfg.pre_processors.enabled,
        cfg.pre_processors.duplicate.enabled,
    ) {
        pre_processors.push(Box::new(DuplicateDetector::new(Duration::from_secs(
            cfg.pre_processors.duplicate.window_secs,
        ))));
    }
    if on(
        cfg.pre_processors.enabled,
        cfg.pre_processors.sanitizer.enabled,
    ) {
        pre_processors.push(Box::new(Sanitizer::new()));
    }

    // ── Post-processors (order matters: RawCleanup → Session → Feedback → ZIP → LogRotator → Retention) ──
    let mut post_processors: Vec<Box<dyn PostProcessor>> = vec![];

    if on(
        cfg.post_processors.enabled,
        cfg.post_processors.raw_cleanup.enabled,
    ) {
        post_processors.push(Box::new(RawCleanup));
    }
    if on(
        cfg.post_processors.enabled,
        cfg.post_processors.session_recorder.enabled,
    ) {
        post_processors.push(Box::new(SessionRecorder));
    }
    // PNG conversion must run BEFORE FeedbackPostProcessor (which can block for
    // 5 minutes waiting on a user dialog) and BEFORE ZIPArchiver (so the zip
    // contains converted PNGs, not raw RGBA dumps).
    if on(
        cfg.post_processors.enabled,
        cfg.post_processors.png_converter.enabled,
    ) {
        post_processors.push(Box::new(PNGConverter));
    }

    // Feedback dialog: CRASH_MONITOR_DIALOG_BIN overrides the default path — used by
    // E2E tests to substitute the mock dialog (no UI, fixed stdout output).
    if on(
        cfg.post_processors.enabled,
        cfg.post_processors.feedback_dialog.enabled,
    ) {
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

    if on(
        cfg.post_processors.enabled,
        cfg.post_processors.zip_archiver.enabled,
    ) {
        post_processors.push(Box::new(ZIPArchiver));
    }
    // Relocate finished reports pending/ → sent/ before retention scans sent/.
    if on(
        cfg.post_processors.enabled,
        cfg.post_processors.move_to_sent.enabled,
    ) {
        post_processors.push(Box::new(MoveToSent::new()));
    }
    if on(
        cfg.post_processors.enabled,
        cfg.post_processors.log_rotator.enabled,
    ) {
        post_processors.push(Box::new(LogRotator::new(
            cfg.post_processors.log_rotator.max_size_mb,
        )));
    }
    if on(
        cfg.post_processors.enabled,
        cfg.post_processors.retention.enabled,
    ) {
        post_processors.push(Box::new(RetentionManager::new(
            cfg.post_processors.retention.max_reports,
            cfg.post_processors.retention.max_size_mb,
            cfg.post_processors.retention.max_age_days,
        )));
    }

    // ── Notifiers ──
    let mut notifiers: Vec<Box<dyn Notifier>> = vec![];

    if on(cfg.notifiers.enabled, cfg.notifiers.console.enabled) {
        notifiers.push(Box::new(ConsoleNotifier));
    }
    if on(
        cfg.notifiers.enabled,
        cfg.notifiers.system_notification.enabled,
    ) {
        notifiers.push(Box::new(SystemNotification::new(true)));
    }

    let pipeline = Pipeline {
        filters,
        collectors,
        shm,
        platform,
        pre_processors,
        post_processors,
        notifiers,
        output_dir: None,
    };
    pipeline.validate_dependencies();
    pipeline
}

#[cfg(test)]
#[path = "../../tests/unit/pipeline/mod_tests.rs"]
mod tests;
