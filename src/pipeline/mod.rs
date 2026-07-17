//! Plugin pipeline architecture for crash/snapshot report generation.
//!
//! Design: `docs/plans/crash_reporter.md` L1493-1867

pub mod report;
pub mod safety;
pub mod traits;
pub mod types;

use mach2::port::mach_port_t;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use crate::platform::PlatformOps;

pub use safety::run_plugin_safe;
pub use traits::{Collector, Filter, Notifier, Plugin, PostProcessor, PreProcessor};
pub use types::{
    CollectedData, CrashEvent, Diagnostics, PluginStatus, Priority, ReportResult, ReportType,
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

// Category-specific timeouts (seconds). `alarm()` has 1s granularity.
const FILTER_TIMEOUT: u32 = 1;
const COLLECTOR_TIMEOUT: u32 = 5;
const PREPROC_TIMEOUT: u32 = 2;
const POSTPROC_TIMEOUT: u32 = 30;
const NOTIFIER_TIMEOUT: u32 = 5;
const STAGE_TIMEOUT: u32 = 5;

impl Pipeline {
    /// Process a crash/snapshot event through the full pipeline.
    #[must_use]
    #[allow(clippy::too_many_lines)] // orchestration function — splitting would scatter pipeline logic
    pub fn handle_event(&self, event: &CrashEvent, task: mach_port_t) -> Diagnostics {
        let mut diagnostics = Diagnostics::new();

        let pending = match &self.output_dir {
            Some(dir) => dir.clone(),
            None => match crate::utils::paths::pending_dir() {
                Ok(d) => d,
                Err(e) => {
                    eprintln!("[monitor] Failed to get pending dir: {e}");
                    return diagnostics;
                }
            },
        };

        // ── Filter ──
        for f in &self.filters {
            let timeout = plugin_timeout(f.timeout_secs(), FILTER_TIMEOUT);
            let pass =
                run_plugin_safe(f.name(), timeout, || f.should_process(event)).unwrap_or(true);
            if !pass {
                return diagnostics;
            }
        }

        // ── Suspend ──
        if let Err(e) = self.platform.suspend_task(task) {
            if event.bail_on_suspend_failure {
                eprintln!("[monitor] {e}");
                return diagnostics;
            }
            eprintln!("[monitor] suspend_task failed (proceeding with best-effort): {e}");
        }

        // ── Collectors ──
        let mut data = CollectedData::default();
        for c in &self.collectors {
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
            let status =
                match run_plugin_safe(c.name(), timeout, || c.collect(event, task, &mut data)) {
                    Some(()) => PluginStatus::Ok,
                    None => PluginStatus::Error("failed or panicked".into()),
                };
            diagnostics.record(c.name(), status, start.elapsed());
        }

        // ── Thread port deallocation + Resume ──
        let thread_ports: Vec<u32> = data.raw.threads.iter().map(|t| t.thread_port).collect();
        let _port_guard = safety::PortGuard::new(thread_ports, self.platform.clone());
        let _ = self.platform.resume_task(task);

        // ── Pre-processors ──
        for pp in &self.pre_processors {
            if !pp.is_available() {
                diagnostics
                    .record_immediate(pp.name(), PluginStatus::Skipped("not available".into()));
                continue;
            }
            if !deps_satisfied(pp.depends_on(), &diagnostics) {
                diagnostics.record_immediate(
                    pp.name(),
                    PluginStatus::Skipped("dependency not met".into()),
                );
                continue;
            }
            let start = Instant::now();
            let timeout = plugin_timeout(pp.timeout_secs(), PREPROC_TIMEOUT);
            let status =
                match run_plugin_safe(pp.name(), timeout, || pp.process(event, task, &mut data)) {
                    Some(()) => PluginStatus::Ok,
                    None => PluginStatus::Error("failed or panicked".into()),
                };
            diagnostics.record(pp.name(), status, start.elapsed());
        }

        // ── Duplicate check (set by DuplicateDetector pre-processor) ──
        if data.duplicate_detected {
            eprintln!("[monitor] Duplicate event detected, skipping report");
            return diagnostics;
        }

        // ── Stage 1: Raw data (fail-safe) ──
        let raw_path: Option<PathBuf> = run_plugin_safe("Stage1Raw", STAGE_TIMEOUT, || {
            safety::write_raw_stage1(&pending, event.report_type, event.pid, &data.raw.threads)
        });

        // Stage 1 shm dump (breadcrumbs + context raw bytes)
        if let Some(shm) = &self.shm {
            let _ = run_plugin_safe("Stage1Shm", STAGE_TIMEOUT, || {
                safety::write_raw_shm_stage1(&pending, event.report_type, event.pid, shm)
            });
        }

        // ── Stage 2: Full JSON report + screenshot PNGs ──
        let screenshots = std::mem::take(&mut data.raw.screenshots);
        let json_path: Option<PathBuf> = run_plugin_safe("Stage2Json", STAGE_TIMEOUT, || {
            let mut crash_report = report::build_report(event, &data, &diagnostics);
            report::write_report(&pending, &mut crash_report, &screenshots)
        });

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
            let status =
                match run_plugin_safe(pp.name(), timeout, || pp.process(event, &mut result)) {
                    Some(()) => PluginStatus::Ok,
                    None => PluginStatus::Error("failed or panicked".into()),
                };
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
                let status = match run_plugin_safe(n.name(), timeout, || n.notify(path)) {
                    Some(()) => PluginStatus::Ok,
                    None => PluginStatus::Error("failed or panicked".into()),
                };
                diagnostics.record(n.name(), status, start.elapsed());
            }
        }

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
        }
    }

    if on(cfg.collectors.enabled, cfg.collectors.environment.enabled) {
        collectors.push(Box::new(EnvironmentCollector::new()));
    }

    // ── Pre-processors (order matters: dependencies must come first) ──
    let mut pre_processors: Vec<Box<dyn PreProcessor>> = vec![];

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
