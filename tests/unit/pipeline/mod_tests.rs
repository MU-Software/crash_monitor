use super::*;
use crate::collectors::thread::RawThreadData;
use crate::platform::mock::MockPlatform;
use crate::postprocessors::{MoveToSent, ZIPArchiver};
use mach2::port::mach_port_t;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

// ═══════════════════════════════════════════════════
//  Mock Collector
// ═══════════════════════════════════════════════════

struct MockCollector {
    dep: &'static [&'static str],
}

impl Plugin for MockCollector {
    fn name(&self) -> &'static str {
        "MockCollector"
    }
    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }
    fn priority(&self) -> Priority {
        Priority::Medium
    }
    fn hard_dependencies(&self) -> &'static [&'static str] {
        self.dep
    }
}

impl Collector for MockCollector {
    fn collect(
        &self,
        _event: &CrashEvent,
        _task: mach_port_t,
        _data: &mut CollectedData,
        _context: &PluginContext,
    ) -> Result<(), String> {
        Ok(())
    }
}

// ═══════════════════════════════════════════════════
//  Mock Filter
// ═══════════════════════════════════════════════════

struct MockFilter {
    allow: bool,
}

impl Plugin for MockFilter {
    fn name(&self) -> &'static str {
        "MockFilter"
    }
    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }
    fn priority(&self) -> Priority {
        Priority::Critical
    }
}

impl Filter for MockFilter {
    fn should_process(
        &self,
        _event: &CrashEvent,
        _context: &PluginContext,
    ) -> Result<bool, String> {
        Ok(self.allow)
    }
}

// ═══════════════════════════════════════════════════
//  Helpers
// ═══════════════════════════════════════════════════

fn make_event() -> CrashEvent {
    CrashEvent {
        report_type: ReportType::Snapshot,
        termination: None,
        exception_type: None,
        exception_code: None,
        exception_subcode: None,
        crashed_thread: None,
        bail_on_suspend_failure: false,
        pid: 1234,
        process_name: "test".into(),
        hang_duration_ms: None,
    }
}

#[test]
fn subprocess_execution_requires_supervisor_boundary() {
    let result = run_stage("BoundaryBypass", PluginExecution::Subprocess, 0, |_| Ok(()));

    assert!(matches!(
        result,
        PluginRunResult::Failed(error) if error.contains("did not use the subprocess supervisor")
    ));
}

#[test]
fn subprocess_execution_allows_explicit_noop() {
    let result = run_stage("BoundaryNoop", PluginExecution::Subprocess, 0, |context| {
        context.mark_subprocess_not_required();
        Ok(())
    });

    assert!(matches!(result, PluginRunResult::Completed(())));
}

#[test]
fn subprocess_adapter_cancellation_maps_to_pipeline_timeout_status() {
    let result = run_stage(
        "CancelledSubprocessAdapter",
        PluginExecution::Subprocess,
        0,
        |context| {
            let cancellation = context.cancellation_token();
            let canceller = std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(50));
                cancellation.cancel();
            });
            let mut command = std::process::Command::new("sleep");
            command.arg("30");
            let subprocess = run_plugin_subprocess("cancelled-adapter", &mut command, context);
            canceller
                .join()
                .map_err(|_| "cancellation thread panicked".to_string())?;
            match subprocess {
                PluginRunResult::TimedOut => context.checkpoint(),
                PluginRunResult::Completed(_) => {
                    Err("subprocess unexpectedly completed".to_string())
                }
                PluginRunResult::Failed(error) => Err(error),
                PluginRunResult::Panicked => Err("subprocess supervisor panicked".to_string()),
            }
        },
    );

    assert!(matches!(result, PluginRunResult::TimedOut));
    assert!(matches!(plugin_status(&result), PluginStatus::TimedOut));
}

// CRITICAL: Test pipelines must always set `output_dir` to a tempdir.
// If `output_dir` is None, `handle_event()` falls back to `~/.modelblockbuilder/crashes/pending/`
// (the user's real crash directory) and pollutes it with test fixture reports.
fn make_pipeline_with_collector(
    collector: Box<dyn Collector>,
    tempdir: &std::path::Path,
) -> Pipeline {
    Pipeline {
        enabled: true,
        triggers: TriggerPolicy::ALL_ENABLED,
        filters: vec![],
        collectors: vec![collector],
        pre_processors: vec![],
        post_processors: vec![],
        notifiers: vec![],
        shm: None,
        platform: Arc::new(MockPlatform::default()),
        output_dir: Some(tempdir.to_path_buf()),
    }
}

fn make_pipeline_with_filter(
    filter: Box<dyn Filter>,
    collector: Box<dyn Collector>,
    tempdir: &std::path::Path,
) -> Pipeline {
    Pipeline {
        enabled: true,
        triggers: TriggerPolicy::ALL_ENABLED,
        filters: vec![filter],
        collectors: vec![collector],
        pre_processors: vec![],
        post_processors: vec![],
        notifiers: vec![],
        shm: None,
        platform: Arc::new(MockPlatform::default()),
        output_dir: Some(tempdir.to_path_buf()),
    }
}

// ═══════════════════════════════════════════════════
//  Tests
// ═══════════════════════════════════════════════════

#[test]
fn test_handle_event_with_mock_collector() {
    let tempdir = tempfile::tempdir().unwrap();
    let pipeline =
        make_pipeline_with_collector(Box::new(MockCollector { dep: &[] }), tempdir.path());
    let event = make_event();
    let diag = pipeline.handle_event(&event, 0);

    assert!(
        diag.succeeded("MockCollector"),
        "MockCollector should be recorded as Ok"
    );
}

#[test]
fn test_expired_capture_deadline_is_diagnosed_as_timeout() {
    let tempdir = tempfile::tempdir().unwrap();
    let pipeline =
        make_pipeline_with_collector(Box::new(MockCollector { dep: &[] }), tempdir.path());
    let cancelled = Arc::new(AtomicBool::new(true));

    let payload = pipeline.collect_snapshot(&make_event(), 0, &cancelled);
    let status = payload
        .diagnostics
        .plugins
        .iter()
        .find(|entry| entry.name == "CaptureDeadline")
        .map(|entry| &entry.status);

    assert!(matches!(status, Some(PluginStatus::TimedOut)));
}

#[test]
fn test_filter_blocks_processing() {
    let tempdir = tempfile::tempdir().unwrap();
    let pipeline = make_pipeline_with_filter(
        Box::new(MockFilter { allow: false }),
        Box::new(MockCollector { dep: &[] }),
        tempdir.path(),
    );
    let event = make_event();
    let diag = pipeline.handle_event(&event, 0);

    // Filters are intentionally outside the live-task capture window. The
    // collector runs, then finalization stops before writing a report.
    assert!(
        diag.succeeded("MockCollector"),
        "capture must finish before a potentially blocking filter"
    );
    assert!(!json_report_exists(tempdir.path()));
}

#[test]
fn test_dependency_ordering_valid() {
    // A has no deps, B depends on A — valid order
    let plugins: Vec<Box<dyn Collector>> =
        vec![Box::new(MockCollector { dep: &[] }), Box::new(DepCollector)];
    // Should not panic
    validate_plugin_order(PluginCategory::Collector, &plugins).unwrap();
}

#[test]
fn test_dependency_ordering_invalid() {
    // B depends on A, but B is registered before A — invalid
    let plugins: Vec<Box<dyn Collector>> =
        vec![Box::new(DepCollector), Box::new(MockCollector { dep: &[] })];
    let error = validate_plugin_order(PluginCategory::Collector, &plugins).unwrap_err();
    assert!(matches!(
        error,
        crate::config::ConfigValidationError::InvalidDependencyOrder {
            category: PluginCategory::Collector,
            ref plugin_id,
            ref dependency,
            kind: DependencyKind::Hard,
        } if plugin_id == "DepCollector" && dependency == "MockCollector"
    ));
}

// A collector that depends on MockCollector
struct DepCollector;

impl Plugin for DepCollector {
    fn name(&self) -> &'static str {
        "DepCollector"
    }
    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }
    fn priority(&self) -> Priority {
        Priority::Low
    }
    fn hard_dependencies(&self) -> &'static [&'static str] {
        &["MockCollector"]
    }
}

impl Collector for DepCollector {
    fn collect(
        &self,
        _event: &CrashEvent,
        _task: mach_port_t,
        _data: &mut CollectedData,
        _context: &PluginContext,
    ) -> Result<(), String> {
        Ok(())
    }
}

// ═══════════════════════════════════════════════════
//  Phase B.2: Full-flow test infrastructure
// ═══════════════════════════════════════════════════

/// Collector that records it was called and injects fake thread data.
struct TrackingCollector {
    called: AtomicBool,
}

impl TrackingCollector {
    fn new() -> Self {
        Self {
            called: AtomicBool::new(false),
        }
    }
}

impl Plugin for TrackingCollector {
    fn name(&self) -> &'static str {
        "TrackingCollector"
    }
    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }
    fn priority(&self) -> Priority {
        Priority::Critical
    }
}

impl Collector for TrackingCollector {
    fn collect(
        &self,
        _event: &CrashEvent,
        _task: mach_port_t,
        data: &mut CollectedData,
        _context: &PluginContext,
    ) -> Result<(), String> {
        self.called.store(true, Ordering::SeqCst);
        // Inject a fake thread with port=42 so PortGuard can deallocate it
        let mut regs = BTreeMap::new();
        regs.insert("pc".into(), 0xDEAD_BEEF);
        regs.insert("sp".into(), 0x1000);
        regs.insert("fp".into(), 0x2000);
        data.raw.threads.push(RawThreadData {
            thread_port: 42,
            name: Some("main".into()),
            crashed: true,
            registers: Some(regs),
            backtrace: vec![0xAAAA, 0xBBBB],
            stack_capture: None,
        });
        Ok(())
    }
}

/// Collector that always fails.
struct FailingCollector;

impl Plugin for FailingCollector {
    fn name(&self) -> &'static str {
        "FailingCollector"
    }
    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }
    fn priority(&self) -> Priority {
        Priority::Medium
    }
}

impl Collector for FailingCollector {
    fn collect(
        &self,
        _event: &CrashEvent,
        _task: mach_port_t,
        _data: &mut CollectedData,
        _context: &PluginContext,
    ) -> Result<(), String> {
        Err("intentional failure".into())
    }
}

/// Collector that depends on `FailingCollector`.
struct DependentOnFailCollector;

impl Plugin for DependentOnFailCollector {
    fn name(&self) -> &'static str {
        "DependentOnFailCollector"
    }
    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }
    fn priority(&self) -> Priority {
        Priority::Low
    }
    fn hard_dependencies(&self) -> &'static [&'static str] {
        &["FailingCollector"]
    }
}

impl Collector for DependentOnFailCollector {
    fn collect(
        &self,
        _event: &CrashEvent,
        _task: mach_port_t,
        _data: &mut CollectedData,
        _context: &PluginContext,
    ) -> Result<(), String> {
        Ok(())
    }
}

fn make_crash_event() -> CrashEvent {
    CrashEvent {
        report_type: ReportType::Crash,
        termination: None,
        exception_type: Some(1),
        exception_code: Some(0xDEAD),
        exception_subcode: Some(0xBEEF),
        crashed_thread: Some(42),
        bail_on_suspend_failure: false,
        pid: 9999,
        process_name: "test_app".into(),
        hang_duration_ms: None,
    }
}

fn make_pipeline_with_tempdir(
    collectors: Vec<Box<dyn Collector>>,
    platform: Arc<MockPlatform>,
    tempdir: &std::path::Path,
) -> Pipeline {
    Pipeline {
        enabled: true,
        triggers: TriggerPolicy::ALL_ENABLED,
        filters: vec![],
        collectors,
        pre_processors: vec![],
        post_processors: vec![],
        notifiers: vec![],
        shm: None,
        platform,
        output_dir: Some(tempdir.to_path_buf()),
    }
}

// ═══════════════════════════════════════════════════
//  Phase B.2: Full-flow tests
// ═══════════════════════════════════════════════════

#[test]
fn test_full_crash_flow_writes_files() {
    let tempdir = tempfile::tempdir().unwrap();
    let platform = Arc::new(MockPlatform::default());

    let pipeline = make_pipeline_with_tempdir(
        vec![Box::new(TrackingCollector::new())],
        platform.clone(),
        tempdir.path(),
    );

    let event = make_crash_event();
    let diag = pipeline.handle_event(&event, 123);

    // Verify collector ran
    assert!(diag.succeeded("TrackingCollector"));
    assert_eq!(platform.suspend_count(), 1);
    assert_eq!(platform.resume_count(), 1);

    // Verify files were written
    let files: Vec<_> = std::fs::read_dir(tempdir.path())
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    assert!(
        files.len() >= 2,
        "Expected raw + json files, got {}",
        files.len()
    );

    // Check for JSON report
    let json_file = files
        .iter()
        .find(|f| f.path().extension().is_some_and(|e| e == "json"));
    assert!(json_file.is_some(), "Should have a JSON report file");

    // Check for raw file
    let raw_file = files.iter().find(|f| {
        f.file_name()
            .to_str()
            .is_some_and(|n| n.contains("_raw.bin"))
    });
    assert!(raw_file.is_some(), "Should have a raw.bin file");
}

#[test]
fn test_bail_on_suspend_failure_true() {
    let tempdir = tempfile::tempdir().unwrap();
    let mut platform = MockPlatform::default();
    platform.suspend_fails = true;
    let platform = Arc::new(platform);

    let pipeline = make_pipeline_with_tempdir(
        vec![Box::new(MockCollector { dep: &[] })],
        platform.clone(),
        tempdir.path(),
    );

    let mut event = make_crash_event();
    event.bail_on_suspend_failure = true;

    let diag = pipeline.handle_event(&event, 0);

    // Pipeline bailed — no collectors should have run
    assert!(
        diag.plugins.is_empty(),
        "No plugins should run when bail=true and suspend fails"
    );

    // No files written
    let files: Vec<_> = std::fs::read_dir(tempdir.path())
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    assert!(
        files.is_empty(),
        "No files should be written when pipeline bails"
    );
}

#[test]
fn test_proceed_on_suspend_failure_false() {
    let tempdir = tempfile::tempdir().unwrap();
    let mut platform = MockPlatform::default();
    platform.suspend_fails = true;
    let platform = Arc::new(platform);

    let pipeline = make_pipeline_with_tempdir(
        vec![Box::new(MockCollector { dep: &[] })],
        platform.clone(),
        tempdir.path(),
    );

    let mut event = make_crash_event();
    event.bail_on_suspend_failure = false;

    let diag = pipeline.handle_event(&event, 0);

    // Collector should still run despite suspend failure
    assert!(diag.succeeded("MockCollector"));
    // Never decrement a suspend count that this monitor did not acquire.
    assert_eq!(platform.resume_count(), 0);
}

#[test]
fn test_port_deallocation_via_port_guard() {
    let tempdir = tempfile::tempdir().unwrap();
    let platform = Arc::new(MockPlatform::default());

    let pipeline = make_pipeline_with_tempdir(
        vec![Box::new(TrackingCollector::new())],
        platform.clone(),
        tempdir.path(),
    );

    let event = make_crash_event();
    let _diag = pipeline.handle_event(&event, 0);

    // TrackingCollector pushes thread with port=42
    let ports = platform.deallocated_ports();
    assert!(
        ports.contains(&42),
        "Port 42 should be deallocated by PortGuard, got: {ports:?}"
    );
}

#[test]
fn test_dependency_skip_on_failure() {
    let tempdir = tempfile::tempdir().unwrap();
    let platform = Arc::new(MockPlatform::default());

    let pipeline = make_pipeline_with_tempdir(
        vec![
            Box::new(FailingCollector),
            Box::new(DependentOnFailCollector),
        ],
        platform,
        tempdir.path(),
    );

    let event = make_crash_event();
    let diag = pipeline.handle_event(&event, 0);

    // FailingCollector should have error status
    assert!(!diag.succeeded("FailingCollector"));
    // DependentOnFailCollector should be skipped due to dependency not met
    let dep_status = diag
        .plugins
        .iter()
        .find(|p| p.name == "DependentOnFailCollector");
    assert!(
        dep_status.is_some(),
        "DependentOnFailCollector should be in diagnostics"
    );
    match &dep_status.unwrap().status {
        PluginStatus::Skipped(reason) => assert!(
            reason.contains("dependency"),
            "Expected dependency skip, got: {reason}"
        ),
        other => panic!("Expected Skipped, got: {other:?}"),
    }
}

#[test]
fn test_stage1_raw_file_contents() {
    let tempdir = tempfile::tempdir().unwrap();
    let platform = Arc::new(MockPlatform::default());

    let pipeline = make_pipeline_with_tempdir(
        vec![Box::new(TrackingCollector::new())],
        platform,
        tempdir.path(),
    );

    let event = make_crash_event();
    let _diag = pipeline.handle_event(&event, 0);

    // Find the raw.bin file
    let raw_file = std::fs::read_dir(tempdir.path())
        .unwrap()
        .filter_map(Result::ok)
        .find(|f| {
            f.file_name()
                .to_str()
                .is_some_and(|n| n.contains("_raw.bin"))
        })
        .expect("raw.bin file should exist");

    let contents = std::fs::read_to_string(raw_file.path()).unwrap();
    assert!(
        contents.contains("---thread 0"),
        "Should contain thread header"
    );
    assert!(contents.contains("port=42"), "Should contain port=42");
    assert!(
        contents.contains("crashed=true"),
        "Should contain crashed=true"
    );
    assert!(contents.contains("pc="), "Should contain register data");
    assert!(
        contents.contains("---backtrace---"),
        "Should contain backtrace header"
    );
}

#[test]
fn test_anr_event_produces_report() {
    let tempdir = tempfile::tempdir().unwrap();
    let platform = Arc::new(MockPlatform::default());

    let pipeline = make_pipeline_with_tempdir(
        vec![Box::new(MockCollector { dep: &[] })],
        platform,
        tempdir.path(),
    );

    let event = CrashEvent {
        report_type: ReportType::Anr,
        termination: None,
        exception_type: None,
        exception_code: None,
        exception_subcode: None,
        crashed_thread: None,
        bail_on_suspend_failure: false,
        pid: 9999,
        process_name: "test_app".into(),
        hang_duration_ms: Some(5000),
    };

    let _diag = pipeline.handle_event(&event, 0);

    // Find JSON file and verify report_type
    let json_file = std::fs::read_dir(tempdir.path())
        .unwrap()
        .filter_map(Result::ok)
        .find(|f| f.path().extension().is_some_and(|e| e == "json"))
        .expect("JSON report should exist");

    let json_str = std::fs::read_to_string(json_file.path()).unwrap();
    assert!(
        json_str.contains("\"type\": \"anr\""),
        "Report type should be anr, got: {json_str}"
    );
}

// ═══════════════════════════════════════════════════
//  Phase 9: Resilience — configurable mocks for every category
// ═══════════════════════════════════════════════════

/// Filter whose `should_process` itself errors. The pipeline treats a filter
/// error as "proceed" (`unwrap_or(true)`), so processing must continue.
struct ErroringFilter;

impl Plugin for ErroringFilter {
    fn name(&self) -> &'static str {
        "ErroringFilter"
    }
    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }
    fn priority(&self) -> Priority {
        Priority::Critical
    }
}

impl Filter for ErroringFilter {
    fn should_process(
        &self,
        _event: &CrashEvent,
        _context: &PluginContext,
    ) -> Result<bool, String> {
        Err("filter check blew up".into())
    }
}

/// Collector configurable by name/availability/panic. `called` is shared so a
/// test can assert whether it ran.
struct CfgCollector {
    name: &'static str,
    called: Arc<AtomicBool>,
    available: bool,
    panic: bool,
}

impl Plugin for CfgCollector {
    fn name(&self) -> &'static str {
        self.name
    }
    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }
    fn priority(&self) -> Priority {
        Priority::Medium
    }
    fn is_available(&self) -> bool {
        self.available
    }
}

impl Collector for CfgCollector {
    fn collect(
        &self,
        _event: &CrashEvent,
        _task: mach_port_t,
        _data: &mut CollectedData,
        _context: &PluginContext,
    ) -> Result<(), String> {
        self.called.store(true, Ordering::SeqCst);
        assert!(!self.panic, "intentional collector panic");
        Ok(())
    }
}

/// Pre-processor configurable by name/availability/panic, and able to flag the
/// event as a duplicate (which makes the pipeline skip report generation).
struct CfgPreProcessor {
    name: &'static str,
    called: Arc<AtomicBool>,
    available: bool,
    panic: bool,
    set_duplicate: bool,
}

impl Plugin for CfgPreProcessor {
    fn name(&self) -> &'static str {
        self.name
    }
    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }
    fn priority(&self) -> Priority {
        Priority::Medium
    }
    fn is_available(&self) -> bool {
        self.available
    }
}

impl PreProcessor for CfgPreProcessor {
    fn process(
        &self,
        _event: &CrashEvent,
        data: &mut CollectedData,
        _context: &PluginContext,
    ) -> Result<(), String> {
        self.called.store(true, Ordering::SeqCst);
        assert!(!self.panic, "intentional pre-processor panic");
        if self.set_duplicate {
            data.duplicate_detected = true;
        }
        Ok(())
    }
}

/// Post-processor configurable by name/availability/panic.
struct CfgPostProcessor {
    name: &'static str,
    called: Arc<AtomicBool>,
    available: bool,
    panic: bool,
}

impl Plugin for CfgPostProcessor {
    fn name(&self) -> &'static str {
        self.name
    }
    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }
    fn priority(&self) -> Priority {
        Priority::Low
    }
    fn is_available(&self) -> bool {
        self.available
    }
}

impl PostProcessor for CfgPostProcessor {
    fn process(
        &self,
        _event: &CrashEvent,
        _result: &mut ReportResult,
        _context: &PluginContext,
    ) -> Result<(), String> {
        self.called.store(true, Ordering::SeqCst);
        assert!(!self.panic, "intentional post-processor panic");
        Ok(())
    }
}

struct TimingOutPostProcessor;

impl Plugin for TimingOutPostProcessor {
    fn name(&self) -> &'static str {
        "TimingOutPost"
    }

    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }

    fn priority(&self) -> Priority {
        Priority::Low
    }
}

impl PostProcessor for TimingOutPostProcessor {
    fn process(
        &self,
        _event: &CrashEvent,
        _result: &mut ReportResult,
        context: &PluginContext,
    ) -> Result<(), String> {
        context.cancellation_token().cancel();
        Err("deadline reached".to_string())
    }
}

/// Notifier configurable by name/availability; captures the report path it saw.
struct CfgNotifier {
    name: &'static str,
    available: bool,
    captured: Arc<Mutex<Option<PathBuf>>>,
}

impl Plugin for CfgNotifier {
    fn name(&self) -> &'static str {
        self.name
    }
    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }
    fn priority(&self) -> Priority {
        Priority::Low
    }
    fn is_available(&self) -> bool {
        self.available
    }
}

impl Notifier for CfgNotifier {
    fn notify(&self, report_path: &Path, _context: &PluginContext) -> Result<(), String> {
        *self.captured.lock().unwrap() = Some(report_path.to_path_buf());
        Ok(())
    }
}

struct DependencyBehavior {
    name: &'static str,
    hard: &'static [&'static str],
    order: &'static [&'static str],
    fail: bool,
    available: bool,
}

impl DependencyBehavior {
    fn run(&self) -> Result<(), String> {
        if self.fail {
            Err(format!("{} failed", self.name))
        } else {
            Ok(())
        }
    }
}

macro_rules! dependency_plugin_metadata {
    ($plugin:ty) => {
        impl Plugin for $plugin {
            fn name(&self) -> &'static str {
                self.0.name
            }
            fn execution(&self) -> PluginExecution {
                PluginExecution::Cooperative
            }
            fn priority(&self) -> Priority {
                Priority::Medium
            }
            fn hard_dependencies(&self) -> &'static [&'static str] {
                self.0.hard
            }
            fn order_after(&self) -> &'static [&'static str] {
                self.0.order
            }
            fn is_available(&self) -> bool {
                self.0.available
            }
        }
    };
}

struct DependencyFilter(DependencyBehavior);
dependency_plugin_metadata!(DependencyFilter);
impl Filter for DependencyFilter {
    fn should_process(
        &self,
        _event: &CrashEvent,
        _context: &PluginContext,
    ) -> Result<bool, String> {
        self.0.run().map(|()| true)
    }
}

struct DependencyCollector(DependencyBehavior);
dependency_plugin_metadata!(DependencyCollector);
impl Collector for DependencyCollector {
    fn collect(
        &self,
        _event: &CrashEvent,
        _task: mach_port_t,
        _data: &mut CollectedData,
        _context: &PluginContext,
    ) -> Result<(), String> {
        self.0.run()
    }
}

struct DependencyPreProcessor(DependencyBehavior);
dependency_plugin_metadata!(DependencyPreProcessor);
impl PreProcessor for DependencyPreProcessor {
    fn process(
        &self,
        _event: &CrashEvent,
        _data: &mut CollectedData,
        _context: &PluginContext,
    ) -> Result<(), String> {
        self.0.run()
    }
}

struct DependencyPostProcessor(DependencyBehavior);
dependency_plugin_metadata!(DependencyPostProcessor);
impl PostProcessor for DependencyPostProcessor {
    fn process(
        &self,
        _event: &CrashEvent,
        _result: &mut ReportResult,
        _context: &PluginContext,
    ) -> Result<(), String> {
        self.0.run()
    }
}

struct DependencyNotifier(DependencyBehavior);
dependency_plugin_metadata!(DependencyNotifier);
impl Notifier for DependencyNotifier {
    fn notify(&self, _report_path: &Path, _context: &PluginContext) -> Result<(), String> {
        self.0.run()
    }
}

fn dependency_behavior(
    name: &'static str,
    hard: &'static [&'static str],
    order: &'static [&'static str],
    fail: bool,
) -> DependencyBehavior {
    DependencyBehavior {
        name,
        hard,
        order,
        fail,
        available: true,
    }
}

#[allow(clippy::too_many_arguments)]
fn make_full_pipeline(
    filters: Vec<Box<dyn Filter>>,
    collectors: Vec<Box<dyn Collector>>,
    pre_processors: Vec<Box<dyn PreProcessor>>,
    post_processors: Vec<Box<dyn PostProcessor>>,
    notifiers: Vec<Box<dyn Notifier>>,
    tempdir: &std::path::Path,
) -> Pipeline {
    Pipeline {
        enabled: true,
        triggers: TriggerPolicy::ALL_ENABLED,
        filters,
        collectors,
        pre_processors,
        post_processors,
        notifiers,
        shm: None,
        platform: Arc::new(MockPlatform::default()),
        output_dir: Some(tempdir.to_path_buf()),
    }
}

fn json_report_exists(dir: &std::path::Path) -> bool {
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .any(|f| f.path().extension().is_some_and(|e| e == "json"))
}

fn assert_dependency_skipped(diagnostics: &Diagnostics, plugin_name: &str) {
    let status = diagnostics
        .plugins
        .iter()
        .find(|plugin| plugin.name == plugin_name)
        .map(|plugin| &plugin.status);
    assert!(
        matches!(status, Some(PluginStatus::Skipped(reason)) if reason.contains("dependency")),
        "{plugin_name} was not dependency-skipped"
    );
}

#[test]
#[allow(clippy::too_many_lines)] // one explicit provider/dependent trio per pipeline stage
fn test_live_pipeline_applies_hard_and_order_only_failure_contract_to_all_five_stages() {
    let tempdir = tempfile::tempdir().unwrap();
    let pipeline = make_full_pipeline(
        vec![
            Box::new(DependencyFilter(dependency_behavior(
                "FilterProvider",
                &[],
                &[],
                true,
            ))),
            Box::new(DependencyFilter(dependency_behavior(
                "FilterHard",
                &["FilterProvider"],
                &[],
                false,
            ))),
            Box::new(DependencyFilter(dependency_behavior(
                "FilterOrder",
                &[],
                &["FilterProvider"],
                false,
            ))),
        ],
        vec![
            Box::new(DependencyCollector(dependency_behavior(
                "CollectorProvider",
                &[],
                &[],
                true,
            ))),
            Box::new(DependencyCollector(dependency_behavior(
                "CollectorHard",
                &["CollectorProvider"],
                &[],
                false,
            ))),
            Box::new(DependencyCollector(dependency_behavior(
                "CollectorOrder",
                &[],
                &["CollectorProvider"],
                false,
            ))),
        ],
        vec![
            Box::new(DependencyPreProcessor(dependency_behavior(
                "PreProvider",
                &[],
                &[],
                true,
            ))),
            Box::new(DependencyPreProcessor(dependency_behavior(
                "PreHard",
                &["PreProvider"],
                &[],
                false,
            ))),
            Box::new(DependencyPreProcessor(dependency_behavior(
                "PreOrder",
                &[],
                &["PreProvider"],
                false,
            ))),
        ],
        vec![
            Box::new(DependencyPostProcessor(dependency_behavior(
                "PostProvider",
                &[],
                &[],
                true,
            ))),
            Box::new(DependencyPostProcessor(dependency_behavior(
                "PostHard",
                &["PostProvider"],
                &[],
                false,
            ))),
            Box::new(DependencyPostProcessor(dependency_behavior(
                "PostOrder",
                &[],
                &["PostProvider"],
                false,
            ))),
        ],
        vec![
            Box::new(DependencyNotifier(dependency_behavior(
                "NotifierProvider",
                &[],
                &[],
                true,
            ))),
            Box::new(DependencyNotifier(dependency_behavior(
                "NotifierHard",
                &["NotifierProvider"],
                &[],
                false,
            ))),
            Box::new(DependencyNotifier(dependency_behavior(
                "NotifierOrder",
                &[],
                &["NotifierProvider"],
                false,
            ))),
        ],
        tempdir.path(),
    );
    pipeline.validate_dependencies().unwrap();

    let diagnostics = pipeline.handle_event(&make_event(), 0);
    for hard in [
        "FilterHard",
        "CollectorHard",
        "PreHard",
        "PostHard",
        "NotifierHard",
    ] {
        assert_dependency_skipped(&diagnostics, hard);
    }
    for order_only in [
        "FilterOrder",
        "CollectorOrder",
        "PreOrder",
        "PostOrder",
        "NotifierOrder",
    ] {
        assert!(
            diagnostics.succeeded(order_only),
            "order-only dependent {order_only} did not run after provider failure"
        );
    }
}

#[test]
fn test_termination_pipeline_applies_hard_and_order_only_contract_to_three_live_stages() {
    let tempdir = tempfile::tempdir().unwrap();
    let pipeline = make_full_pipeline(
        vec![
            Box::new(DependencyFilter(dependency_behavior(
                "TermFilterProvider",
                &[],
                &[],
                true,
            ))),
            Box::new(DependencyFilter(dependency_behavior(
                "TermFilterHard",
                &["TermFilterProvider"],
                &[],
                false,
            ))),
            Box::new(DependencyFilter(dependency_behavior(
                "TermFilterOrder",
                &[],
                &["TermFilterProvider"],
                false,
            ))),
        ],
        vec![],
        vec![],
        vec![
            Box::new(DependencyPostProcessor(dependency_behavior(
                "TermPostProvider",
                &[],
                &[],
                true,
            ))),
            Box::new(DependencyPostProcessor(dependency_behavior(
                "TermPostHard",
                &["TermPostProvider"],
                &[],
                false,
            ))),
            Box::new(DependencyPostProcessor(dependency_behavior(
                "TermPostOrder",
                &[],
                &["TermPostProvider"],
                false,
            ))),
        ],
        vec![
            Box::new(DependencyNotifier(dependency_behavior(
                "TermNotifierProvider",
                &[],
                &[],
                true,
            ))),
            Box::new(DependencyNotifier(dependency_behavior(
                "TermNotifierHard",
                &["TermNotifierProvider"],
                &[],
                false,
            ))),
            Box::new(DependencyNotifier(dependency_behavior(
                "TermNotifierOrder",
                &[],
                &["TermNotifierProvider"],
                false,
            ))),
        ],
        tempdir.path(),
    );
    pipeline.validate_dependencies().unwrap();
    let event = CrashEvent {
        report_type: ReportType::ExitFailure,
        termination: Some(TerminationReason::Exited {
            exit_code: 7,
            runtime_ms: 10,
        }),
        exception_type: None,
        exception_code: None,
        exception_subcode: None,
        crashed_thread: None,
        bail_on_suspend_failure: false,
        pid: 1234,
        process_name: "terminated-test".into(),
        hang_duration_ms: None,
    };

    let diagnostics = pipeline.handle_termination_event(&event);
    for hard in ["TermFilterHard", "TermPostHard", "TermNotifierHard"] {
        assert_dependency_skipped(&diagnostics, hard);
    }
    for order_only in ["TermFilterOrder", "TermPostOrder", "TermNotifierOrder"] {
        assert!(diagnostics.succeeded(order_only));
    }
}

#[test]
fn test_unavailable_filter_hard_provider_skips_dependent_in_live_and_termination_paths() {
    let tempdir = tempfile::tempdir().unwrap();
    let pipeline = make_full_pipeline(
        vec![
            Box::new(DependencyFilter(DependencyBehavior {
                name: "UnavailableFilterProvider",
                hard: &[],
                order: &[],
                fail: false,
                available: false,
            })),
            Box::new(DependencyFilter(dependency_behavior(
                "UnavailableFilterDependent",
                &["UnavailableFilterProvider"],
                &[],
                false,
            ))),
        ],
        vec![],
        vec![],
        vec![],
        vec![],
        tempdir.path(),
    );
    pipeline.validate_dependencies().unwrap();

    let live = pipeline.handle_event(&make_event(), 0);
    assert_dependency_skipped(&live, "UnavailableFilterDependent");
    assert!(matches!(
        live.plugins
            .iter()
            .find(|plugin| plugin.name == "UnavailableFilterProvider")
            .map(|plugin| &plugin.status),
        Some(PluginStatus::Skipped(reason)) if reason == "not available"
    ));

    let termination_event = CrashEvent {
        report_type: ReportType::ExitFailure,
        termination: Some(TerminationReason::Exited {
            exit_code: 9,
            runtime_ms: 20,
        }),
        exception_type: None,
        exception_code: None,
        exception_subcode: None,
        crashed_thread: None,
        bail_on_suspend_failure: false,
        pid: 2345,
        process_name: "unavailable-filter-test".into(),
        hang_duration_ms: None,
    };
    let termination = pipeline.handle_termination_event(&termination_event);
    assert_dependency_skipped(&termination, "UnavailableFilterDependent");
    assert!(matches!(
        termination
            .plugins
            .iter()
            .find(|plugin| plugin.name == "UnavailableFilterProvider")
            .map(|plugin| &plugin.status),
        Some(PluginStatus::Skipped(reason)) if reason == "not available"
    ));
}

// ── Panic isolation (the core stability guarantee) ──

#[test]
fn test_collector_panic_is_isolated() {
    let tempdir = tempfile::tempdir().unwrap();
    let survived = Arc::new(AtomicBool::new(false));
    let pipeline = make_full_pipeline(
        vec![],
        vec![
            Box::new(CfgCollector {
                name: "PanicCollector",
                called: Arc::new(AtomicBool::new(false)),
                available: true,
                panic: true,
            }),
            Box::new(CfgCollector {
                name: "SurvivorCollector",
                called: survived.clone(),
                available: true,
                panic: false,
            }),
        ],
        vec![],
        vec![],
        vec![],
        tempdir.path(),
    );

    let diag = pipeline.handle_event(&make_crash_event(), 0);

    // The panicking collector is recorded as an error, not a crash of the monitor.
    assert!(!diag.succeeded("PanicCollector"));
    // A collector registered after the panic still runs.
    assert!(survived.load(Ordering::SeqCst), "pipeline halted on panic");
    assert!(diag.succeeded("SurvivorCollector"));
    // A report is still produced despite the panic.
    assert!(json_report_exists(tempdir.path()));
}

#[test]
fn test_preprocessor_panic_is_isolated() {
    let tempdir = tempfile::tempdir().unwrap();
    let survived = Arc::new(AtomicBool::new(false));
    let pipeline = make_full_pipeline(
        vec![],
        vec![Box::new(TrackingCollector::new())],
        vec![
            Box::new(CfgPreProcessor {
                name: "PanicPre",
                called: Arc::new(AtomicBool::new(false)),
                available: true,
                panic: true,
                set_duplicate: false,
            }),
            Box::new(CfgPreProcessor {
                name: "SurvivorPre",
                called: survived.clone(),
                available: true,
                panic: false,
                set_duplicate: false,
            }),
        ],
        vec![],
        vec![],
        tempdir.path(),
    );

    let diag = pipeline.handle_event(&make_crash_event(), 0);

    assert!(!diag.succeeded("PanicPre"));
    assert!(survived.load(Ordering::SeqCst));
    assert!(json_report_exists(tempdir.path()));
}

#[test]
fn test_postprocessor_panic_is_isolated() {
    let tempdir = tempfile::tempdir().unwrap();
    let survived = Arc::new(AtomicBool::new(false));
    let pipeline = make_full_pipeline(
        vec![],
        vec![Box::new(TrackingCollector::new())],
        vec![],
        vec![
            Box::new(CfgPostProcessor {
                name: "PanicPost",
                called: Arc::new(AtomicBool::new(false)),
                available: true,
                panic: true,
            }),
            Box::new(CfgPostProcessor {
                name: "SurvivorPost",
                called: survived.clone(),
                available: true,
                panic: false,
            }),
        ],
        vec![],
        tempdir.path(),
    );

    let diag = pipeline.handle_event(&make_crash_event(), 0);

    assert!(!diag.succeeded("PanicPost"));
    assert!(survived.load(Ordering::SeqCst));
}

// ── Pre-processor loop paths ──

#[test]
fn test_preprocessor_runs_and_records() {
    let tempdir = tempfile::tempdir().unwrap();
    let called = Arc::new(AtomicBool::new(false));
    let pipeline = make_full_pipeline(
        vec![],
        vec![Box::new(TrackingCollector::new())],
        vec![Box::new(CfgPreProcessor {
            name: "RunPre",
            called: called.clone(),
            available: true,
            panic: false,
            set_duplicate: false,
        })],
        vec![],
        vec![],
        tempdir.path(),
    );

    let diag = pipeline.handle_event(&make_crash_event(), 0);

    assert!(called.load(Ordering::SeqCst));
    assert!(diag.succeeded("RunPre"));
}

#[test]
fn test_preprocessor_unavailable_is_skipped() {
    let tempdir = tempfile::tempdir().unwrap();
    let called = Arc::new(AtomicBool::new(false));
    let pipeline = make_full_pipeline(
        vec![],
        vec![Box::new(TrackingCollector::new())],
        vec![Box::new(CfgPreProcessor {
            name: "UnavailPre",
            called: called.clone(),
            available: false,
            panic: false,
            set_duplicate: false,
        })],
        vec![],
        vec![],
        tempdir.path(),
    );

    let diag = pipeline.handle_event(&make_crash_event(), 0);

    assert!(!called.load(Ordering::SeqCst), "unavailable pre ran");
    let status = diag.plugins.iter().find(|p| p.name == "UnavailPre");
    assert!(matches!(
        status.map(|p| &p.status),
        Some(PluginStatus::Skipped(_))
    ));
}

#[test]
fn test_duplicate_detected_skips_report() {
    let tempdir = tempfile::tempdir().unwrap();
    let pipeline = make_full_pipeline(
        vec![],
        vec![Box::new(TrackingCollector::new())],
        vec![Box::new(CfgPreProcessor {
            name: "DupPre",
            called: Arc::new(AtomicBool::new(false)),
            available: true,
            panic: false,
            set_duplicate: true,
        })],
        vec![],
        vec![],
        tempdir.path(),
    );

    let _ = pipeline.handle_event(&make_crash_event(), 0);

    // Duplicate → Stage 2 report generation is skipped, so no JSON is written.
    assert!(
        !json_report_exists(tempdir.path()),
        "duplicate event should not produce a JSON report"
    );
}

// ── Post-processor & notifier loop paths ──

#[test]
fn test_postprocessor_runs_and_records() {
    let tempdir = tempfile::tempdir().unwrap();
    let called = Arc::new(AtomicBool::new(false));
    let pipeline = make_full_pipeline(
        vec![],
        vec![Box::new(TrackingCollector::new())],
        vec![],
        vec![Box::new(CfgPostProcessor {
            name: "RunPost",
            called: called.clone(),
            available: true,
            panic: false,
        })],
        vec![],
        tempdir.path(),
    );

    let diag = pipeline.handle_event(&make_crash_event(), 0);

    assert!(called.load(Ordering::SeqCst));
    assert!(diag.succeeded("RunPost"));
}

#[test]
fn test_plugin_timeout_is_diagnosed_separately_from_error() {
    let tempdir = tempfile::tempdir().unwrap();
    let pipeline = make_full_pipeline(
        vec![],
        vec![],
        vec![],
        vec![Box::new(TimingOutPostProcessor)],
        vec![],
        tempdir.path(),
    );

    let diagnostics = pipeline.handle_event(&make_crash_event(), 0);
    let status = diagnostics
        .plugins
        .iter()
        .find(|entry| entry.name == "TimingOutPost")
        .map(|entry| &entry.status);

    assert!(matches!(status, Some(PluginStatus::TimedOut)));
}

#[test]
fn test_postprocessor_unavailable_is_skipped() {
    let tempdir = tempfile::tempdir().unwrap();
    let called = Arc::new(AtomicBool::new(false));
    let pipeline = make_full_pipeline(
        vec![],
        vec![Box::new(TrackingCollector::new())],
        vec![],
        vec![Box::new(CfgPostProcessor {
            name: "UnavailPost",
            called: called.clone(),
            available: false,
            panic: false,
        })],
        vec![],
        tempdir.path(),
    );

    let diag = pipeline.handle_event(&make_crash_event(), 0);

    assert!(!called.load(Ordering::SeqCst));
    let status = diag.plugins.iter().find(|p| p.name == "UnavailPost");
    assert!(matches!(
        status.map(|p| &p.status),
        Some(PluginStatus::Skipped(_))
    ));
}

#[test]
fn test_notifier_runs_with_report_path() {
    let tempdir = tempfile::tempdir().unwrap();
    let captured = Arc::new(Mutex::new(None));
    let pipeline = make_full_pipeline(
        vec![],
        vec![Box::new(TrackingCollector::new())],
        vec![],
        vec![],
        vec![Box::new(CfgNotifier {
            name: "RunNotifier",
            available: true,
            captured: captured.clone(),
        })],
        tempdir.path(),
    );

    let _ = pipeline.handle_event(&make_crash_event(), 0);

    let path = captured.lock().unwrap().clone();
    let path = path.expect("notifier should have received the report path");
    assert!(path.extension().is_some_and(|e| e == "json"));
}

#[test]
fn test_zip_move_and_notifier_share_exact_final_report_path() {
    let tempdir = tempfile::tempdir().unwrap();
    let pending = tempdir.path().join("pending");
    let sent = tempdir.path().join("sent");
    std::fs::create_dir_all(&pending).unwrap();
    let captured = Arc::new(Mutex::new(None));
    let pipeline = make_full_pipeline(
        vec![],
        vec![],
        vec![],
        vec![
            Box::new(ZIPArchiver),
            Box::new(MoveToSent::with_dir(sent.clone())),
        ],
        vec![Box::new(CfgNotifier {
            name: "FinalPathNotifier",
            available: true,
            captured: captured.clone(),
        })],
        &pending,
    );

    let diagnostics = pipeline.handle_event(&make_crash_event(), 0);

    let report_path = diagnostics
        .report_path
        .expect("pipeline should expose its final artifact");
    assert_eq!(captured.lock().unwrap().as_ref(), Some(&report_path));
    assert_eq!(report_path.parent(), Some(sent.as_path()));
    assert_eq!(
        report_path.extension().and_then(|ext| ext.to_str()),
        Some("zip")
    );
    assert!(report_path.exists());
}

#[test]
fn test_notifier_unavailable_is_skipped() {
    let tempdir = tempfile::tempdir().unwrap();
    let captured = Arc::new(Mutex::new(None));
    let pipeline = make_full_pipeline(
        vec![],
        vec![Box::new(TrackingCollector::new())],
        vec![],
        vec![],
        vec![Box::new(CfgNotifier {
            name: "UnavailNotifier",
            available: false,
            captured: captured.clone(),
        })],
        tempdir.path(),
    );

    let diag = pipeline.handle_event(&make_crash_event(), 0);

    assert!(
        captured.lock().unwrap().is_none(),
        "unavailable notifier ran"
    );
    let status = diag.plugins.iter().find(|p| p.name == "UnavailNotifier");
    assert!(matches!(
        status.map(|p| &p.status),
        Some(PluginStatus::Skipped(_))
    ));
}

// ── Filter/collector edge cases ──

#[test]
fn test_filter_error_proceeds() {
    let tempdir = tempfile::tempdir().unwrap();
    let called = Arc::new(AtomicBool::new(false));
    let pipeline = make_full_pipeline(
        vec![Box::new(ErroringFilter)],
        vec![Box::new(CfgCollector {
            name: "AfterErrFilter",
            called: called.clone(),
            available: true,
            panic: false,
        })],
        vec![],
        vec![],
        vec![],
        tempdir.path(),
    );

    let diag = pipeline.handle_event(&make_crash_event(), 0);

    // A filter that errors is treated as "proceed", so the collector still runs.
    assert!(called.load(Ordering::SeqCst));
    assert!(diag.succeeded("AfterErrFilter"));
}

#[test]
fn test_collector_unavailable_is_skipped() {
    let tempdir = tempfile::tempdir().unwrap();
    let called = Arc::new(AtomicBool::new(false));
    let pipeline = make_full_pipeline(
        vec![],
        vec![Box::new(CfgCollector {
            name: "UnavailCollector",
            called: called.clone(),
            available: false,
            panic: false,
        })],
        vec![],
        vec![],
        vec![],
        tempdir.path(),
    );

    let diag = pipeline.handle_event(&make_crash_event(), 0);

    assert!(!called.load(Ordering::SeqCst));
    let status = diag.plugins.iter().find(|p| p.name == "UnavailCollector");
    assert!(matches!(
        status.map(|p| &p.status),
        Some(PluginStatus::Skipped(_))
    ));
}

// ── Full pipeline with a plugin in every category (9.4) ──

#[test]
fn test_full_pipeline_all_categories_run() {
    let tempdir = tempfile::tempdir().unwrap();
    let pre_called = Arc::new(AtomicBool::new(false));
    let post_called = Arc::new(AtomicBool::new(false));
    let notified = Arc::new(Mutex::new(None));

    let pipeline = make_full_pipeline(
        vec![Box::new(MockFilter { allow: true })],
        vec![Box::new(TrackingCollector::new())],
        vec![Box::new(CfgPreProcessor {
            name: "AllPre",
            called: pre_called.clone(),
            available: true,
            panic: false,
            set_duplicate: false,
        })],
        vec![Box::new(CfgPostProcessor {
            name: "AllPost",
            called: post_called.clone(),
            available: true,
            panic: false,
        })],
        vec![Box::new(CfgNotifier {
            name: "AllNotifier",
            available: true,
            captured: notified.clone(),
        })],
        tempdir.path(),
    );

    let diag = pipeline.handle_event(&make_crash_event(), 0);

    // Every category executed in order and a report was produced.
    assert!(diag.succeeded("TrackingCollector"));
    assert!(diag.succeeded("AllPre"));
    assert!(diag.succeeded("AllPost"));
    assert!(pre_called.load(Ordering::SeqCst));
    assert!(post_called.load(Ordering::SeqCst));
    assert!(notified.lock().unwrap().is_some());
    assert!(json_report_exists(tempdir.path()));
}

#[test]
fn test_report_policy_maps_each_trigger_to_exactly_one_report_type() {
    let tempdir = tempfile::tempdir().unwrap();
    let mut pipeline = make_full_pipeline(vec![], vec![], vec![], vec![], vec![], tempdir.path());
    let report_types = [
        ReportType::Crash,
        ReportType::ExitFailure,
        ReportType::SignalFailure,
        ReportType::Oom,
        ReportType::Anr,
        ReportType::Snapshot,
    ];

    for expected in report_types {
        pipeline.triggers = TriggerPolicy {
            crash: expected == ReportType::Crash,
            exit_failure: expected == ReportType::ExitFailure,
            signal_failure: expected == ReportType::SignalFailure,
            probable_oom: expected == ReportType::Oom,
            anr: expected == ReportType::Anr,
            snapshot: expected == ReportType::Snapshot,
        };

        for actual in report_types {
            assert_eq!(
                pipeline.report_enabled(actual),
                actual == expected,
                "policy for {expected:?} was applied to {actual:?}"
            );
        }
    }

    pipeline.enabled = false;
    pipeline.triggers = TriggerPolicy::ALL_ENABLED;
    for report_type in report_types {
        assert!(!pipeline.report_enabled(report_type));
    }
}

#[test]
fn test_global_disable_skips_every_report_type_without_side_effects() {
    let tempdir = tempfile::tempdir().unwrap();
    let platform = Arc::new(MockPlatform::default());
    let collector_called = Arc::new(AtomicBool::new(false));
    let pre_called = Arc::new(AtomicBool::new(false));
    let post_called = Arc::new(AtomicBool::new(false));
    let notified = Arc::new(Mutex::new(None));
    let pipeline = Pipeline {
        enabled: false,
        triggers: TriggerPolicy::ALL_ENABLED,
        filters: vec![Box::new(MockFilter { allow: true })],
        collectors: vec![Box::new(CfgCollector {
            name: "DisabledCollector",
            called: collector_called.clone(),
            available: true,
            panic: false,
        })],
        pre_processors: vec![Box::new(CfgPreProcessor {
            name: "DisabledPreProcessor",
            called: pre_called.clone(),
            available: true,
            panic: false,
            set_duplicate: false,
        })],
        post_processors: vec![Box::new(CfgPostProcessor {
            name: "DisabledPostProcessor",
            called: post_called.clone(),
            available: true,
            panic: false,
        })],
        notifiers: vec![Box::new(CfgNotifier {
            name: "DisabledNotifier",
            available: true,
            captured: notified.clone(),
        })],
        shm: None,
        platform: platform.clone(),
        output_dir: Some(tempdir.path().to_path_buf()),
    };

    for report_type in [
        ReportType::Crash,
        ReportType::Snapshot,
        ReportType::Anr,
        ReportType::ExitFailure,
        ReportType::SignalFailure,
        ReportType::Oom,
    ] {
        let mut event = make_crash_event();
        event.report_type = report_type;
        event.termination = match report_type {
            ReportType::ExitFailure => Some(TerminationReason::Exited {
                exit_code: 7,
                runtime_ms: 10,
            }),
            ReportType::SignalFailure | ReportType::Oom => Some(TerminationReason::Signaled {
                signal: 9,
                core_dumped: false,
                runtime_ms: 10,
            }),
            _ => None,
        };

        let diagnostics = match report_type {
            ReportType::ExitFailure | ReportType::SignalFailure | ReportType::Oom => {
                pipeline.handle_termination_event(&event)
            }
            _ => pipeline.handle_event(&event, 123),
        };
        assert!(diagnostics.plugins.is_empty());
        assert!(diagnostics.report_path.is_none());
    }

    assert_eq!(platform.suspend_count(), 0);
    assert_eq!(platform.resume_count(), 0);
    assert!(platform.deallocated_ports().is_empty());
    assert!(!collector_called.load(Ordering::SeqCst));
    assert!(!pre_called.load(Ordering::SeqCst));
    assert!(!post_called.load(Ordering::SeqCst));
    assert!(notified.lock().unwrap().is_none());
    let artifacts: Vec<_> = std::fs::read_dir(tempdir.path())
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    assert!(artifacts.is_empty(), "global disable created {artifacts:?}");
}

// ── Real factory: builds and passes dependency-order validation ──

#[test]
fn test_attachment_copier_obeys_preprocessor_category_switch() {
    for attachment_collector_registered in [false, true] {
        for pre_processors_enabled in [false, true] {
            assert_eq!(
                should_register_attachment_copier(
                    attachment_collector_registered,
                    pre_processors_enabled,
                ),
                attachment_collector_registered && pre_processors_enabled
            );
        }
    }
}

#[test]
fn test_macos_factory_preserves_validated_global_and_trigger_policy() {
    let disabled =
        serde_json::from_str::<crate::config::CrashReporterConfig>(r#"{ "enabled": false }"#)
            .unwrap()
            .validate()
            .unwrap();
    let disabled_pipeline = default_macos_pipeline_from_config(None, &disabled).unwrap();
    assert!(!disabled_pipeline.enabled);
    assert!(disabled_pipeline.collectors.is_empty());
    assert!(disabled_pipeline.post_processors.is_empty());
    for report_type in [
        ReportType::Crash,
        ReportType::ExitFailure,
        ReportType::SignalFailure,
        ReportType::Oom,
        ReportType::Anr,
        ReportType::Snapshot,
    ] {
        assert!(!disabled_pipeline.report_enabled(report_type));
    }

    let selective = serde_json::from_str::<crate::config::CrashReporterConfig>(
        r#"{
            "triggers": {
                "crash": { "enabled": false },
                "oom_detection": { "enabled": false },
                "snapshot": { "enabled": false }
            }
        }"#,
    )
    .unwrap()
    .validate()
    .unwrap();
    let selective_pipeline = default_macos_pipeline_from_config(None, &selective).unwrap();
    assert!(selective_pipeline.enabled);
    assert!(!selective_pipeline.report_enabled(ReportType::Crash));
    assert!(selective_pipeline.report_enabled(ReportType::ExitFailure));
    assert!(selective_pipeline.report_enabled(ReportType::SignalFailure));
    assert!(!selective_pipeline.report_enabled(ReportType::Oom));
    assert!(selective_pipeline.report_enabled(ReportType::Anr));
    assert!(!selective_pipeline.report_enabled(ReportType::Snapshot));
}

#[test]
fn test_default_macos_pipeline_builds_and_validates() {
    // Exercises the full plugin-registration factory with default (all-enabled)
    // config. Structured validation catches registration drift without a
    // startup panic.
    let pipeline = default_macos_pipeline(None).unwrap();
    pipeline.validate_dependencies().unwrap();

    // Default config enables every category.
    assert!(!pipeline.collectors.is_empty(), "no collectors registered");
    assert!(
        !pipeline.pre_processors.is_empty(),
        "no pre-processors registered"
    );
    assert!(
        !pipeline.post_processors.is_empty(),
        "no post-processors registered"
    );
    assert!(!pipeline.filters.is_empty(), "no filters registered");
    assert!(!pipeline.notifiers.is_empty(), "no notifiers registered");
}

#[test]
fn test_factory_runtime_metadata_matches_static_config_registry() {
    let validated = crate::config::CrashReporterConfig::default()
        .validate()
        .unwrap();
    let pipeline = default_macos_pipeline_from_config(None, &validated).unwrap();
    let categories = [
        (
            PluginCategory::Filter,
            plugin_graph_nodes(&pipeline.filters),
        ),
        (
            PluginCategory::Collector,
            plugin_graph_nodes(&pipeline.collectors),
        ),
        (
            PluginCategory::PreProcessor,
            plugin_graph_nodes(&pipeline.pre_processors),
        ),
        (
            PluginCategory::PostProcessor,
            plugin_graph_nodes(&pipeline.post_processors),
        ),
        (
            PluginCategory::Notifier,
            plugin_graph_nodes(&pipeline.notifiers),
        ),
    ];

    let mut runtime_ids = BTreeSet::new();
    for (runtime_category, nodes) in categories {
        for runtime in nodes {
            runtime_ids.insert(runtime.id.clone());
            let (configured_category, configured) =
                crate::config::registered_plugin_spec(&runtime.id).unwrap_or_else(|| {
                    panic!("factory plugin '{}' is absent from registry", runtime.id)
                });
            assert_eq!(
                runtime_category, configured_category,
                "{} category drift",
                runtime.id
            );
            assert_eq!(
                runtime.hard_dependencies, configured.hard_dependencies,
                "{} hard dependency drift",
                runtime.id
            );
            assert_eq!(
                runtime.order_dependencies, configured.order_dependencies,
                "{} order-only dependency drift",
                runtime.id
            );
        }
    }

    // Exact reverse roster for a default factory without SHM. Breadcrumb,
    // context, screenshot, attachment, and the derived AttachmentCopier are
    // intentionally absent; FeedbackDialog is availability-dependent.
    let unconditional: BTreeSet<String> = [
        "DiskSpaceFilter",
        "RateLimiter",
        "ThreadCollector",
        "MemoryCollector",
        "DylibCollector",
        "EnvironmentCollector",
        "SessionEnricher",
        "SymbolResolver",
        "Fingerprinter",
        "BuildInfoEnricher",
        "DuplicateDetector",
        "Sanitizer",
        "RawCleanup",
        "SessionRecorder",
        "PNGConverter",
        "ZIPArchiver",
        "MoveToSent",
        "LogRotator",
        "RetentionManager",
        "ConsoleNotifier",
    ]
    .into_iter()
    .map(str::to_string)
    .collect();
    assert!(
        unconditional.is_subset(&runtime_ids),
        "factory omitted default plugins: {:?}",
        unconditional.difference(&runtime_ids).collect::<Vec<_>>()
    );
    let conditional_or_unexpected: Vec<&String> = runtime_ids.difference(&unconditional).collect();
    assert!(
        conditional_or_unexpected
            .iter()
            .all(|plugin_id| plugin_id.as_str() == "FeedbackDialog"),
        "unexpected default factory plugins: {conditional_or_unexpected:?}"
    );
}

#[test]
fn test_factory_consumes_closed_hard_dependencies_but_keeps_order_only_dependents() {
    let hard_disabled = serde_json::from_str::<crate::config::CrashReporterConfig>(
        r#"{
            "pre_processors": {
                "fingerprint": { "enabled": false },
                "duplicate": { "enabled": true }
            }
        }"#,
    )
    .unwrap()
    .validate()
    .unwrap();
    let pipeline = default_macos_pipeline_from_config(None, &hard_disabled).unwrap();
    let ids: BTreeSet<&str> = pipeline
        .pre_processors
        .iter()
        .map(|plugin| plugin.name())
        .collect();
    assert!(!ids.contains("Fingerprinter"));
    assert!(!ids.contains("DuplicateDetector"));
    assert!(matches!(
        hard_disabled.diagnostics(),
        [crate::config::ConfigValidationDiagnostic::DependentDisabled {
            plugin_id,
            dependency,
            ..
        }] if plugin_id == "DuplicateDetector" && dependency == "Fingerprinter"
    ));

    let order_missing = serde_json::from_str::<crate::config::CrashReporterConfig>(
        r#"{
            "pre_processors": {
                "symbolizer": { "enabled": false },
                "fingerprint": { "enabled": true }
            }
        }"#,
    )
    .unwrap()
    .validate()
    .unwrap();
    let pipeline = default_macos_pipeline_from_config(None, &order_missing).unwrap();
    let ids: BTreeSet<&str> = pipeline
        .pre_processors
        .iter()
        .map(|plugin| plugin.name())
        .collect();
    assert!(!ids.contains("SymbolResolver"));
    assert!(ids.contains("Fingerprinter"));
    pipeline.validate_dependencies().unwrap();
}

#[test]
fn test_runtime_registry_rejects_cross_category_order_only_dependency() {
    let tempdir = tempfile::tempdir().unwrap();
    let pipeline = make_full_pipeline(
        vec![Box::new(DependencyFilter(dependency_behavior(
            "CrossDependent",
            &[],
            &["CrossProvider"],
            false,
        )))],
        vec![Box::new(DependencyCollector(dependency_behavior(
            "CrossProvider",
            &[],
            &[],
            false,
        )))],
        vec![],
        vec![],
        vec![],
        tempdir.path(),
    );

    assert!(matches!(
        pipeline.validate_dependencies(),
        Err(crate::config::ConfigValidationError::MissingDependency {
            category: PluginCategory::Filter,
            kind: DependencyKind::OrderOnly,
            ref plugin_id,
            ref dependency,
        }) if plugin_id == "CrossDependent" && dependency == "CrossProvider"
    ));
}

#[test]
fn test_runtime_registry_rejects_duplicate_ids_across_categories() {
    let tempdir = tempfile::tempdir().unwrap();
    let pipeline = make_full_pipeline(
        vec![Box::new(DependencyFilter(dependency_behavior(
            "GlobalDuplicate",
            &[],
            &[],
            false,
        )))],
        vec![Box::new(DependencyCollector(dependency_behavior(
            "GlobalDuplicate",
            &[],
            &[],
            false,
        )))],
        vec![],
        vec![],
        vec![],
        tempdir.path(),
    );

    assert!(matches!(
        pipeline.validate_dependencies(),
        Err(crate::config::ConfigValidationError::DuplicatePluginId {
            ref plugin_id,
            first_category: PluginCategory::Filter,
            second_category: PluginCategory::Collector,
        }) if plugin_id == "GlobalDuplicate"
    ));
}
