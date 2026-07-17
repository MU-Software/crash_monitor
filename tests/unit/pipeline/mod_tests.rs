use super::*;
use crate::collectors::thread::RawThreadData;
use crate::platform::mock::MockPlatform;
use crate::postprocessors::{MoveToSent, ZIPArchiver};
use mach2::port::mach_port_t;
use std::collections::BTreeMap;
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
    fn priority(&self) -> Priority {
        Priority::Medium
    }
    fn depends_on(&self) -> &'static [&'static str] {
        self.dep
    }
}

impl Collector for MockCollector {
    fn collect(
        &self,
        _event: &CrashEvent,
        _task: mach_port_t,
        _data: &mut CollectedData,
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
    fn priority(&self) -> Priority {
        Priority::Critical
    }
}

impl Filter for MockFilter {
    fn should_process(&self, _event: &CrashEvent) -> Result<bool, String> {
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

// CRITICAL: Test pipelines must always set `output_dir` to a tempdir.
// If `output_dir` is None, `handle_event()` falls back to `~/.modelblockbuilder/crashes/pending/`
// (the user's real crash directory) and pollutes it with test fixture reports.
fn make_pipeline_with_collector(
    collector: Box<dyn Collector>,
    tempdir: &std::path::Path,
) -> Pipeline {
    Pipeline {
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
fn test_filter_blocks_processing() {
    let tempdir = tempfile::tempdir().unwrap();
    let pipeline = make_pipeline_with_filter(
        Box::new(MockFilter { allow: false }),
        Box::new(MockCollector { dep: &[] }),
        tempdir.path(),
    );
    let event = make_event();
    let diag = pipeline.handle_event(&event, 0);

    // Filter returned false → pipeline short-circuits, no collectors run
    assert!(
        diag.plugins.is_empty(),
        "No plugins should run when filter blocks"
    );
}

#[test]
fn test_dependency_ordering_valid() {
    // A has no deps, B depends on A — valid order
    let plugins: Vec<Box<dyn Collector>> =
        vec![Box::new(MockCollector { dep: &[] }), Box::new(DepCollector)];
    // Should not panic
    validate_plugin_order("Collector", &plugins);
}

#[test]
#[should_panic(expected = "depends on 'MockCollector' which is registered after it")]
fn test_dependency_ordering_invalid() {
    // B depends on A, but B is registered before A — invalid
    let plugins: Vec<Box<dyn Collector>> =
        vec![Box::new(DepCollector), Box::new(MockCollector { dep: &[] })];
    validate_plugin_order("Collector", &plugins);
}

// A collector that depends on MockCollector
struct DepCollector;

impl Plugin for DepCollector {
    fn name(&self) -> &'static str {
        "DepCollector"
    }
    fn priority(&self) -> Priority {
        Priority::Low
    }
    fn depends_on(&self) -> &'static [&'static str] {
        &["MockCollector"]
    }
}

impl Collector for DepCollector {
    fn collect(
        &self,
        _event: &CrashEvent,
        _task: mach_port_t,
        _data: &mut CollectedData,
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
    fn priority(&self) -> Priority {
        Priority::Low
    }
    fn depends_on(&self) -> &'static [&'static str] {
        &["FailingCollector"]
    }
}

impl Collector for DependentOnFailCollector {
    fn collect(
        &self,
        _event: &CrashEvent,
        _task: mach_port_t,
        _data: &mut CollectedData,
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
    // Resume should still be called
    assert_eq!(platform.resume_count(), 1);
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
    fn priority(&self) -> Priority {
        Priority::Critical
    }
}

impl Filter for ErroringFilter {
    fn should_process(&self, _event: &CrashEvent) -> Result<bool, String> {
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
        _task: mach_port_t,
        data: &mut CollectedData,
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
    fn priority(&self) -> Priority {
        Priority::Low
    }
    fn is_available(&self) -> bool {
        self.available
    }
}

impl PostProcessor for CfgPostProcessor {
    fn process(&self, _event: &CrashEvent, _result: &mut ReportResult) -> Result<(), String> {
        self.called.store(true, Ordering::SeqCst);
        assert!(!self.panic, "intentional post-processor panic");
        Ok(())
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
    fn priority(&self) -> Priority {
        Priority::Low
    }
    fn is_available(&self) -> bool {
        self.available
    }
}

impl Notifier for CfgNotifier {
    fn notify(&self, report_path: &Path) -> Result<(), String> {
        *self.captured.lock().unwrap() = Some(report_path.to_path_buf());
        Ok(())
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

// ── Real factory: builds and passes dependency-order validation ──

#[test]
fn test_default_macos_pipeline_builds_and_validates() {
    // Exercises the full plugin-registration factory with default (all-enabled)
    // config. validate_dependencies() panics if any plugin is registered before
    // a dependency it declares, so this guards against a future plugin being
    // added out of order.
    let pipeline = default_macos_pipeline(None);
    pipeline.validate_dependencies();

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
