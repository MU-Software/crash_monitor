use super::*;
use crate::collectors::thread::RawThreadData;
use crate::platform::mock::MockPlatform;
use crate::postprocessors::{MoveToSent, RetentionManager, ZIPArchiver};
use mach2::port::mach_port_t;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

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

#[derive(Clone, Debug)]
struct StageContextObservation {
    stage: &'static str,
    report_id: String,
    context_address: usize,
    committed_report_id: Option<String>,
    committed_report_dir: Option<PathBuf>,
}

#[derive(Clone)]
struct StageContextProbe {
    name: &'static str,
    phase: PostProcessorPhase,
    observations: Arc<Mutex<Vec<StageContextObservation>>>,
}

impl StageContextProbe {
    fn new(
        name: &'static str,
        phase: PostProcessorPhase,
        observations: Arc<Mutex<Vec<StageContextObservation>>>,
    ) -> Self {
        Self {
            name,
            phase,
            observations,
        }
    }

    fn observe(&self, context: &PluginContext) -> Result<(), String> {
        let report = context
            .report_context()
            .ok_or_else(|| format!("{} received no report context", self.name))?;
        let committed = context.committed_report();
        self.observations
            .lock()
            .map_err(|_| "context observations lock poisoned".to_string())?
            .push(StageContextObservation {
                stage: self.name,
                report_id: report.report_id().as_str().to_string(),
                context_address: std::ptr::from_ref(report) as usize,
                committed_report_id: committed
                    .as_ref()
                    .map(|report| report.report_id.as_str().to_string()),
                committed_report_dir: committed.map(|report| report.report_dir),
            });
        Ok(())
    }
}

impl Plugin for StageContextProbe {
    fn name(&self) -> &'static str {
        self.name
    }

    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }

    fn priority(&self) -> Priority {
        Priority::Medium
    }
}

impl Collector for StageContextProbe {
    fn collect(
        &self,
        _event: &CrashEvent,
        _task: mach_port_t,
        _data: &mut CollectedData,
        context: &PluginContext,
    ) -> Result<(), String> {
        self.observe(context)
    }
}

impl Filter for StageContextProbe {
    fn should_process(&self, _event: &CrashEvent, context: &PluginContext) -> Result<bool, String> {
        self.observe(context)?;
        Ok(true)
    }
}

impl PreProcessor for StageContextProbe {
    fn process(
        &self,
        _event: &CrashEvent,
        _data: &mut CollectedData,
        context: &PluginContext,
    ) -> Result<(), String> {
        self.observe(context)
    }
}

impl PostProcessor for StageContextProbe {
    fn phase(&self) -> PostProcessorPhase {
        self.phase
    }

    fn process(
        &self,
        _event: &CrashEvent,
        _result: &mut ReportResult,
        context: &PluginContext,
    ) -> Result<(), String> {
        self.observe(context)
    }
}

impl Notifier for StageContextProbe {
    fn notify(&self, _report_path: &Path, context: &PluginContext) -> Result<(), String> {
        self.observe(context)
    }
}

// ═══════════════════════════════════════════════════
//  Helpers
// ═══════════════════════════════════════════════════

fn make_event() -> CrashEvent {
    CrashEvent {
        report_id: crate::pipeline::ReportId::default(),
        report_type: ReportType::Snapshot,
        termination: None,
        exception_type: None,
        exception_code: None,
        exception_subcode: None,
        exception_codes: Vec::new(),
        crashed_thread: None,
        bail_on_suspend_failure: false,
        pid: 1234,
        process_name: "test".into(),
        hang_duration_ms: None,
    }
}

fn unique_shm_pid() -> u32 {
    static NEXT_PID: AtomicU32 = AtomicU32::new(1_700_000);
    NEXT_PID.fetch_add(1, Ordering::Relaxed)
}

fn write_shm_app_version_bytes(shm: &crate::shm::SharedMemory, value: &str) {
    const FIELD_LEN: usize = 16;
    assert!(value.len() < FIELD_LEN);
    let offset =
        crate::shm::CONTEXT_OFFSET + std::mem::offset_of!(crate::shm::SutCrashContext, app_version);
    // SAFETY: the test owns this mapping and writes a bounded schema field
    // while no snapshot operation is running.
    unsafe {
        let field = shm.base_ptr().add(offset);
        std::ptr::write_bytes(field, 0, FIELD_LEN);
        std::ptr::copy_nonoverlapping(value.as_ptr(), field, value.len());
    }
}

fn write_shm_history_max(shm: &crate::shm::SharedMemory, value: i32) {
    let offset = crate::shm::SETTINGS_OFFSET
        + std::mem::offset_of!(crate::shm::SutCrashSettingsSnapshot, history_max);
    // SAFETY: the test owns this mapping and writes one in-bounds schema field
    // before any snapshot operation starts.
    unsafe {
        std::ptr::copy_nonoverlapping(
            value.to_ne_bytes().as_ptr(),
            shm.base_ptr().add(offset),
            std::mem::size_of::<i32>(),
        );
    }
}

fn store_shm_context_generation(shm: &crate::shm::SharedMemory, value: u32) {
    let offset = crate::shm::SECTION1_OFFSET
        + std::mem::offset_of!(crate::shm::ShmHeader, context_generation);
    // SAFETY: the schema guarantees this publication word is naturally
    // aligned, and the test keeps the mapping alive for the atomic store.
    #[allow(clippy::cast_ptr_alignment)] // schema offset is compile-time aligned
    let generation = unsafe { &*shm.base_ptr().add(offset).cast::<AtomicU32>() };
    generation.store(value, Ordering::Release);
}

fn store_shm_settings_generation(shm: &crate::shm::SharedMemory, value: u32) {
    let offset = crate::shm::SECTION1_OFFSET
        + std::mem::offset_of!(crate::shm::ShmHeader, settings_generation);
    // SAFETY: the schema guarantees this publication word is naturally
    // aligned, and the test keeps the mapping alive for the atomic store.
    #[allow(clippy::cast_ptr_alignment)] // schema offset is compile-time aligned
    let generation = unsafe { &*shm.base_ptr().add(offset).cast::<AtomicU32>() };
    generation.store(value, Ordering::Release);
}

fn publish_shm_app_version(shm: &crate::shm::SharedMemory, value: &str) {
    store_shm_context_generation(shm, 1);
    write_shm_app_version_bytes(shm, value);
    store_shm_context_generation(shm, 2);
}

#[test]
fn subprocess_execution_requires_supervisor_boundary() {
    let result = run_stage("BoundaryBypass", PluginExecution::Subprocess, None, |_| {
        Ok(())
    });

    assert!(matches!(
        result,
        PluginRunResult::Failed(error) if error.contains("did not use the subprocess supervisor")
    ));
}

#[test]
fn subprocess_execution_allows_explicit_noop() {
    let result = run_stage(
        "BoundaryNoop",
        PluginExecution::Subprocess,
        None,
        |context| {
            context.mark_subprocess_not_required();
            Ok(())
        },
    );

    assert!(matches!(result, PluginRunResult::Completed(())));
}

#[test]
fn subprocess_adapter_cancellation_maps_to_pipeline_timeout_status() {
    let result = run_stage(
        "CancelledSubprocessAdapter",
        PluginExecution::Subprocess,
        None,
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
                PluginRunResult::Panicked(message) => {
                    Err(format!("subprocess supervisor panicked: {message}"))
                }
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
        collection_policy: crate::config::CollectionPolicy::FULL,
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
        collection_policy: crate::config::CollectionPolicy::FULL,
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
    let event = make_event();
    let report_context = pipeline.create_report_context(&event).unwrap();

    let payload = pipeline.collect_snapshot(&event, 0, &cancelled, None, &report_context);
    let status = payload
        .diagnostics
        .plugins
        .iter()
        .find(|entry| entry.name == "CaptureDeadline")
        .map(|entry| &entry.status);

    assert!(matches!(status, Some(PluginStatus::TimedOut)));
}

#[test]
fn test_minimal_policy_never_persists_stage1_shm_raw() {
    let tempdir = tempfile::tempdir().unwrap();
    let shm =
        Arc::new(crate::shm::SharedMemory::create(unique_shm_pid()).expect("create shared memory"));
    publish_shm_app_version(&shm, "private-secret");
    let platform = Arc::new(MockPlatform::default());
    let pipeline = Pipeline {
        enabled: true,
        triggers: TriggerPolicy::ALL_ENABLED,
        collection_policy: crate::config::CollectionPolicy::MINIMAL,
        filters: vec![],
        collectors: vec![],
        pre_processors: vec![],
        post_processors: vec![],
        notifiers: vec![],
        shm: Some(shm),
        platform,
        output_dir: Some(tempdir.path().to_path_buf()),
    };

    let CaptureOutcome::Captured(captured) = pipeline.capture_event(&make_event(), 7) else {
        panic!("capture should succeed");
    };
    assert!(captured.raw_shm.is_none());
    let _ = pipeline.finalize_captured(*captured);

    assert!(committed_artifact(tempdir.path(), "context.bin").is_none());
    assert!(committed_artifact(tempdir.path(), "breadcrumbs.bin").is_none());
    let report = committed_artifact(tempdir.path(), "report.json").expect("minimal report");
    let report_bytes = std::fs::read(report).unwrap();
    assert!(
        !report_bytes
            .windows("private-secret".len())
            .any(|window| window == b"private-secret")
    );
}

#[test]
fn test_stage1_shm_dump_uses_pre_resume_owned_bytes() {
    let tempdir = tempfile::tempdir().unwrap();
    let shm =
        Arc::new(crate::shm::SharedMemory::create(unique_shm_pid()).expect("create shared memory"));
    publish_shm_app_version(&shm, "before-resume");
    let platform = Arc::new(MockPlatform::default());
    let pipeline = Pipeline {
        enabled: true,
        triggers: TriggerPolicy::ALL_ENABLED,
        collection_policy: crate::config::CollectionPolicy::FULL,
        filters: vec![],
        collectors: vec![],
        pre_processors: vec![],
        post_processors: vec![],
        notifiers: vec![],
        shm: Some(shm.clone()),
        platform: platform.clone(),
        output_dir: Some(tempdir.path().to_path_buf()),
    };

    let CaptureOutcome::Captured(captured) = pipeline.capture_event(&make_event(), 7) else {
        panic!("capture should succeed");
    };
    assert_eq!(platform.resume_count(), 1);
    assert!(
        !captured
            .diagnostics
            .plugins
            .iter()
            .any(|entry| entry.name == "ShmSnapshot"),
        "a stable snapshot must not produce a consistency diagnostic"
    );

    // Simulate the child changing the live mapping immediately after resume.
    publish_shm_app_version(&shm, "after-resume");
    let _ = pipeline.finalize_captured(*captured);

    let raw_context =
        committed_artifact(tempdir.path(), "context.bin").expect("Stage 1 context dump");
    let bytes = std::fs::read(raw_context).unwrap();
    assert!(
        bytes
            .windows("before-resume".len())
            .any(|window| window == b"before-resume")
    );
    assert!(
        !bytes
            .windows("after-resume".len())
            .any(|window| window == b"after-resume")
    );
}

#[test]
fn test_torn_context_snapshot_is_diagnosed_and_sanitized_without_live_reaccess() {
    let tempdir = tempfile::tempdir().unwrap();
    let shm =
        Arc::new(crate::shm::SharedMemory::create(unique_shm_pid()).expect("create shared memory"));
    store_shm_context_generation(&shm, 1);
    write_shm_app_version_bytes(&shm, "torn-context");
    store_shm_settings_generation(&shm, 1);
    write_shm_history_max(&shm, 321);
    store_shm_settings_generation(&shm, 2);

    let platform = Arc::new(MockPlatform::default());
    let mut pipeline = Pipeline {
        enabled: true,
        triggers: TriggerPolicy::ALL_ENABLED,
        collection_policy: crate::config::CollectionPolicy::FULL,
        filters: vec![],
        collectors: vec![Box::new(crate::collectors::ContextCollector::new())],
        pre_processors: vec![],
        post_processors: vec![],
        notifiers: vec![],
        shm: Some(shm.clone()),
        platform: platform.clone(),
        output_dir: Some(tempdir.path().to_path_buf()),
    };

    let CaptureOutcome::Captured(captured) = pipeline.capture_event(&make_event(), 7) else {
        panic!("capture should retain sanitized stable sections");
    };
    assert_eq!(platform.resume_count(), 1);
    assert!(captured.diagnostics.succeeded("ContextCollector"));
    let snapshot_diagnostic = captured
        .diagnostics
        .plugins
        .iter()
        .find(|entry| entry.name == "ShmSnapshot")
        .expect("torn context consistency diagnostic");
    assert!(
        matches!(
            &snapshot_diagnostic.status,
            PluginStatus::Error(error) if error.contains("Context") && error.contains("sanitized")
        ),
        "unexpected snapshot diagnostic: {:?}",
        snapshot_diagnostic.status
    );

    assert!(
        captured.data.raw.crash_context.is_none(),
        "the torn context must not reach collectors"
    );
    assert_eq!(
        captured
            .data
            .raw
            .settings_snapshot
            .as_ref()
            .expect("stable settings remain readable")
            .history_max,
        321
    );

    let raw_context = &captured
        .raw_shm
        .as_ref()
        .expect("sanitized Stage 1 snapshot remains available")
        .context;
    let context_len = std::mem::size_of::<crate::shm::SutCrashContext>();
    assert!(
        raw_context[..context_len].iter().all(|byte| *byte == 0),
        "the rejected context bytes must be zeroed"
    );
    let history_max_offset =
        context_len + std::mem::offset_of!(crate::shm::SutCrashSettingsSnapshot, history_max);
    assert_eq!(
        i32::from_ne_bytes(
            raw_context[history_max_offset..history_max_offset + std::mem::size_of::<i32>()]
                .try_into()
                .unwrap()
        ),
        321,
        "the stable settings bytes must be preserved"
    );
    let expected_stage1 = raw_context.clone();

    // Remove the last live-mapping owners before finalization. Any accidental
    // post-resume mapping access would now fail; Stage 1 must use owned bytes.
    drop(pipeline.shm.take());
    drop(shm);
    let diagnostics = pipeline.finalize_captured(*captured);
    assert!(
        diagnostics
            .plugins
            .iter()
            .any(|entry| entry.name == "ShmSnapshot"),
        "the consistency diagnostic must survive finalization"
    );

    let raw_context_path =
        committed_artifact(tempdir.path(), "context.bin").expect("Stage 1 context dump");
    assert_eq!(std::fs::read(raw_context_path).unwrap(), expected_stage1);
}

#[test]
fn test_best_effort_suspend_failure_never_snapshots_live_shm() {
    let tempdir = tempfile::tempdir().unwrap();
    let shm =
        Arc::new(crate::shm::SharedMemory::create(unique_shm_pid()).expect("create shared memory"));
    let mut platform = MockPlatform::default();
    platform.suspend_fails = true;
    let platform = Arc::new(platform);
    let pipeline = Pipeline {
        enabled: true,
        triggers: TriggerPolicy::ALL_ENABLED,
        collection_policy: crate::config::CollectionPolicy::FULL,
        filters: vec![],
        collectors: vec![],
        pre_processors: vec![],
        post_processors: vec![],
        notifiers: vec![],
        shm: Some(shm),
        platform: platform.clone(),
        output_dir: Some(tempdir.path().to_path_buf()),
    };

    let CaptureOutcome::Captured(captured) = pipeline.capture_event(&make_event(), 7) else {
        panic!("fatal best-effort capture should continue");
    };

    assert_eq!(platform.resume_count(), 0);
    assert!(captured.raw_shm.is_none());
    assert!(
        !captured
            .diagnostics
            .plugins
            .iter()
            .any(|entry| entry.name == "ShmSnapshot")
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

    // Filters are intentionally outside the live-task capture window. The
    // collector runs, then finalization stops before writing a report.
    assert!(
        diag.succeeded("MockCollector"),
        "capture must finish before a potentially blocking filter"
    );
    let emergency = diag
        .emergency_snapshot
        .expect("filter rejection must retain minimum evidence");
    assert_eq!(emergency.pid, event.pid);
    assert_eq!(emergency.report_type, event.report_type);
    assert!(!json_report_exists(tempdir.path()));
    assert_eq!(std::fs::read_dir(tempdir.path()).unwrap().count(), 0);
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
            thread_id: 4_242,
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

/// Collector whose implementation panics inside the plugin boundary.
struct PanickingCollector;

impl Plugin for PanickingCollector {
    fn name(&self) -> &'static str {
        "PanickingCollector"
    }
    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }
    fn priority(&self) -> Priority {
        Priority::Medium
    }
}

impl Collector for PanickingCollector {
    fn collect(
        &self,
        _event: &CrashEvent,
        _task: mach_port_t,
        _data: &mut CollectedData,
        _context: &PluginContext,
    ) -> Result<(), String> {
        panic!("intentional collector panic");
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
        report_id: crate::pipeline::ReportId::default(),
        report_type: ReportType::Crash,
        termination: None,
        exception_type: Some(1),
        exception_code: Some(0xDEAD),
        exception_subcode: Some(0xBEEF),
        exception_codes: vec![0xDEAD, 0xBEEF],
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
        collection_policy: crate::config::CollectionPolicy::FULL,
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

    let report_path = diag.report_path.expect("committed JSON report");
    let report_dir = report_path.parent().expect("report directory");
    assert_eq!(
        report_path.file_name().and_then(|name| name.to_str()),
        Some("report.json")
    );
    assert!(report_dir.join("threads.txt").exists());
    assert!(report_dir.join("manifest.json").exists());
}

#[test]
fn identical_pid_type_and_second_create_two_isolated_reports() {
    let tempdir = tempfile::tempdir().unwrap();
    let pipeline = make_pipeline_with_tempdir(
        vec![Box::new(TrackingCollector::new())],
        Arc::new(MockPlatform::default()),
        tempdir.path(),
    );
    let first = make_crash_event();
    let second = make_crash_event();
    assert_ne!(first.report_id, second.report_id);
    assert_eq!(first.pid, second.pid);
    assert_eq!(first.report_type, second.report_type);

    let first_path = pipeline
        .handle_event(&first, 123)
        .report_path
        .expect("first committed report");
    let second_path = pipeline
        .handle_event(&second, 123)
        .report_path
        .expect("second committed report");

    assert_ne!(first_path.parent(), second_path.parent());
    for (event, report_path) in [(&first, first_path), (&second, second_path)] {
        let report_dir = report_path.parent().expect("report directory");
        assert_eq!(
            report_dir.file_name().and_then(|name| name.to_str()),
            Some(event.report_id.as_str())
        );
        let manifest = crate::pipeline::load_manifest(&report_dir.join("manifest.json")).unwrap();
        assert_eq!(manifest.report_id, event.report_id);
        assert_eq!(
            manifest
                .artifacts
                .iter()
                .map(|artifact| artifact.path.as_str())
                .collect::<Vec<_>>(),
            vec!["report.json", "threads.txt"]
        );
        let report = crate::pipeline::report::load_report(&report_path).unwrap();
        assert_eq!(report.header.report_id.as_ref(), Some(&event.report_id));
    }

    assert_eq!(
        std::fs::read_dir(tempdir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.path().is_dir())
            .count(),
        2
    );
}

#[test]
fn every_pipeline_phase_shares_one_immutable_report_context() {
    let tempdir = tempfile::tempdir().unwrap();
    let observations = Arc::new(Mutex::new(Vec::new()));
    let probe = |name, phase| StageContextProbe::new(name, phase, observations.clone());
    let pipeline = Pipeline {
        enabled: true,
        triggers: TriggerPolicy::ALL_ENABLED,
        collection_policy: crate::config::CollectionPolicy::FULL,
        filters: vec![Box::new(probe(
            "ContextFilter",
            PostProcessorPhase::BeforeCommit,
        ))],
        collectors: vec![Box::new(probe(
            "ContextCollector",
            PostProcessorPhase::BeforeCommit,
        ))],
        pre_processors: vec![Box::new(probe(
            "ContextPreProcessor",
            PostProcessorPhase::BeforeCommit,
        ))],
        post_processors: vec![
            Box::new(probe(
                "ContextBeforeCommit",
                PostProcessorPhase::BeforeCommit,
            )),
            Box::new(probe("ContextAfterCommit", PostProcessorPhase::AfterCommit)),
            Box::new(probe("ContextAfterNotify", PostProcessorPhase::AfterNotify)),
            Box::new(probe(
                "ContextFinalCleanup",
                PostProcessorPhase::FinalCleanup,
            )),
        ],
        notifiers: vec![Box::new(probe(
            "ContextNotifier",
            PostProcessorPhase::AfterCommit,
        ))],
        shm: None,
        platform: Arc::new(MockPlatform::default()),
        output_dir: Some(tempdir.path().to_path_buf()),
    };
    let event = make_event();

    let diagnostics = pipeline.handle_event(&event, 0);
    let report_path = diagnostics.report_path.expect("committed report path");
    let observed = observations.lock().unwrap();
    assert_eq!(observed.len(), 8, "observations: {observed:?}");
    assert!(
        observed
            .iter()
            .all(|entry| entry.report_id == event.report_id.as_str())
    );
    assert_eq!(
        observed
            .iter()
            .map(|entry| entry.context_address)
            .collect::<BTreeSet<_>>()
            .len(),
        1,
        "all stages must receive the same ReportContext allocation"
    );
    for entry in observed.iter().filter(|entry| {
        matches!(
            entry.stage,
            "ContextCollector" | "ContextFilter" | "ContextPreProcessor" | "ContextBeforeCommit"
        )
    }) {
        assert!(entry.committed_report_id.is_none(), "{}", entry.stage);
    }
    for entry in observed.iter().filter(|entry| {
        matches!(
            entry.stage,
            "ContextAfterCommit" | "ContextNotifier" | "ContextAfterNotify" | "ContextFinalCleanup"
        )
    }) {
        assert_eq!(
            entry.committed_report_id.as_deref(),
            Some(event.report_id.as_str()),
            "{}",
            entry.stage
        );
        assert_eq!(
            entry.committed_report_dir.as_deref(),
            report_path.parent(),
            "{}",
            entry.stage
        );
    }
}

struct PublicationGate {
    released: Mutex<BTreeSet<String>>,
    wake: Condvar,
    missing_path: AtomicBool,
}

impl PublicationGate {
    fn release(&self, report_id: &str) {
        self.released.lock().unwrap().insert(report_id.to_string());
        self.wake.notify_all();
    }
}

struct GatedPathNotifier {
    entered: std::sync::mpsc::Sender<(String, PathBuf)>,
    gate: Arc<PublicationGate>,
}

impl Plugin for GatedPathNotifier {
    fn name(&self) -> &'static str {
        "GatedPathNotifier"
    }

    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }

    fn priority(&self) -> Priority {
        Priority::Medium
    }
}

impl Notifier for GatedPathNotifier {
    fn notify(&self, report_path: &Path, context: &PluginContext) -> Result<(), String> {
        let report_id = context
            .report_context()
            .ok_or_else(|| "notifier received no report context".to_string())?
            .report_id()
            .as_str()
            .to_string();
        if !report_path.exists() {
            self.gate.missing_path.store(true, Ordering::SeqCst);
        }
        self.entered
            .send((report_id.clone(), report_path.to_path_buf()))
            .map_err(|error| format!("cannot announce notifier entry: {error}"))?;

        let mut released = self
            .gate
            .released
            .lock()
            .map_err(|_| "publication gate lock poisoned".to_string())?;
        while !released.remove(&report_id) {
            context.checkpoint()?;
            let (next, _) = self
                .gate
                .wake
                .wait_timeout(released, std::time::Duration::from_millis(10))
                .map_err(|_| "publication gate lock poisoned".to_string())?;
            released = next;
        }
        drop(released);

        if !report_path.exists() {
            self.gate.missing_path.store(true, Ordering::SeqCst);
        }
        Ok(())
    }
}

#[test]
fn concurrent_retention_never_removes_a_report_from_a_live_notifier() {
    let root = tempfile::tempdir().unwrap();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let gate = Arc::new(PublicationGate {
        released: Mutex::new(BTreeSet::new()),
        wake: Condvar::new(),
        missing_path: AtomicBool::new(false),
    });
    let pipeline = Arc::new(Pipeline {
        enabled: true,
        triggers: TriggerPolicy::ALL_ENABLED,
        collection_policy: crate::config::CollectionPolicy::FULL,
        filters: vec![],
        collectors: vec![],
        pre_processors: vec![],
        post_processors: vec![Box::new(RetentionManager::with_dir(
            1,
            u64::MAX,
            u64::MAX,
            root.path().to_path_buf(),
        ))],
        notifiers: vec![Box::new(GatedPathNotifier {
            entered: entered_tx,
            gate: gate.clone(),
        })],
        shm: None,
        platform: Arc::new(MockPlatform::default()),
        output_dir: Some(root.path().to_path_buf()),
    });
    let event_a = make_event();
    let event_b = make_event();
    let id_a = event_a.report_id.as_str().to_string();
    let id_b = event_b.report_id.as_str().to_string();

    let pipeline_a = pipeline.clone();
    let thread_a = std::thread::spawn(move || pipeline_a.handle_event(&event_a, 0));
    let (entered_a, path_a) = entered_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("first notifier did not start");
    assert_eq!(entered_a, id_a);
    filetime::set_file_mtime(
        path_a.parent().unwrap(),
        filetime::FileTime::from_unix_time(1, 0),
    )
    .unwrap();

    let pipeline_b = pipeline.clone();
    let thread_b = std::thread::spawn(move || pipeline_b.handle_event(&event_b, 0));
    let (entered_b, path_b) = entered_rx
        .recv_timeout(std::time::Duration::from_secs(2))
        .expect("second notifier did not start");
    assert_eq!(entered_b, id_b);
    filetime::set_file_mtime(
        path_b.parent().unwrap(),
        filetime::FileTime::from_unix_time(2, 0),
    )
    .unwrap();

    // The newer report finishes first. Its final-cleanup pass must defer at the
    // leased older report rather than deleting either live notifier path.
    gate.release(&id_b);
    let diagnostics_b = thread_b.join().unwrap();
    assert!(diagnostics_b.report_path.is_some());
    assert!(path_a.exists());
    assert!(path_b.exists());
    assert!(crate::pipeline::artifact::is_report_publication_leased(
        path_a.parent().unwrap()
    ));
    assert!(!crate::pipeline::artifact::is_report_publication_leased(
        path_b.parent().unwrap()
    ));
    filetime::set_file_mtime(
        path_a.parent().unwrap(),
        filetime::FileTime::from_unix_time(1, 0),
    )
    .unwrap();
    filetime::set_file_mtime(
        path_b.parent().unwrap(),
        filetime::FileTime::from_unix_time(2, 0),
    )
    .unwrap();

    // Once the older notifier finishes, its own publication lease remains
    // active through FinalCleanup. The impossible one-report quota must be
    // reported as deferred rather than invalidating either committed path.
    gate.release(&id_a);
    let diagnostics_a = thread_a.join().unwrap();
    assert!(!gate.missing_path.load(Ordering::SeqCst));
    assert!(path_a.exists());
    assert!(path_b.exists());
    assert!(diagnostics_a.report_path.is_some());
    assert!(diagnostics_a.plugins.iter().any(|plugin| {
        plugin.name == "RetentionManager"
            && matches!(
                &plugin.status,
                PluginStatus::Error(error) if error.contains("deferred by a live lease")
            )
    }));
    assert_eq!(
        std::fs::read_dir(root.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry.path().is_dir())
            .count(),
        2
    );
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

    // Pipeline bailed — only the explicit suspension diagnostic is present.
    assert_eq!(diag.plugins.len(), 1);
    assert_eq!(diag.plugins[0].name, "CaptureSuspend");
    assert!(matches!(&diag.plugins[0].status, PluginStatus::Error(_)));
    assert_eq!(platform.resume_count(), 0);

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
        platform.clone(),
        tempdir.path(),
    );

    let event = make_crash_event();
    let diag = pipeline.handle_event(&event, 0);

    // FailingCollector should have error status
    assert!(!diag.succeeded("FailingCollector"));
    assert_eq!(platform.suspend_count(), 1);
    assert_eq!(platform.resume_count(), 1);
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
fn test_panicking_collector_keeps_suspend_resume_balanced() {
    let tempdir = tempfile::tempdir().unwrap();
    let platform = Arc::new(MockPlatform::default());
    let pipeline = make_pipeline_with_tempdir(
        vec![Box::new(PanickingCollector)],
        platform.clone(),
        tempdir.path(),
    );

    let diagnostics = pipeline.handle_event(&make_crash_event(), 123);

    assert!(diagnostics.plugins.iter().any(|entry| {
        entry.name == "PanickingCollector"
            && matches!(&entry.status, PluginStatus::Error(error) if error == "panicked")
    }));
    assert_eq!(platform.suspend_count(), 1);
    assert_eq!(platform.resume_count(), 1);
    assert_eq!(platform.terminate_count(), 0);
}

#[test]
fn test_resume_failure_is_diagnosed_and_terminates_after_bounded_retry() {
    let tempdir = tempfile::tempdir().unwrap();
    let mut platform = MockPlatform::default();
    platform.resume_fails = true;
    let platform = Arc::new(platform);
    let pipeline = make_pipeline_with_tempdir(vec![], platform.clone(), tempdir.path());

    let diagnostics = pipeline.handle_event(&make_crash_event(), 123);

    assert!(diagnostics.plugins.iter().any(|entry| {
        entry.name == "TaskResume" && matches!(&entry.status, PluginStatus::Error(_))
    }));
    assert_eq!(platform.suspend_count(), 1);
    assert_eq!(
        platform.resume_count(),
        crate::platform::RESUME_ATTEMPT_LIMIT
    );
    assert_eq!(platform.terminate_count(), 1);
    assert_eq!(platform.supervisor_health().task_control_failures.len(), 1);
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

    let raw_file =
        committed_artifact(tempdir.path(), "threads.txt").expect("threads.txt file should exist");
    let contents = std::fs::read_to_string(raw_file).unwrap();
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
fn test_stack_sentinel_is_absent_from_minimal_report_and_present_after_opt_in() {
    for capture_stack_memory in [false, true] {
        let tempdir = tempfile::tempdir().unwrap();
        let mut state = vec![0_u32; 68];
        state[31 * 2] = 0x1000;
        state[32 * 2] = 0x2000;
        let mut mock = MockPlatform::default();
        mock.threads = vec![crate::platform::mock::MockThread {
            port: 42,
            stable_id: 4_242,
            name: Some("private-thread".into()),
            state,
        }];
        mock.memory.insert(0x1000, vec![0x5a; 64 * 1024]);
        let platform = Arc::new(mock);
        let mut pipeline = make_pipeline_with_tempdir(
            vec![Box::new(crate::collectors::ThreadCollector::new(
                platform.clone(),
                capture_stack_memory,
            ))],
            platform,
            tempdir.path(),
        );
        pipeline.collection_policy = if capture_stack_memory {
            crate::config::CollectionPolicy::FULL
        } else {
            crate::config::CollectionPolicy::MINIMAL
        };

        let _ = pipeline.handle_event(&make_crash_event(), 7);
        let report_path =
            committed_artifact(tempdir.path(), "report.json").expect("committed report");
        let report: serde_json::Value =
            serde_json::from_slice(&std::fs::read(report_path).unwrap()).unwrap();
        let stack = &report["threads"][0]["stack_memory"];
        if capture_stack_memory {
            assert_eq!(stack["size"], 64 * 1024);
            assert!(
                stack["hex_dump"]
                    .as_str()
                    .is_some_and(|encoded| encoded.starts_with("WlpaWlp"))
            );
        } else {
            assert!(stack.is_null());
            let bytes = serde_json::to_vec(&report).unwrap();
            assert!(
                !bytes
                    .windows(16)
                    .any(|window| window == b"WlpaWlpaWlpaWlpa")
            );
        }
    }
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
        report_id: crate::pipeline::ReportId::default(),
        report_type: ReportType::Anr,
        termination: None,
        exception_type: None,
        exception_code: None,
        exception_subcode: None,
        exception_codes: Vec::new(),
        crashed_thread: None,
        bail_on_suspend_failure: false,
        pid: 9999,
        process_name: "test_app".into(),
        hang_duration_ms: Some(5000),
    };

    let _diag = pipeline.handle_event(&event, 0);

    let json_file =
        committed_artifact(tempdir.path(), "report.json").expect("JSON report should exist");
    let json_str = std::fs::read_to_string(json_file).unwrap();
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

struct PriorityCollector {
    name: &'static str,
    priority: Priority,
    hard: &'static [&'static str],
}

impl Plugin for PriorityCollector {
    fn name(&self) -> &'static str {
        self.name
    }

    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }

    fn priority(&self) -> Priority {
        self.priority
    }

    fn hard_dependencies(&self) -> &'static [&'static str] {
        self.hard
    }
}

impl Collector for PriorityCollector {
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
        collection_policy: crate::config::CollectionPolicy::FULL,
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
    committed_artifact(dir, "report.json").is_some()
}

fn committed_artifact(dir: &std::path::Path, name: &str) -> Option<std::path::PathBuf> {
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .map(|report_dir| report_dir.join(name))
        .find(|path| path.is_file())
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
        report_id: crate::pipeline::ReportId::default(),
        report_type: ReportType::ExitFailure,
        termination: Some(TerminationReason::Exited {
            exit_code: 7,
            runtime_ms: 10,
        }),
        exception_type: None,
        exception_code: None,
        exception_subcode: None,
        exception_codes: Vec::new(),
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
        report_id: crate::pipeline::ReportId::default(),
        report_type: ReportType::ExitFailure,
        termination: Some(TerminationReason::Exited {
            exit_code: 9,
            runtime_ms: 20,
        }),
        exception_type: None,
        exception_code: None,
        exception_subcode: None,
        exception_codes: Vec::new(),
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

    let event = make_crash_event();
    let diagnostics = pipeline.handle_event(&event, 0);

    // Duplicate → Stage 2 report generation is skipped, so no JSON is written.
    assert!(
        !json_report_exists(tempdir.path()),
        "duplicate event should not produce a JSON report"
    );
    assert_eq!(
        diagnostics
            .emergency_snapshot
            .as_ref()
            .map(|snapshot| snapshot.report_id.as_str()),
        Some(event.report_id.as_str())
    );
    assert_eq!(std::fs::read_dir(tempdir.path()).unwrap().count(), 0);
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
    let report_dir = report_path.parent().expect("report directory");
    assert_eq!(report_dir.parent(), Some(sent.as_path()));
    assert!(report_dir.join("manifest.json").exists());
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
        collection_policy: crate::config::CollectionPolicy::MINIMAL,
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

#[test]
fn disabled_pipeline_recovery_is_a_filesystem_noop() {
    let root = tempfile::tempdir().unwrap();
    let output_root = root.path().join("not-created");
    let pipeline = Pipeline {
        enabled: false,
        triggers: TriggerPolicy::ALL_ENABLED,
        collection_policy: crate::config::CollectionPolicy::MINIMAL,
        filters: vec![],
        collectors: vec![],
        pre_processors: vec![],
        post_processors: vec![],
        notifiers: vec![],
        shm: None,
        platform: Arc::new(MockPlatform::default()),
        output_dir: Some(output_root.clone()),
    };

    assert_eq!(pipeline.recover_prepared_artifacts().unwrap(), 0);
    assert_eq!(
        pipeline.recover_startup_artifacts().unwrap(),
        StartupRecovery::default()
    );
    assert!(!output_root.exists());
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
fn test_macos_factory_injects_configured_report_root() {
    let root = tempfile::tempdir().unwrap();
    let configured = root.path().join("configured-reports");
    let config = crate::config::CrashReporterConfig {
        report_dir: Some(configured.to_string_lossy().into_owned()),
        ..crate::config::CrashReporterConfig::default()
    };
    let validated = config.validate().unwrap();

    let pipeline = default_macos_pipeline_from_config(None, &validated).unwrap();

    assert_eq!(pipeline.output_dir.as_deref(), Some(configured.as_path()));
}

#[test]
fn test_default_macos_pipeline_builds_and_validates() {
    // Exercises the full plugin-registration factory with private-by-default
    // config. Structured validation catches registration drift without a
    // startup panic.
    let pipeline = default_macos_pipeline(None).unwrap();
    pipeline.validate_dependencies().unwrap();
    assert_eq!(
        pipeline.collection_policy,
        crate::config::CollectionPolicy::MINIMAL
    );

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
fn test_macos_factory_registers_explicitly_consented_sensitive_collectors() {
    let validated = serde_json::from_str::<crate::config::CrashReporterConfig>(
        r#"{
            "privacy": {
                "level": "full",
                "consent": "granted",
                "raw_shm": true
            },
            "collectors": {
                "thread": { "enabled": true, "stack_memory": true },
                "memory": { "enabled": true },
                "environment": { "enabled": true },
                "screenshot": { "enabled": true },
                "attachment": { "enabled": true }
            }
        }"#,
    )
    .unwrap()
    .validate()
    .unwrap();
    let pipeline = default_macos_pipeline_from_config(None, &validated).unwrap();
    let collector_ids: BTreeSet<&str> = pipeline
        .collectors
        .iter()
        .map(|collector| collector.name())
        .collect();

    assert!(collector_ids.contains("MemoryCollector"));
    assert!(collector_ids.contains("EnvironmentCollector"));
    assert!(pipeline.collection_policy.capture_stack_memory);
    assert!(pipeline.collection_policy.capture_shm_screenshots);
    assert!(pipeline.collection_policy.capture_shm_attachments);
    assert!(pipeline.collection_policy.persist_raw_shm);
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

    // Exact reverse roster for a default factory without SHM. Sensitive
    // collectors are private-by-default; breadcrumb/context require SHM, and
    // FeedbackDialog is availability-dependent.
    let unconditional: BTreeSet<String> = [
        "DiskSpaceFilter",
        "RateLimiter",
        "ThreadCollector",
        "DylibCollector",
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
    assert!(!runtime_ids.contains("MemoryCollector"));
    assert!(!runtime_ids.contains("EnvironmentCollector"));
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
        pipeline.finish_registration(),
        Err(crate::config::ConfigValidationError::DuplicatePluginId {
            ref plugin_id,
            first_category: PluginCategory::Filter,
            second_category: PluginCategory::Collector,
        }) if plugin_id == "GlobalDuplicate"
    ));
}

#[test]
fn registration_stably_orders_priority_without_crossing_dependencies() {
    let tempdir = tempfile::tempdir().unwrap();
    let pipeline = make_full_pipeline(
        vec![],
        vec![
            Box::new(PriorityCollector {
                name: "medium-a",
                priority: Priority::Medium,
                hard: &[],
            }),
            Box::new(PriorityCollector {
                name: "provider",
                priority: Priority::Low,
                hard: &[],
            }),
            Box::new(PriorityCollector {
                name: "medium-b",
                priority: Priority::Medium,
                hard: &[],
            }),
            Box::new(PriorityCollector {
                name: "dependent",
                priority: Priority::Critical,
                hard: &["provider"],
            }),
        ],
        vec![],
        vec![],
        vec![],
        tempdir.path(),
    )
    .finish_registration()
    .unwrap();

    let names = pipeline
        .collectors
        .iter()
        .map(|plugin| plugin.name())
        .collect::<Vec<_>>();
    assert_eq!(names, ["medium-a", "medium-b", "provider", "dependent"]);
}

#[test]
fn plugin_timeout_policy_distinguishes_default_disabled_and_override() {
    assert_eq!(
        PluginTimeout::CategoryDefault.resolve(Duration::from_secs(5)),
        Some(Duration::from_secs(5))
    );
    assert_eq!(
        PluginTimeout::Disabled.resolve(Duration::from_secs(5)),
        None
    );
    assert_eq!(
        PluginTimeout::Override(Duration::from_millis(25)).resolve(Duration::from_secs(5)),
        Some(Duration::from_millis(25))
    );
}

struct CommitBreaker;

impl Plugin for CommitBreaker {
    fn name(&self) -> &'static str {
        "CommitBreaker"
    }

    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }

    fn priority(&self) -> Priority {
        Priority::Low
    }
}

impl PostProcessor for CommitBreaker {
    fn process(
        &self,
        _event: &CrashEvent,
        _result: &mut ReportResult,
        context: &PluginContext,
    ) -> Result<(), String> {
        let staging = context
            .artifact_transaction()
            .expect("artifact transaction")
            .staging_dir();
        std::fs::write(staging.join("unregistered.bin"), b"break exact manifest")
            .map_err(|error| error.to_string())
    }
}

struct AfterCommitProbe {
    calls: Arc<AtomicU32>,
    mutation_rejected: Arc<AtomicBool>,
}

impl Plugin for AfterCommitProbe {
    fn name(&self) -> &'static str {
        "AfterCommitProbe"
    }

    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }

    fn priority(&self) -> Priority {
        Priority::Low
    }
}

impl PostProcessor for AfterCommitProbe {
    fn phase(&self) -> PostProcessorPhase {
        PostProcessorPhase::AfterCommit
    }

    fn process(
        &self,
        _event: &CrashEvent,
        _result: &mut ReportResult,
        context: &PluginContext,
    ) -> Result<(), String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let transaction = context
            .artifact_transaction()
            .expect("artifact transaction");
        self.mutation_rejected.store(
            transaction
                .write_bytes("late.bin", ArtifactKind::Attachment, b"late")
                .is_err(),
            Ordering::SeqCst,
        );
        Ok(())
    }
}

#[test]
fn commit_failure_runs_no_after_commit_side_effects() {
    let root = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicU32::new(0));
    let pipeline = Pipeline {
        enabled: true,
        triggers: TriggerPolicy::ALL_ENABLED,
        collection_policy: crate::config::CollectionPolicy::FULL,
        filters: vec![],
        collectors: vec![],
        pre_processors: vec![],
        post_processors: vec![
            Box::new(CommitBreaker),
            Box::new(AfterCommitProbe {
                calls: calls.clone(),
                mutation_rejected: Arc::new(AtomicBool::new(false)),
            }),
        ],
        notifiers: vec![],
        shm: None,
        platform: Arc::new(MockPlatform::default()),
        output_dir: Some(root.path().to_path_buf()),
    };

    let diagnostics = pipeline.handle_event(&make_crash_event(), 123);

    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(diagnostics.plugins.iter().any(|plugin| {
        plugin.name == "ArtifactCommit" && matches!(plugin.status, PluginStatus::Error(_))
    }));
    assert!(diagnostics.report_path.is_none());
}

#[test]
fn after_commit_stage_cannot_mutate_the_sealed_transaction() {
    let root = tempfile::tempdir().unwrap();
    let calls = Arc::new(AtomicU32::new(0));
    let mutation_rejected = Arc::new(AtomicBool::new(false));
    let pipeline = Pipeline {
        enabled: true,
        triggers: TriggerPolicy::ALL_ENABLED,
        collection_policy: crate::config::CollectionPolicy::FULL,
        filters: vec![],
        collectors: vec![],
        pre_processors: vec![],
        post_processors: vec![Box::new(AfterCommitProbe {
            calls: calls.clone(),
            mutation_rejected: mutation_rejected.clone(),
        })],
        notifiers: vec![],
        shm: None,
        platform: Arc::new(MockPlatform::default()),
        output_dir: Some(root.path().to_path_buf()),
    };

    let diagnostics = pipeline.handle_event(&make_crash_event(), 123);

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(mutation_rejected.load(Ordering::SeqCst));
    assert!(diagnostics.report_path.is_some());
}

#[test]
fn live_report_loader_overlays_terminal_diagnostics_from_exact_manifest() {
    let root = tempfile::tempdir().unwrap();
    let observations = Arc::new(Mutex::new(Vec::new()));
    let pipeline = Pipeline {
        enabled: true,
        triggers: TriggerPolicy::ALL_ENABLED,
        collection_policy: crate::config::CollectionPolicy::FULL,
        filters: vec![],
        collectors: vec![],
        pre_processors: vec![],
        post_processors: vec![
            Box::new(ZIPArchiver),
            Box::new(StageContextProbe::new(
                "LateAfterCommit",
                PostProcessorPhase::AfterCommit,
                observations.clone(),
            )),
            Box::new(StageContextProbe::new(
                "LateAfterNotify",
                PostProcessorPhase::AfterNotify,
                observations.clone(),
            )),
            Box::new(StageContextProbe::new(
                "LateFinalCleanup",
                PostProcessorPhase::FinalCleanup,
                observations.clone(),
            )),
        ],
        notifiers: vec![Box::new(StageContextProbe::new(
            "LateNotifier",
            PostProcessorPhase::AfterCommit,
            observations,
        ))],
        shm: None,
        platform: Arc::new(MockPlatform::default()),
        output_dir: Some(root.path().to_path_buf()),
    };

    let diagnostics = pipeline.handle_event(&make_crash_event(), 123);
    let report_path = diagnostics.report_path.expect("committed ZIP report");
    assert_eq!(
        report_path
            .extension()
            .and_then(|extension| extension.to_str()),
        Some("zip")
    );
    let report_dir = report_path.parent().unwrap();
    let manifest = crate::pipeline::load_manifest(&report_dir.join("manifest.json")).unwrap();
    let final_plugins = manifest
        .final_diagnostics
        .as_ref()
        .and_then(|value| value.pointer("/plugins"))
        .and_then(serde_json::Value::as_object)
        .expect("terminal diagnostics in committed manifest");
    for name in [
        "LateAfterCommit",
        "LateNotifier",
        "LateAfterNotify",
        "LateFinalCleanup",
    ] {
        assert_eq!(final_plugins[name]["status"], "ok", "missing {name}");
    }

    let loaded = crate::pipeline::report::load_report(&report_path).unwrap();
    let overlaid_plugins = loaded
        .diagnostics
        .as_ref()
        .and_then(|value| value.pointer("/plugins"))
        .and_then(serde_json::Value::as_object)
        .expect("loader-overlaid diagnostics");
    assert_eq!(overlaid_plugins["LateFinalCleanup"]["status"], "ok");

    assert_eq!(manifest.artifacts.len(), 1);
    assert_eq!(manifest.artifacts[0].path, "report.zip");
    assert_eq!(manifest.artifacts[0].kind, ArtifactKind::Archive);
    assert_eq!(
        manifest.artifacts[0].size,
        std::fs::metadata(&report_path).unwrap().len()
    );
    let mut names = std::fs::read_dir(report_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect::<Vec<_>>();
    names.sort();
    assert_eq!(names, ["manifest.json", "report.zip"]);
}

#[test]
fn termination_report_persists_diagnostics_after_final_cleanup() {
    let root = tempfile::tempdir().unwrap();
    let observations = Arc::new(Mutex::new(Vec::new()));
    let pipeline = Pipeline {
        enabled: true,
        triggers: TriggerPolicy::ALL_ENABLED,
        collection_policy: crate::config::CollectionPolicy::FULL,
        filters: vec![],
        collectors: vec![],
        pre_processors: vec![],
        post_processors: vec![Box::new(StageContextProbe::new(
            "TerminationFinalCleanup",
            PostProcessorPhase::FinalCleanup,
            observations,
        ))],
        notifiers: vec![],
        shm: None,
        platform: Arc::new(MockPlatform::default()),
        output_dir: Some(root.path().to_path_buf()),
    };
    let event = CrashEvent {
        report_id: crate::pipeline::ReportId::default(),
        report_type: ReportType::ExitFailure,
        termination: Some(TerminationReason::Exited {
            exit_code: 17,
            runtime_ms: 250,
        }),
        exception_type: None,
        exception_code: None,
        exception_subcode: None,
        exception_codes: Vec::new(),
        crashed_thread: None,
        bail_on_suspend_failure: false,
        pid: 4321,
        process_name: "terminated-test".into(),
        hang_duration_ms: None,
    };

    let diagnostics = pipeline.handle_termination_event(&event);
    let report_path = diagnostics.report_path.expect("termination report");
    let loaded = crate::pipeline::report::load_report(&report_path).unwrap();
    assert_eq!(
        loaded
            .diagnostics
            .as_ref()
            .and_then(|value| value.pointer("/plugins/TerminationFinalCleanup/status")),
        Some(&serde_json::json!("ok"))
    );
    let manifest =
        crate::pipeline::load_manifest(&report_path.parent().unwrap().join("manifest.json"))
            .unwrap();
    assert_eq!(
        manifest
            .final_diagnostics
            .as_ref()
            .and_then(|value| value.pointer("/plugins/TerminationFinalCleanup/status")),
        Some(&serde_json::json!("ok"))
    );
}

#[test]
fn post_publish_sync_failure_keeps_report_notifier_and_diagnostic() {
    let root = tempfile::tempdir().unwrap();
    let notified = Arc::new(Mutex::new(None));
    let pipeline = Pipeline {
        enabled: true,
        triggers: TriggerPolicy::ALL_ENABLED,
        collection_policy: crate::config::CollectionPolicy::FULL,
        filters: vec![],
        collectors: vec![],
        pre_processors: vec![],
        post_processors: vec![],
        notifiers: vec![Box::new(CfgNotifier {
            name: "DurabilityNotifier",
            available: true,
            captured: notified.clone(),
        })],
        shm: None,
        platform: Arc::new(MockPlatform::default()),
        output_dir: Some(root.path().to_path_buf()),
    };

    let diagnostics =
        crate::pipeline::artifact::with_test_directory_sync_failure(root.path(), || {
            pipeline.handle_event(&make_crash_event(), 123)
        });

    let report_path = diagnostics
        .report_path
        .as_ref()
        .expect("rename remains the publication boundary after fsync failure");
    assert!(report_path.is_file());
    assert_eq!(notified.lock().unwrap().as_ref(), Some(report_path));
    assert!(diagnostics.plugins.iter().any(|plugin| {
        plugin.name == "ArtifactDurability"
            && matches!(
                &plugin.status,
                PluginStatus::Error(error) if error.contains("simulated directory sync failure")
            )
    }));
}

struct PhaseDependencyPostProcessor {
    name: &'static str,
    phase: PostProcessorPhase,
    order_after: &'static [&'static str],
}

impl Plugin for PhaseDependencyPostProcessor {
    fn name(&self) -> &'static str {
        self.name
    }

    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }

    fn priority(&self) -> Priority {
        Priority::Low
    }

    fn order_after(&self) -> &'static [&'static str] {
        self.order_after
    }
}

impl PostProcessor for PhaseDependencyPostProcessor {
    fn phase(&self) -> PostProcessorPhase {
        self.phase
    }

    fn process(
        &self,
        _event: &CrashEvent,
        _result: &mut ReportResult,
        _context: &PluginContext,
    ) -> Result<(), String> {
        Ok(())
    }
}

#[test]
fn dependency_validation_rejects_dependencies_on_any_later_phase() {
    for (dependent_phase, dependency_phase) in [
        (
            PostProcessorPhase::BeforeCommit,
            PostProcessorPhase::AfterCommit,
        ),
        (
            PostProcessorPhase::BeforeCommit,
            PostProcessorPhase::AfterNotify,
        ),
        (
            PostProcessorPhase::BeforeCommit,
            PostProcessorPhase::FinalCleanup,
        ),
        (
            PostProcessorPhase::AfterCommit,
            PostProcessorPhase::AfterNotify,
        ),
        (
            PostProcessorPhase::AfterCommit,
            PostProcessorPhase::FinalCleanup,
        ),
        (
            PostProcessorPhase::AfterNotify,
            PostProcessorPhase::FinalCleanup,
        ),
    ] {
        let root = tempfile::tempdir().unwrap();
        let pipeline = make_full_pipeline(
            vec![],
            vec![],
            vec![],
            vec![
                Box::new(PhaseDependencyPostProcessor {
                    name: "LaterPhase",
                    phase: dependency_phase,
                    order_after: &[],
                }),
                Box::new(PhaseDependencyPostProcessor {
                    name: "EarlierPhase",
                    phase: dependent_phase,
                    order_after: &["LaterPhase"],
                }),
            ],
            vec![],
            root.path(),
        );

        assert!(matches!(
            pipeline.validate_dependencies(),
            Err(crate::config::ConfigValidationError::InvalidDependencyOrder {
                category: PluginCategory::PostProcessor,
                ref plugin_id,
                ref dependency,
                kind: DependencyKind::OrderOnly,
            }) if plugin_id == "EarlierPhase" && dependency == "LaterPhase"
        ));
    }
}

#[test]
fn dependency_validation_allows_dependencies_on_any_earlier_phase() {
    let root = tempfile::tempdir().unwrap();
    let pipeline = make_full_pipeline(
        vec![],
        vec![],
        vec![],
        vec![
            Box::new(PhaseDependencyPostProcessor {
                name: "StagingMutation",
                phase: PostProcessorPhase::BeforeCommit,
                order_after: &[],
            }),
            Box::new(PhaseDependencyPostProcessor {
                name: "PublishedState",
                phase: PostProcessorPhase::AfterCommit,
                order_after: &["StagingMutation"],
            }),
            Box::new(PhaseDependencyPostProcessor {
                name: "PostNotificationCleanup",
                phase: PostProcessorPhase::AfterNotify,
                order_after: &["StagingMutation", "PublishedState"],
            }),
            Box::new(PhaseDependencyPostProcessor {
                name: "TerminalMaintenance",
                phase: PostProcessorPhase::FinalCleanup,
                order_after: &[
                    "StagingMutation",
                    "PublishedState",
                    "PostNotificationCleanup",
                ],
            }),
        ],
        vec![],
        root.path(),
    );

    pipeline.validate_dependencies().unwrap();
}
