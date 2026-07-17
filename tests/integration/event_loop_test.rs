//! In-process integration tests for the extracted event loop.
//!
//! Uses `TestEventSource` to inject predetermined events, exercising the full
//! pipeline in-process (no external binary, no real Mach ports). This allows
//! `cargo llvm-cov` to instrument all code paths.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crash_monitor::event_loop::{
    AnrConfig, EXIT_CHILD_FAILURE, EXIT_DETECTED_CRASH, EXIT_MONITOR_INTERNAL, EventSource,
    MonitorEvent, MonitorOutcome, event_loop,
};
use crash_monitor::pipeline::{
    CollectedData, Collector, CrashEvent, Notifier, Pipeline, Plugin, PluginContext,
    PluginExecution, PluginStatus, PostProcessor, Priority, ReportResult, TerminationReason,
    TriggerPolicy,
};
use crash_monitor::platform::ReceivedMachMessage;
use crash_monitor::platform::mock::MockPlatform;
use crash_monitor::platform::{PlatformOps, RESUME_ATTEMPT_LIMIT};
use crash_monitor::postprocessors::ZIPArchiver;
use crash_monitor::shm::{
    CONTEXT_OFFSET, SECTION1_OFFSET, SHM_PRODUCER_READY, SharedMemory, ShmHeader, SutCrashContext,
};

type ReleaseGate = Arc<(Mutex<bool>, Condvar)>;

// ═══════════════════════════════════════════════════
//  TestEventSource
// ═══════════════════════════════════════════════════

struct TestEventSource {
    events: VecDeque<MonitorEvent>,
}

impl TestEventSource {
    fn new(events: Vec<MonitorEvent>) -> Self {
        Self {
            events: events.into(),
        }
    }
}

impl EventSource for TestEventSource {
    fn poll(&mut self) -> Option<MonitorEvent> {
        self.events.pop_front()
    }
}

/// A poll-by-poll script that can place idle iterations before a later event.
/// `TestEventSource` cannot represent these intentional `None` entries because
/// an empty queue is indistinguishable from an idle poll.
struct ScriptedPollSource {
    polls: VecDeque<Option<MonitorEvent>>,
}

impl ScriptedPollSource {
    fn new(polls: Vec<Option<MonitorEvent>>) -> Self {
        Self {
            polls: polls.into(),
        }
    }
}

impl EventSource for ScriptedPollSource {
    fn poll(&mut self) -> Option<MonitorEvent> {
        self.polls
            .pop_front()
            .expect("event loop exhausted the scripted poll sequence")
    }
}

// ═══════════════════════════════════════════════════
//  Helpers
// ═══════════════════════════════════════════════════

fn make_test_pipeline(tempdir: &std::path::Path) -> Arc<Pipeline> {
    make_test_pipeline_with_triggers(tempdir, TriggerPolicy::ALL_ENABLED)
}

fn make_test_pipeline_with_triggers(
    tempdir: &std::path::Path,
    triggers: TriggerPolicy,
) -> Arc<Pipeline> {
    Arc::new(Pipeline {
        enabled: true,
        triggers,
        filters: vec![],
        collectors: vec![],
        pre_processors: vec![],
        post_processors: vec![],
        notifiers: vec![],
        shm: None,
        platform: Arc::new(MockPlatform::default()),
        output_dir: Some(tempdir.to_path_buf()),
    })
}

fn unique_event_loop_shm_id() -> u32 {
    static NEXT_ID: AtomicU32 = AtomicU32::new(2_100_000);
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

fn publish_anr_readiness(shm: &SharedMemory, heartbeat_value: u64) {
    let heartbeat_offset =
        CONTEXT_OFFSET + std::mem::offset_of!(SutCrashContext, heartbeat_counter);
    let ready_offset = SECTION1_OFFSET + std::mem::offset_of!(ShmHeader, producer_ready);

    // SAFETY: both offsets are schema-asserted atomic locations inside this
    // test-owned mapping, which remains alive through the event loop call.
    #[allow(clippy::cast_ptr_alignment)]
    let heartbeat = unsafe { &*shm.base_ptr().add(heartbeat_offset).cast::<AtomicU64>() };
    #[allow(clippy::cast_ptr_alignment)]
    let ready = unsafe { &*shm.base_ptr().add(ready_offset).cast::<AtomicU32>() };
    heartbeat.store(heartbeat_value, Ordering::Release);
    ready.store(SHM_PRODUCER_READY, Ordering::Release);
}

fn count_json_files(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "json"))
        .count()
}

fn assert_no_artifacts(dir: &std::path::Path) {
    let entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .collect();
    assert!(
        entries.is_empty(),
        "expected no artifacts, found {entries:?}"
    );
}

fn read_only_report(dir: &std::path::Path) -> serde_json::Value {
    let mut reports = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"));
    let report = reports.next().expect("one JSON report");
    assert!(reports.next().is_none(), "expected exactly one JSON report");
    serde_json::from_slice(&std::fs::read(report.path()).unwrap()).unwrap()
}

fn read_all_reports(dir: &std::path::Path) -> Vec<serde_json::Value> {
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .map(|entry| serde_json::from_slice(&std::fs::read(entry.path()).unwrap()).unwrap())
        .collect()
}

fn assert_task_resume_diagnostic(dir: &std::path::Path) {
    let report = read_only_report(dir);
    let task_resume = report
        .pointer("/_diagnostics/plugins/TaskResume")
        .expect("TaskResume diagnostic in persisted report");
    assert_eq!(task_resume["status"], "error");
    assert!(
        task_resume["error"]
            .as_str()
            .is_some_and(|error| error.contains("resume attempted")),
        "unexpected TaskResume diagnostic: {task_resume}"
    );
}

fn exited(exit_code: i32, runtime_ms: u64) -> MonitorEvent {
    MonitorEvent::ChildTerminated(TerminationReason::Exited {
        exit_code,
        runtime_ms,
    })
}

fn signaled(signal: i32, core_dumped: bool, runtime_ms: u64) -> MonitorEvent {
    MonitorEvent::ChildTerminated(TerminationReason::Signaled {
        signal,
        core_dumped,
        runtime_ms,
    })
}

fn report_event(report_type: crash_monitor::pipeline::ReportType) -> CrashEvent {
    use crash_monitor::pipeline::ReportType;

    let termination = match report_type {
        ReportType::ExitFailure => Some(TerminationReason::Exited {
            exit_code: 23,
            runtime_ms: 10,
        }),
        ReportType::SignalFailure => Some(TerminationReason::Signaled {
            signal: 15,
            core_dumped: false,
            runtime_ms: 10,
        }),
        ReportType::Oom => Some(TerminationReason::Signaled {
            signal: 9,
            core_dumped: false,
            runtime_ms: 10,
        }),
        ReportType::Crash | ReportType::Snapshot | ReportType::Anr => None,
    };
    CrashEvent {
        report_type,
        exception_type: (report_type == ReportType::Crash).then_some(1),
        exception_code: (report_type == ReportType::Crash).then_some(0xDEAD),
        exception_subcode: (report_type == ReportType::Crash).then_some(0xBEEF),
        exception_codes: if report_type == ReportType::Crash {
            vec![0xDEAD, 0xBEEF]
        } else {
            Vec::new()
        },
        crashed_thread: (report_type == ReportType::Crash).then_some(42),
        bail_on_suspend_failure: matches!(report_type, ReportType::Snapshot | ReportType::Anr),
        pid: 9999,
        process_name: "test_app".to_string(),
        hang_duration_ms: (report_type == ReportType::Anr).then_some(5_000),
        termination,
    }
}

fn policy_with_disabled(report_type: crash_monitor::pipeline::ReportType) -> TriggerPolicy {
    use crash_monitor::pipeline::ReportType;

    let mut policy = TriggerPolicy::ALL_ENABLED;
    match report_type {
        ReportType::Crash => policy.crash = false,
        ReportType::Snapshot => policy.snapshot = false,
        ReportType::Anr => policy.anr = false,
        ReportType::Oom => policy.probable_oom = false,
        ReportType::ExitFailure => policy.exit_failure = false,
        ReportType::SignalFailure => policy.signal_failure = false,
    }
    policy
}

fn test_request() -> ReceivedMachMessage {
    ReceivedMachMessage::test_fixture(42).0
}

#[allow(clippy::unnecessary_wraps)] // signature matches the injected reply callback
fn noop_reply(_request: &mut ReceivedMachMessage) -> Result<(), String> {
    Ok(())
}

struct BlockingGate {
    entered_tx: SyncSender<()>,
    release: ReleaseGate,
}

impl BlockingGate {
    fn wait(&self) -> Result<(), String> {
        let _ = self.entered_tx.send(());
        let (lock, condvar) = &*self.release;
        let guard = match lock.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let wait = condvar
            .wait_timeout_while(guard, Duration::from_secs(5), |released| !*released)
            .map_err(|_| "blocking gate mutex poisoned".to_string())?;
        if wait.1.timed_out() {
            return Err("blocking gate timed out".into());
        }
        Ok(())
    }
}

struct BlockingPostProcessor {
    name: &'static str,
    gate: BlockingGate,
}

struct CountingCollector {
    calls: Arc<AtomicUsize>,
}

struct SlowCaptureCollector {
    delay: Duration,
}

impl Plugin for SlowCaptureCollector {
    fn name(&self) -> &'static str {
        "SlowCaptureCollector"
    }

    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }

    fn priority(&self) -> Priority {
        Priority::Critical
    }
}

impl Collector for SlowCaptureCollector {
    fn collect(
        &self,
        _event: &CrashEvent,
        _task: mach2::port::mach_port_t,
        _data: &mut CollectedData,
        _context: &PluginContext,
    ) -> Result<(), String> {
        std::thread::sleep(self.delay);
        Ok(())
    }
}

impl Plugin for CountingCollector {
    fn name(&self) -> &'static str {
        "CountingCollector"
    }

    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }

    fn priority(&self) -> Priority {
        Priority::Critical
    }
}

impl Collector for CountingCollector {
    fn collect(
        &self,
        _event: &CrashEvent,
        _task: mach2::port::mach_port_t,
        _data: &mut CollectedData,
        _context: &PluginContext,
    ) -> Result<(), String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct CountingPostProcessor {
    calls: Arc<AtomicUsize>,
}

impl Plugin for CountingPostProcessor {
    fn name(&self) -> &'static str {
        "CountingPostProcessor"
    }

    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }

    fn priority(&self) -> Priority {
        Priority::Low
    }
}

impl PostProcessor for CountingPostProcessor {
    fn process(
        &self,
        _event: &CrashEvent,
        _result: &mut ReportResult,
        _context: &PluginContext,
    ) -> Result<(), String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

fn disabled_pipeline(
    tempdir: &std::path::Path,
) -> (
    Arc<Pipeline>,
    Arc<MockPlatform>,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
) {
    let platform = Arc::new(MockPlatform::default());
    let collector_calls = Arc::new(AtomicUsize::new(0));
    let post_processor_calls = Arc::new(AtomicUsize::new(0));
    let pipeline = Arc::new(Pipeline {
        enabled: false,
        triggers: TriggerPolicy::ALL_ENABLED,
        filters: vec![],
        collectors: vec![Box::new(CountingCollector {
            calls: collector_calls.clone(),
        })],
        pre_processors: vec![],
        post_processors: vec![Box::new(CountingPostProcessor {
            calls: post_processor_calls.clone(),
        })],
        notifiers: vec![],
        shm: None,
        platform: platform.clone(),
        output_dir: Some(tempdir.to_path_buf()),
    });
    (pipeline, platform, collector_calls, post_processor_calls)
}

struct BlockingCollector {
    gate: BlockingGate,
}

impl Plugin for BlockingCollector {
    fn name(&self) -> &'static str {
        "BlockingCollector"
    }

    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }

    fn priority(&self) -> Priority {
        Priority::Critical
    }
}

impl Collector for BlockingCollector {
    fn collect(
        &self,
        _event: &CrashEvent,
        _task: mach2::port::mach_port_t,
        _data: &mut CollectedData,
        _context: &PluginContext,
    ) -> Result<(), String> {
        self.gate.wait()
    }
}

impl Plugin for BlockingPostProcessor {
    fn name(&self) -> &'static str {
        self.name
    }

    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }

    fn priority(&self) -> Priority {
        Priority::Low
    }
}

impl PostProcessor for BlockingPostProcessor {
    fn process(
        &self,
        _event: &CrashEvent,
        _result: &mut ReportResult,
        _context: &PluginContext,
    ) -> Result<(), String> {
        self.gate.wait()
    }
}

struct BlockingNotifier {
    gate: BlockingGate,
}

struct ThreadRecordingPostProcessor {
    thread_tx: SyncSender<std::thread::ThreadId>,
}

impl Plugin for ThreadRecordingPostProcessor {
    fn name(&self) -> &'static str {
        "ThreadRecordingPostProcessor"
    }

    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }

    fn priority(&self) -> Priority {
        Priority::Low
    }
}

impl PostProcessor for ThreadRecordingPostProcessor {
    fn process(
        &self,
        _event: &CrashEvent,
        _result: &mut ReportResult,
        _context: &PluginContext,
    ) -> Result<(), String> {
        let _ = self.thread_tx.send(std::thread::current().id());
        Ok(())
    }
}

impl Plugin for BlockingNotifier {
    fn name(&self) -> &'static str {
        "BlockingNotifier"
    }

    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }

    fn priority(&self) -> Priority {
        Priority::Low
    }
}

impl Notifier for BlockingNotifier {
    fn notify(&self, _report_path: &Path, _context: &PluginContext) -> Result<(), String> {
        self.gate.wait()
    }
}

fn blocking_gate() -> (BlockingGate, Receiver<()>, ReleaseGate) {
    let (entered_tx, entered_rx) = mpsc::sync_channel(1);
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    (
        BlockingGate {
            entered_tx,
            release: release.clone(),
        },
        entered_rx,
        release,
    )
}

fn release_gate(release: &ReleaseGate) {
    let (lock, condvar) = &**release;
    let mut released = match lock.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    *released = true;
    condvar.notify_all();
}

fn assert_blocking_finalizer_does_not_delay_reply(
    post_processors: Vec<Box<dyn PostProcessor>>,
    notifiers: Vec<Box<dyn Notifier>>,
    entered_rx: &Receiver<()>,
    release: &ReleaseGate,
) {
    let tempdir = tempfile::tempdir().unwrap();
    let platform = Arc::new(MockPlatform::default());
    let pipeline = Arc::new(Pipeline {
        enabled: true,
        triggers: TriggerPolicy::ALL_ENABLED,
        filters: vec![],
        collectors: vec![],
        pre_processors: vec![],
        post_processors,
        notifiers,
        shm: None,
        platform: platform.clone(),
        output_dir: Some(tempdir.path().to_path_buf()),
    });
    let mut source = TestEventSource::new(vec![MonitorEvent::Crash {
        received_at: std::time::Instant::now(),
        exception_type: 1,
        code: 0xDEAD,
        subcode: 0xBEEF,
        raw_codes: vec![0xDEAD, 0xBEEF],
        request: test_request(),
    }]);
    let (reply_tx, reply_rx) = mpsc::sync_channel(1);
    let platform_at_reply = platform.clone();

    let mut result = event_loop(
        &mut source,
        &pipeline,
        123,
        9999,
        "test_app",
        &|_| {
            assert_eq!(
                platform_at_reply.resume_count(),
                1,
                "resume must happen before the Mach reply callback"
            );
            let _ = reply_tx.send(());
            Ok(())
        },
        None,
        None,
    );

    reply_rx
        .recv_timeout(Duration::from_millis(500))
        .expect("Mach reply must precede finalization");
    assert_eq!(platform.resume_count(), 1, "task must already be resumed");
    assert!(
        matches!(entered_rx.try_recv(), Err(mpsc::TryRecvError::Empty)),
        "fatal finalizer must wait for the termination handoff"
    );

    let finalization = result
        .crash_finalization
        .take()
        .expect("fatal finalization ticket");
    let pipeline_for_completion = pipeline.clone();
    let completion = std::thread::spawn(move || {
        finalization.complete(
            pipeline_for_completion,
            Some(TerminationReason::Signaled {
                signal: 11,
                core_dumped: true,
                runtime_ms: 25,
            }),
            Duration::from_secs(3),
        )
    });

    entered_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("blocking finalization stage should start after handoff");
    release_gate(release);
    let diagnostics = completion
        .join()
        .expect("completion thread")
        .expect("fatal finalizer result");
    assert!(diagnostics.report_path.is_some());
}

// ═══════════════════════════════════════════════════
//  Tests
// ═══════════════════════════════════════════════════

fn assert_pipeline_rejects_without_work(
    enabled: bool,
    triggers: TriggerPolicy,
    report_type: crash_monitor::pipeline::ReportType,
) {
    let tempdir = tempfile::tempdir().unwrap();
    let platform = Arc::new(MockPlatform::default());
    let collector_calls = Arc::new(AtomicUsize::new(0));
    let postprocessor_calls = Arc::new(AtomicUsize::new(0));
    let pipeline = Pipeline {
        enabled,
        triggers,
        filters: vec![],
        collectors: vec![Box::new(CountingCollector {
            calls: collector_calls.clone(),
        })],
        pre_processors: vec![],
        post_processors: vec![Box::new(CountingPostProcessor {
            calls: postprocessor_calls.clone(),
        })],
        notifiers: vec![],
        shm: None,
        platform: platform.clone(),
        output_dir: Some(tempdir.path().to_path_buf()),
    };
    let event = report_event(report_type);

    let diagnostics = if event.termination.is_some() {
        pipeline.handle_termination_event(&event)
    } else {
        pipeline.handle_event(&event, 123)
    };

    assert!(diagnostics.plugins.is_empty());
    assert!(diagnostics.report_path.is_none());
    assert_eq!(platform.suspend_count(), 0, "{report_type:?}");
    assert_eq!(platform.resume_count(), 0, "{report_type:?}");
    assert_eq!(collector_calls.load(Ordering::SeqCst), 0, "{report_type:?}");
    assert_eq!(
        postprocessor_calls.load(Ordering::SeqCst),
        0,
        "{report_type:?}"
    );
    assert_no_artifacts(tempdir.path());
}

#[test]
fn test_global_disabled_is_a_no_work_no_artifact_kill_switch_for_every_trigger() {
    use crash_monitor::pipeline::ReportType;

    for report_type in [
        ReportType::Crash,
        ReportType::ExitFailure,
        ReportType::SignalFailure,
        ReportType::Oom,
        ReportType::Anr,
        ReportType::Snapshot,
    ] {
        assert_pipeline_rejects_without_work(false, TriggerPolicy::ALL_ENABLED, report_type);
    }
}

#[test]
fn test_each_disabled_trigger_rejects_only_its_report_path_before_work() {
    use crash_monitor::pipeline::ReportType;

    for report_type in [
        ReportType::Crash,
        ReportType::ExitFailure,
        ReportType::SignalFailure,
        ReportType::Oom,
        ReportType::Anr,
        ReportType::Snapshot,
    ] {
        assert_pipeline_rejects_without_work(true, policy_with_disabled(report_type), report_type);
    }
}

#[test]
fn test_disabled_crash_trigger_replies_without_worker_capture_or_artifacts() {
    let tempdir = tempfile::tempdir().unwrap();
    let platform = Arc::new(MockPlatform::default());
    let collector_calls = Arc::new(AtomicUsize::new(0));
    let postprocessor_calls = Arc::new(AtomicUsize::new(0));
    let pipeline = Arc::new(Pipeline {
        enabled: true,
        triggers: TriggerPolicy {
            crash: false,
            ..TriggerPolicy::ALL_ENABLED
        },
        filters: vec![],
        collectors: vec![Box::new(CountingCollector {
            calls: collector_calls.clone(),
        })],
        pre_processors: vec![],
        post_processors: vec![Box::new(CountingPostProcessor {
            calls: postprocessor_calls.clone(),
        })],
        notifiers: vec![],
        shm: None,
        platform: platform.clone(),
        output_dir: Some(tempdir.path().to_path_buf()),
    });
    let mut source = TestEventSource::new(vec![MonitorEvent::Crash {
        received_at: std::time::Instant::now(),
        exception_type: 1,
        code: 0xDEAD,
        subcode: 0xBEEF,
        raw_codes: vec![0xDEAD, 0xBEEF],
        request: test_request(),
    }]);
    let replies = AtomicUsize::new(0);

    let result = event_loop(
        &mut source,
        &pipeline,
        123,
        9999,
        "test_app",
        &|_| {
            replies.fetch_add(1, Ordering::SeqCst);
            Ok(())
        },
        None,
        None,
    );

    assert!(matches!(
        result.outcome,
        MonitorOutcome::DetectedCrash { .. }
    ));
    assert!(result.crash_finalization.is_none());
    assert_eq!(replies.load(Ordering::SeqCst), 1);
    assert_eq!(platform.suspend_count(), 0);
    assert_eq!(platform.resume_count(), 0);
    assert_eq!(collector_calls.load(Ordering::SeqCst), 0);
    assert_eq!(postprocessor_calls.load(Ordering::SeqCst), 0);
    assert_no_artifacts(tempdir.path());
}

#[test]
fn test_reply_failure_is_monitor_failure_with_finalization_ticket_and_cleanup() {
    let tempdir = tempfile::tempdir().unwrap();
    let pipeline = make_test_pipeline(tempdir.path());
    let (request, destroys) = ReceivedMachMessage::test_fixture(42);
    let mut source = TestEventSource::new(vec![MonitorEvent::Crash {
        received_at: std::time::Instant::now(),
        exception_type: 1,
        code: 0xDEAD,
        subcode: 0xBEEF,
        raw_codes: vec![0xDEAD, 0xBEEF],
        request,
    }]);

    let mut result = event_loop(
        &mut source,
        &pipeline,
        123,
        9999,
        "test_app",
        &|_| Err("timed out".to_string()),
        None,
        None,
    );

    assert!(matches!(
        &result.outcome,
        MonitorOutcome::MonitorFailure(message)
            if message.contains("failed to send deferred Mach exception reply")
    ));
    assert_eq!(result.exit_code(), EXIT_MONITOR_INTERNAL);
    assert!(result.crash_cleanup_required);
    assert_eq!(destroys.load(Ordering::SeqCst), 1);

    let termination = TerminationReason::Signaled {
        signal: 11,
        core_dumped: true,
        runtime_ms: 25,
    };
    let diagnostics = result
        .crash_finalization
        .take()
        .expect("captured crash must retain its finalization ticket")
        .complete(pipeline, Some(termination), Duration::from_secs(2))
        .expect("fatal finalization result");
    assert!(diagnostics.report_path.is_some());
    result.outcome = result
        .outcome
        .with_crash_result(Some(termination), diagnostics.report_path);
    assert!(matches!(result.outcome, MonitorOutcome::MonitorFailure(_)));
}

#[test]
fn test_disabled_event_loop_ignores_snapshot_and_abnormal_exit_without_artifacts() {
    let tempdir = tempfile::tempdir().unwrap();
    let (pipeline, platform, collector_calls, post_processor_calls) =
        disabled_pipeline(tempdir.path());
    let mut source = TestEventSource::new(vec![MonitorEvent::Snapshot, exited(23, 15)]);

    let result = event_loop(
        &mut source,
        &pipeline,
        123,
        9999,
        "test_app",
        &noop_reply,
        None,
        None,
    );

    assert_eq!(result.exit_code(), EXIT_CHILD_FAILURE);
    assert_eq!(platform.suspend_count(), 0);
    assert_eq!(platform.resume_count(), 0);
    assert_eq!(collector_calls.load(Ordering::SeqCst), 0);
    assert_eq!(post_processor_calls.load(Ordering::SeqCst), 0);
    assert_no_artifacts(tempdir.path());
}

#[test]
fn test_crash_event_produces_report_and_exits() {
    let tempdir = tempfile::tempdir().unwrap();
    let pipeline = make_test_pipeline(tempdir.path());

    let mut source = TestEventSource::new(vec![
        MonitorEvent::Crash {
            received_at: std::time::Instant::now(),
            exception_type: 1,
            code: 0xDEAD,
            subcode: 0xBEEF,
            raw_codes: vec![0xDEAD, 0xBEEF],
            request: test_request(),
        },
        exited(0, 10),
    ]);

    let mut outcome = event_loop(
        &mut source,
        &pipeline,
        0,
        9999,
        "test_app",
        &noop_reply,
        None,
        None,
    );
    assert_eq!(count_json_files(tempdir.path()), 0);
    let expected_termination = TerminationReason::Signaled {
        signal: 11,
        core_dumped: true,
        runtime_ms: 10,
    };
    let diagnostics = outcome
        .crash_finalization
        .take()
        .expect("fatal finalization handoff")
        .complete(
            pipeline.clone(),
            Some(expected_termination),
            std::time::Duration::from_secs(2),
        )
        .expect("fatal finalizer result");
    outcome.outcome = outcome
        .outcome
        .with_crash_result(Some(expected_termination), diagnostics.report_path);

    let report_path = match &outcome.outcome {
        MonitorOutcome::DetectedCrash {
            termination,
            report_path: Some(path),
        } => {
            assert_eq!(*termination, Some(expected_termination));
            path
        }
        other => panic!("unexpected outcome: {other:?}"),
    };
    assert!(report_path.exists());
    assert_eq!(outcome.report_path(), Some(report_path.as_path()));
    assert_eq!(outcome.exit_code(), EXIT_DETECTED_CRASH);

    assert!(
        count_json_files(tempdir.path()) >= 1,
        "Should produce a JSON report"
    );
}

#[test]
fn test_feedback_hang_cannot_delay_resume_or_mach_reply() {
    let (gate, entered_rx, release) = blocking_gate();
    assert_blocking_finalizer_does_not_delay_reply(
        vec![Box::new(BlockingPostProcessor {
            name: "FeedbackDialog",
            gate,
        })],
        vec![],
        &entered_rx,
        &release,
    );
}

#[test]
fn test_zip_hang_cannot_delay_resume_or_mach_reply() {
    let (gate, entered_rx, release) = blocking_gate();
    assert_blocking_finalizer_does_not_delay_reply(
        vec![Box::new(BlockingPostProcessor {
            name: "ZIPArchiver",
            gate,
        })],
        vec![],
        &entered_rx,
        &release,
    );
}

#[test]
fn test_notifier_hang_cannot_delay_resume_or_mach_reply() {
    let (gate, entered_rx, release) = blocking_gate();
    assert_blocking_finalizer_does_not_delay_reply(
        vec![],
        vec![Box::new(BlockingNotifier { gate })],
        &entered_rx,
        &release,
    );
}

#[test]
fn test_capture_timeout_uses_absolute_mach_receive_deadline() {
    let tempdir = tempfile::tempdir().unwrap();
    let platform = Arc::new(MockPlatform::default());
    let (gate, entered_rx, release) = blocking_gate();
    let pipeline = Arc::new(Pipeline {
        enabled: true,
        triggers: TriggerPolicy::ALL_ENABLED,
        filters: vec![],
        collectors: vec![Box::new(BlockingCollector { gate })],
        pre_processors: vec![],
        post_processors: vec![],
        notifiers: vec![],
        shm: None,
        platform: platform.clone(),
        output_dir: Some(tempdir.path().to_path_buf()),
    });
    let received_at = std::time::Instant::now()
        .checked_sub(Duration::from_millis(4_900))
        .expect("recent monotonic timestamp");
    let mut source = TestEventSource::new(vec![MonitorEvent::Crash {
        received_at,
        exception_type: 1,
        code: 0xDEAD,
        subcode: 0xBEEF,
        raw_codes: vec![0xDEAD, 0xBEEF],
        request: test_request(),
    }]);
    let (reply_tx, reply_rx) = mpsc::sync_channel(1);
    let started = std::time::Instant::now();

    let mut result = event_loop(
        &mut source,
        &pipeline,
        123,
        9999,
        "test_app",
        &|_| {
            let _ = reply_tx.send(());
            Ok(())
        },
        None,
        None,
    );

    assert!(
        started.elapsed() < Duration::from_secs(1),
        "capture must use the remaining absolute budget, not a fresh five seconds"
    );
    entered_rx
        .recv_timeout(Duration::from_millis(500))
        .expect("collector entered before the deadline");
    reply_rx
        .recv_timeout(Duration::from_millis(500))
        .expect("reply follows timeout resume");
    assert_eq!(platform.suspend_count(), 1);
    assert_eq!(platform.resume_count(), 1);

    release_gate(&release);
    let diagnostics = result
        .crash_finalization
        .take()
        .expect("minimum fatal capture should still be finalized")
        .complete(
            pipeline.clone(),
            Some(TerminationReason::Signaled {
                signal: 11,
                core_dumped: false,
                runtime_ms: 5_100,
            }),
            Duration::from_secs(2),
        )
        .expect("fatal finalizer result");
    assert!(diagnostics.report_path.is_some());
}

#[test]
fn test_fatal_zip_is_created_with_termination_before_archiving() {
    let tempdir = tempfile::tempdir().unwrap();
    let pipeline = Arc::new(Pipeline {
        enabled: true,
        triggers: TriggerPolicy::ALL_ENABLED,
        filters: vec![],
        collectors: vec![],
        pre_processors: vec![],
        post_processors: vec![Box::new(ZIPArchiver)],
        notifiers: vec![],
        shm: None,
        platform: Arc::new(MockPlatform::default()),
        output_dir: Some(tempdir.path().to_path_buf()),
    });
    let mut source = TestEventSource::new(vec![MonitorEvent::Crash {
        received_at: std::time::Instant::now(),
        exception_type: 1,
        code: 0xDEAD,
        subcode: 0xBEEF,
        raw_codes: vec![0xDEAD, 0xBEEF],
        request: test_request(),
    }]);
    let mut result = event_loop(
        &mut source,
        &pipeline,
        123,
        9999,
        "test_app",
        &noop_reply,
        None,
        None,
    );
    let expected = TerminationReason::Signaled {
        signal: 11,
        core_dumped: true,
        runtime_ms: 77,
    };

    let diagnostics = result
        .crash_finalization
        .take()
        .expect("fatal finalization ticket")
        .complete(pipeline, Some(expected), Duration::from_secs(2))
        .expect("fatal finalizer result");
    let path = diagnostics.report_path.expect("ZIP artifact");
    assert_eq!(path.extension().and_then(|ext| ext.to_str()), Some("zip"));
    let report = crash_monitor::pipeline::report::load_report(&path).expect("report inside ZIP");
    assert_eq!(report.termination, Some(expected));
}

#[test]
fn test_snapshot_event_continues() {
    let tempdir = tempfile::tempdir().unwrap();
    let pipeline = make_test_pipeline(tempdir.path());

    let mut source = TestEventSource::new(vec![MonitorEvent::Snapshot, exited(0, 25)]);

    let outcome = event_loop(
        &mut source,
        &pipeline,
        0,
        9999,
        "test_app",
        &noop_reply,
        None,
        None,
    );
    assert_eq!(outcome.exit_code(), 0, "Snapshot should not terminate");

    let json_count = count_json_files(tempdir.path());
    assert!(
        json_count >= 1,
        "Should produce a snapshot report, got {json_count}"
    );
}

#[test]
fn test_unclaimed_shm_never_arms_anr_watchdog() {
    let tempdir = tempfile::tempdir().unwrap();
    let shm = Arc::new(
        SharedMemory::create(unique_event_loop_shm_id()).expect("create zeroed shared memory"),
    );
    let platform = Arc::new(MockPlatform::default());
    let pipeline = Arc::new(Pipeline {
        enabled: true,
        triggers: TriggerPolicy::ALL_ENABLED,
        filters: vec![],
        collectors: vec![],
        pre_processors: vec![],
        post_processors: vec![],
        notifiers: vec![],
        shm: Some(shm.clone()),
        platform: platform.clone(),
        output_dir: Some(tempdir.path().to_path_buf()),
    });
    // Two idle iterations exceed this threshold by a wide margin. Without the
    // producer-ready gate, the monitor-initialized zero heartbeat fires ANR.
    let anr_config = AnrConfig {
        warmup_ms: 0,
        threshold_ms: 20,
        check_interval_ms: 5,
        cooldown_ms: 0,
    };
    let mut source = ScriptedPollSource::new(vec![None, None, Some(exited(0, 125))]);

    let result = event_loop(
        &mut source,
        &pipeline,
        123,
        9999,
        "test_app",
        &noop_reply,
        Some(&shm),
        Some(&anr_config),
    );

    assert_eq!(result.exit_code(), 0);
    assert_eq!(platform.suspend_count(), 0);
    assert_eq!(platform.resume_count(), 0);
    assert_no_artifacts(tempdir.path());
}

#[test]
fn test_ready_stale_heartbeat_triggers_anr_capture() {
    let tempdir = tempfile::tempdir().unwrap();
    let shm =
        Arc::new(SharedMemory::create(unique_event_loop_shm_id()).expect("create shared memory"));
    publish_anr_readiness(&shm, 7);
    let platform = Arc::new(MockPlatform::default());
    let pipeline = Arc::new(Pipeline {
        enabled: true,
        triggers: TriggerPolicy::ALL_ENABLED,
        filters: vec![],
        collectors: vec![],
        pre_processors: vec![],
        post_processors: vec![],
        notifiers: vec![],
        shm: Some(shm.clone()),
        platform: platform.clone(),
        output_dir: Some(tempdir.path().to_path_buf()),
    });
    let anr_config = AnrConfig {
        warmup_ms: 0,
        threshold_ms: 100,
        check_interval_ms: 5,
        cooldown_ms: 1_000,
    };
    let mut source = ScriptedPollSource::new(vec![None, None, None, Some(exited(0, 175))]);

    let result = event_loop(
        &mut source,
        &pipeline,
        123,
        9999,
        "test_app",
        &noop_reply,
        Some(&shm),
        Some(&anr_config),
    );

    assert_eq!(result.exit_code(), 0);
    assert_eq!(platform.suspend_count(), 1, "ANR capture must suspend once");
    assert_eq!(platform.resume_count(), 1, "ANR capture must resume once");
    let report = read_only_report(tempdir.path());
    assert_eq!(report["header"]["type"], "anr");
    assert!(
        report["header"]["hang_duration_ms"]
            .as_u64()
            .is_some_and(|duration| duration >= anr_config.threshold_ms),
        "ANR report must retain the detected hang duration"
    );
}

#[test]
fn test_slow_anr_capture_time_does_not_trigger_a_second_anr() {
    let tempdir = tempfile::tempdir().unwrap();
    let shm =
        Arc::new(SharedMemory::create(unique_event_loop_shm_id()).expect("create shared memory"));
    publish_anr_readiness(&shm, 11);
    let platform = Arc::new(MockPlatform::default());
    let pipeline = Arc::new(Pipeline {
        enabled: true,
        triggers: TriggerPolicy::ALL_ENABLED,
        filters: vec![],
        collectors: vec![Box::new(SlowCaptureCollector {
            delay: Duration::from_millis(350),
        })],
        pre_processors: vec![],
        post_processors: vec![],
        notifiers: vec![],
        shm: Some(shm.clone()),
        platform: platform.clone(),
        output_dir: Some(tempdir.path().to_path_buf()),
    });
    let anr_config = AnrConfig {
        warmup_ms: 0,
        threshold_ms: 200,
        check_interval_ms: 5,
        cooldown_ms: 0,
    };
    // The first five idle polls make the ready heartbeat stale and cause one
    // ANR. Capture then takes longer than the threshold. With cooldown disabled,
    // the following idle poll would immediately cause a second ANR unless the
    // event loop rebased its heartbeat and clock after the child resumed.
    let mut source = ScriptedPollSource::new(vec![
        None,
        None,
        None,
        None,
        None,
        None,
        Some(exited(0, 700)),
    ]);

    let result = event_loop(
        &mut source,
        &pipeline,
        123,
        9999,
        "test_app",
        &noop_reply,
        Some(&shm),
        Some(&anr_config),
    );

    assert_eq!(result.exit_code(), 0);
    assert_eq!(platform.suspend_count(), 1, "exactly one ANR may suspend");
    assert_eq!(
        platform.resume_count(),
        1,
        "the ANR must resume exactly once"
    );
    let report = read_only_report(tempdir.path());
    assert_eq!(report["header"]["type"], "anr");
}

#[test]
fn test_slow_snapshot_monitor_time_does_not_trigger_false_anr() {
    let tempdir = tempfile::tempdir().unwrap();
    let shm =
        Arc::new(SharedMemory::create(unique_event_loop_shm_id()).expect("create shared memory"));
    publish_anr_readiness(&shm, 7);
    let platform = Arc::new(MockPlatform::default());
    let pipeline = Arc::new(Pipeline {
        enabled: true,
        triggers: TriggerPolicy::ALL_ENABLED,
        filters: vec![],
        collectors: vec![Box::new(SlowCaptureCollector {
            delay: Duration::from_millis(175),
        })],
        pre_processors: vec![],
        post_processors: vec![],
        notifiers: vec![],
        shm: Some(shm.clone()),
        platform: platform.clone(),
        output_dir: Some(tempdir.path().to_path_buf()),
    });
    let anr_config = AnrConfig {
        warmup_ms: 0,
        threshold_ms: 200,
        check_interval_ms: 5,
        cooldown_ms: 0,
    };
    // Let the child run for one poll interval, then spend almost the complete
    // threshold inside Snapshot capture. The post-capture poll keeps total
    // application-running stale time below the threshold, while uncorrected
    // wall time exceeds it. Only the monitor-owned interval must be excluded;
    // the pure watchdog test separately verifies that real pre-capture stale
    // time remains accumulated.
    let mut source = ScriptedPollSource::new(vec![
        None,
        Some(MonitorEvent::Snapshot),
        None,
        Some(exited(0, 250)),
    ]);

    let result = event_loop(
        &mut source,
        &pipeline,
        123,
        9999,
        "test_app",
        &noop_reply,
        Some(&shm),
        Some(&anr_config),
    );

    assert_eq!(result.exit_code(), 0);
    assert_eq!(platform.suspend_count(), 1, "only Snapshot may suspend");
    assert_eq!(
        platform.resume_count(),
        1,
        "Snapshot must resume exactly once"
    );
    let report = read_only_report(tempdir.path());
    assert_eq!(report["header"]["type"], "snapshot");
}

#[test]
fn test_snapshot_preserves_real_stale_time_around_excluded_monitor_work() {
    let tempdir = tempfile::tempdir().unwrap();
    let shm =
        Arc::new(SharedMemory::create(unique_event_loop_shm_id()).expect("create shared memory"));
    publish_anr_readiness(&shm, 13);
    let platform = Arc::new(MockPlatform::default());
    let pipeline = Arc::new(Pipeline {
        enabled: true,
        triggers: TriggerPolicy::ALL_ENABLED,
        filters: vec![],
        collectors: vec![Box::new(SlowCaptureCollector {
            delay: Duration::from_millis(350),
        })],
        pre_processors: vec![],
        post_processors: vec![],
        notifiers: vec![],
        shm: Some(shm.clone()),
        platform: platform.clone(),
        output_dir: Some(tempdir.path().to_path_buf()),
    });
    let anr_config = AnrConfig {
        warmup_ms: 0,
        threshold_ms: 250,
        check_interval_ms: 5,
        cooldown_ms: 1_000,
    };
    // Application-running stale time on either side of Snapshot is below the
    // threshold, but their sum reaches it. Capture itself is longer than the
    // threshold and must be excluded. A full post-event reset would miss the
    // ANR; raw wall-clock accounting would report it too early.
    let mut source = ScriptedPollSource::new(vec![
        None,
        None,
        None,
        Some(MonitorEvent::Snapshot),
        None,
        None,
        None,
        Some(exited(0, 1_100)),
    ]);

    let result = event_loop(
        &mut source,
        &pipeline,
        123,
        9999,
        "test_app",
        &noop_reply,
        Some(&shm),
        Some(&anr_config),
    );

    assert_eq!(result.exit_code(), 0);
    assert_eq!(
        platform.suspend_count(),
        2,
        "Snapshot and the preserved-time ANR must each suspend once"
    );
    assert_eq!(platform.resume_count(), 2);
    let reports = read_all_reports(tempdir.path());
    assert_eq!(reports.len(), 2, "expected one Snapshot and one ANR report");
    let mut report_types = reports
        .iter()
        .filter_map(|report| report["header"]["type"].as_str())
        .collect::<Vec<_>>();
    report_types.sort_unstable();
    assert_eq!(report_types, ["anr", "snapshot"]);
    let anr = reports
        .iter()
        .find(|report| report["header"]["type"] == "anr")
        .expect("ANR report");
    assert!(
        anr["header"]["hang_duration_ms"]
            .as_u64()
            .is_some_and(|duration| duration >= anr_config.threshold_ms)
    );
}

#[test]
fn test_resume_and_terminate_failure_becomes_monitor_failure() {
    let tempdir = tempfile::tempdir().unwrap();
    let mut platform = MockPlatform::default();
    platform.resume_fails = true;
    platform.terminate_fails = true;
    let platform = Arc::new(platform);
    let pipeline = Arc::new(Pipeline {
        enabled: true,
        triggers: TriggerPolicy::ALL_ENABLED,
        filters: vec![],
        collectors: vec![],
        pre_processors: vec![],
        post_processors: vec![],
        notifiers: vec![],
        shm: None,
        platform: platform.clone(),
        output_dir: Some(tempdir.path().to_path_buf()),
    });
    let mut source = TestEventSource::new(vec![MonitorEvent::Snapshot, exited(0, 25)]);

    let result = event_loop(
        &mut source,
        &pipeline,
        123,
        9999,
        "test_app",
        &noop_reply,
        None,
        None,
    );

    assert!(matches!(
        result.outcome,
        MonitorOutcome::MonitorFailure(ref message)
            if message.contains("task-control containment activated")
    ));
    assert_eq!(result.exit_code(), EXIT_MONITOR_INTERNAL);
    assert_eq!(platform.suspend_count(), 1);
    assert_eq!(platform.resume_count(), RESUME_ATTEMPT_LIMIT);
    assert_eq!(platform.terminate_count(), 1);
    assert!(platform.supervisor_health().requires_escalation());
    assert_task_resume_diagnostic(tempdir.path());
}

#[test]
fn test_bounded_resume_failure_and_task_termination_stops_monitoring() {
    let tempdir = tempfile::tempdir().unwrap();
    let mut platform = MockPlatform::default();
    platform.resume_fails = true;
    let platform = Arc::new(platform);
    let pipeline = Arc::new(Pipeline {
        enabled: true,
        triggers: TriggerPolicy::ALL_ENABLED,
        filters: vec![],
        collectors: vec![],
        pre_processors: vec![],
        post_processors: vec![],
        notifiers: vec![],
        shm: None,
        platform: platform.clone(),
        output_dir: Some(tempdir.path().to_path_buf()),
    });
    let mut source = TestEventSource::new(vec![MonitorEvent::Snapshot, exited(0, 25)]);

    let result = event_loop(
        &mut source,
        &pipeline,
        123,
        9999,
        "test_app",
        &noop_reply,
        None,
        None,
    );

    assert!(matches!(
        result.outcome,
        MonitorOutcome::MonitorFailure(ref message)
            if message.contains("recovery=Terminated")
    ));
    assert_eq!(platform.resume_count(), RESUME_ATTEMPT_LIMIT);
    assert_eq!(platform.terminate_count(), 1);
    assert!(!platform.supervisor_health().requires_escalation());
    assert_task_resume_diagnostic(tempdir.path());
}

#[test]
fn test_crash_resume_escalation_still_replies_before_monitor_failure() {
    let tempdir = tempfile::tempdir().unwrap();
    let mut platform = MockPlatform::default();
    platform.resume_fails = true;
    platform.terminate_fails = true;
    let platform = Arc::new(platform);
    let pipeline = Arc::new(Pipeline {
        enabled: true,
        triggers: TriggerPolicy::ALL_ENABLED,
        filters: vec![],
        collectors: vec![],
        pre_processors: vec![],
        post_processors: vec![],
        notifiers: vec![],
        shm: None,
        platform: platform.clone(),
        output_dir: Some(tempdir.path().to_path_buf()),
    });
    let mut source = TestEventSource::new(vec![MonitorEvent::Crash {
        received_at: std::time::Instant::now(),
        exception_type: 1,
        code: 0xDEAD,
        subcode: 0xBEEF,
        raw_codes: vec![0xDEAD, 0xBEEF],
        request: test_request(),
    }]);
    let reply_count = AtomicUsize::new(0);

    let mut result = event_loop(
        &mut source,
        &pipeline,
        123,
        9999,
        "test_app",
        &|_| {
            assert_eq!(platform.resume_count(), RESUME_ATTEMPT_LIMIT);
            assert_eq!(platform.terminate_count(), 1);
            reply_count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        },
        None,
        None,
    );

    assert_eq!(reply_count.load(Ordering::SeqCst), 1);
    assert!(matches!(&result.outcome, MonitorOutcome::MonitorFailure(_)));
    assert!(result.crash_cleanup_required);
    let diagnostics = result
        .crash_finalization
        .take()
        .expect("fatal containment preserves captured diagnostics")
        .complete(
            pipeline.clone(),
            Some(TerminationReason::Signaled {
                signal: 9,
                core_dumped: false,
                runtime_ms: 25,
            }),
            Duration::from_secs(2),
        )
        .expect("fatal containment finalization result");
    assert!(diagnostics.plugins.iter().any(|entry| {
        entry.name == "TaskResume" && matches!(entry.status, PluginStatus::Error(_))
    }));
    assert!(diagnostics.report_path.is_some());
    result.outcome = result
        .outcome
        .with_crash_result(None, diagnostics.report_path.clone());
    assert!(matches!(result.outcome, MonitorOutcome::MonitorFailure(_)));
    assert_eq!(result.exit_code(), EXIT_MONITOR_INTERNAL);
    assert_task_resume_diagnostic(tempdir.path());
}

#[test]
fn test_disabled_snapshot_trigger_does_not_suspend_or_create_artifacts() {
    let tempdir = tempfile::tempdir().unwrap();
    let platform = Arc::new(MockPlatform::default());
    let mut pipeline = make_test_pipeline_with_triggers(
        tempdir.path(),
        TriggerPolicy {
            snapshot: false,
            ..TriggerPolicy::ALL_ENABLED
        },
    );
    Arc::get_mut(&mut pipeline).unwrap().platform = platform.clone();
    let mut source = TestEventSource::new(vec![MonitorEvent::Snapshot, exited(0, 25)]);

    let outcome = event_loop(
        &mut source,
        &pipeline,
        123,
        9999,
        "test_app",
        &noop_reply,
        None,
        None,
    );

    assert_eq!(outcome.exit_code(), 0);
    assert_eq!(platform.suspend_count(), 0);
    assert_eq!(platform.resume_count(), 0);
    assert_no_artifacts(tempdir.path());
}

#[test]
fn test_clean_exit_no_reports() {
    let tempdir = tempfile::tempdir().unwrap();
    let pipeline = make_test_pipeline(tempdir.path());

    let mut source = TestEventSource::new(vec![exited(0, 7)]);

    let outcome = event_loop(
        &mut source,
        &pipeline,
        0,
        9999,
        "test_app",
        &noop_reply,
        None,
        None,
    );
    assert_eq!(outcome.exit_code(), 0);

    let json_count = count_json_files(tempdir.path());
    assert_eq!(json_count, 0, "Clean exit should produce no reports");
}

#[test]
fn test_disabled_exit_failure_trigger_preserves_outcome_without_report() {
    let tempdir = tempfile::tempdir().unwrap();
    let pipeline = make_test_pipeline_with_triggers(
        tempdir.path(),
        TriggerPolicy {
            exit_failure: false,
            ..TriggerPolicy::ALL_ENABLED
        },
    );
    let mut source = TestEventSource::new(vec![exited(23, 7)]);

    let outcome = event_loop(
        &mut source,
        &pipeline,
        0,
        9999,
        "test_app",
        &noop_reply,
        None,
        None,
    );

    assert_eq!(outcome.exit_code(), EXIT_CHILD_FAILURE);
    assert_no_artifacts(tempdir.path());
}

#[test]
fn test_disabled_signal_failure_trigger_preserves_outcome_without_report() {
    let tempdir = tempfile::tempdir().unwrap();
    let pipeline = make_test_pipeline_with_triggers(
        tempdir.path(),
        TriggerPolicy {
            signal_failure: false,
            probable_oom: false,
            ..TriggerPolicy::ALL_ENABLED
        },
    );
    let mut source = TestEventSource::new(vec![signaled(15, false, 7)]);

    let outcome = event_loop(
        &mut source,
        &pipeline,
        0,
        9999,
        "test_app",
        &noop_reply,
        None,
        None,
    );

    assert_eq!(outcome.exit_code(), 143);
    assert_no_artifacts(tempdir.path());
}

#[test]
fn test_sigkill_without_oom_detection_produces_signal_failure() {
    let tempdir = tempfile::tempdir().unwrap();
    let pipeline = make_test_pipeline_with_triggers(
        tempdir.path(),
        TriggerPolicy {
            probable_oom: false,
            ..TriggerPolicy::ALL_ENABLED
        },
    );

    let mut source = TestEventSource::new(vec![signaled(9, false, 123)]);

    let outcome = event_loop(
        &mut source,
        &pipeline,
        0,
        9999,
        "test_app",
        &noop_reply,
        None,
        None,
    );
    assert_eq!(outcome.exit_code(), 137, "128 + 9 = 137");
    let report = read_only_report(tempdir.path());
    assert_eq!(report["header"]["type"], "signal_failure");
    assert_eq!(report["termination"]["kind"], "signaled");
    assert_eq!(report["termination"]["signal"], 9);
    assert_eq!(report["termination"]["core_dumped"], false);
    assert_eq!(report["termination"]["runtime_ms"], 123);
}

#[test]
fn test_sigkill_produces_oom_report_when_enabled() {
    let tempdir = tempfile::tempdir().unwrap();
    let pipeline = make_test_pipeline_with_triggers(
        tempdir.path(),
        TriggerPolicy {
            // OOM classification is independent of the generic signal trigger.
            signal_failure: false,
            ..TriggerPolicy::ALL_ENABLED
        },
    );

    let mut source = TestEventSource::new(vec![signaled(9, false, 321)]);

    let outcome = event_loop(
        &mut source,
        &pipeline,
        0,
        9999,
        "test_app",
        &noop_reply,
        None,
        None,
    );
    assert_eq!(outcome.exit_code(), 137, "Exit code still 128 + 9");
    assert_eq!(
        count_json_files(tempdir.path()),
        1,
        "OOM detection on: SIGKILL should produce one report"
    );

    // Verify the report is tagged as OOM with sigkill trigger.
    let json_path = std::fs::read_dir(tempdir.path())
        .unwrap()
        .filter_map(Result::ok)
        .find(|e| e.path().extension().is_some_and(|x| x == "json"))
        .expect("OOM report file");
    let bytes = std::fs::read(json_path.path()).unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["header"]["type"], "oom");
    assert_eq!(v["header"]["trigger"], "sigkill");
    assert_eq!(v["termination"]["signal"], 9);
    assert_eq!(v["termination"]["runtime_ms"], 321);
}

#[test]
fn test_non_sigkill_signal_produces_signal_failure_report() {
    let tempdir = tempfile::tempdir().unwrap();
    let pipeline = make_test_pipeline(tempdir.path());

    // SIGTERM (15) — not a SIGKILL, so OOM dispatch should not fire even with
    // oom_detection enabled.
    let mut source = TestEventSource::new(vec![signaled(15, false, 456)]);

    let outcome = event_loop(
        &mut source,
        &pipeline,
        0,
        9999,
        "test_app",
        &noop_reply,
        None,
        None,
    );
    assert_eq!(outcome.exit_code(), 143, "128 + 15 = 143");
    let report = read_only_report(tempdir.path());
    assert_eq!(report["header"]["type"], "signal_failure");
    assert_eq!(report["termination"]["signal"], 15);
    assert_eq!(report["termination"]["runtime_ms"], 456);
}

#[test]
fn test_nonzero_exit_produces_exit_failure_and_preserves_code() {
    let tempdir = tempfile::tempdir().unwrap();
    let pipeline = make_test_pipeline(tempdir.path());
    let mut source = TestEventSource::new(vec![exited(23, 789)]);

    let outcome = event_loop(
        &mut source,
        &pipeline,
        0,
        9999,
        "test_app",
        &noop_reply,
        None,
        None,
    );

    assert_eq!(outcome.exit_code(), EXIT_CHILD_FAILURE);
    let report = read_only_report(tempdir.path());
    assert_eq!(report["header"]["type"], "exit_failure");
    assert_eq!(report["termination"]["kind"], "exited");
    assert_eq!(report["termination"]["exit_code"], 23);
    assert_eq!(report["termination"]["runtime_ms"], 789);
}

#[test]
fn test_child_termination_finalization_runs_off_event_loop_thread() {
    let tempdir = tempfile::tempdir().unwrap();
    let (thread_tx, thread_rx) = mpsc::sync_channel(1);
    let pipeline = Arc::new(Pipeline {
        enabled: true,
        triggers: TriggerPolicy::ALL_ENABLED,
        filters: vec![],
        collectors: vec![],
        pre_processors: vec![],
        post_processors: vec![Box::new(ThreadRecordingPostProcessor { thread_tx })],
        notifiers: vec![],
        shm: None,
        platform: Arc::new(MockPlatform::default()),
        output_dir: Some(tempdir.path().to_path_buf()),
    });
    let event_loop_thread = std::thread::current().id();
    let mut source = TestEventSource::new(vec![exited(7, 12)]);

    let outcome = event_loop(
        &mut source,
        &pipeline,
        0,
        9999,
        "test_app",
        &noop_reply,
        None,
        None,
    );

    assert_eq!(outcome.exit_code(), EXIT_CHILD_FAILURE);
    let finalizer_thread = thread_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("termination finalizer thread id");
    assert_ne!(finalizer_thread, event_loop_thread);
}

#[test]
fn test_termination_report_never_touches_dead_task_port() {
    let tempdir = tempfile::tempdir().unwrap();
    let platform = Arc::new(MockPlatform::default());
    let pipeline = Arc::new(Pipeline {
        enabled: true,
        triggers: TriggerPolicy::ALL_ENABLED,
        filters: vec![],
        collectors: vec![],
        pre_processors: vec![],
        post_processors: vec![],
        notifiers: vec![],
        shm: None,
        platform: platform.clone(),
        output_dir: Some(tempdir.path().to_path_buf()),
    });
    let mut source = TestEventSource::new(vec![exited(42, 5)]);

    let outcome = event_loop(
        &mut source,
        &pipeline,
        999,
        9999,
        "test_app",
        &noop_reply,
        None,
        None,
    );

    assert_eq!(outcome.exit_code(), EXIT_CHILD_FAILURE);
    assert_eq!(platform.suspend_count(), 0);
    assert_eq!(platform.resume_count(), 0);
    assert_eq!(count_json_files(tempdir.path()), 1);
}

#[test]
fn test_core_dump_flag_is_preserved() {
    let tempdir = tempfile::tempdir().unwrap();
    let pipeline = make_test_pipeline(tempdir.path());
    let mut source = TestEventSource::new(vec![signaled(6, true, 999)]);

    let outcome = event_loop(
        &mut source,
        &pipeline,
        0,
        9999,
        "test_app",
        &noop_reply,
        None,
        None,
    );

    assert_eq!(outcome.exit_code(), 134);
    let report = read_only_report(tempdir.path());
    assert_eq!(report["termination"]["signal"], 6);
    assert_eq!(report["termination"]["core_dumped"], true);
}

#[test]
fn test_child_exit_137_does_not_collide_with_sigkill_outcome() {
    let tempdir = tempfile::tempdir().unwrap();
    let pipeline = make_test_pipeline(tempdir.path());
    let mut source = TestEventSource::new(vec![exited(137, 1)]);

    let outcome = event_loop(
        &mut source,
        &pipeline,
        0,
        9999,
        "test_app",
        &noop_reply,
        None,
        None,
    );

    assert_eq!(outcome.exit_code(), EXIT_CHILD_FAILURE);
    assert_ne!(outcome.exit_code(), 128 + 9);
    assert_eq!(
        read_only_report(tempdir.path())["termination"]["exit_code"],
        137
    );
}

#[test]
fn test_monitor_failure_has_separate_exit_namespace() {
    let tempdir = tempfile::tempdir().unwrap();
    let pipeline = make_test_pipeline(tempdir.path());
    let mut source = TestEventSource::new(vec![MonitorEvent::MonitorFailure {
        message: "waitpid failed".to_string(),
    }]);

    let outcome = event_loop(
        &mut source,
        &pipeline,
        0,
        9999,
        "test_app",
        &noop_reply,
        None,
        None,
    );

    assert_eq!(outcome.exit_code(), EXIT_MONITOR_INTERNAL);
    assert_eq!(count_json_files(tempdir.path()), 0);
}

#[test]
fn test_multiple_snapshots_before_exit() {
    let tempdir = tempfile::tempdir().unwrap();
    let pipeline = make_test_pipeline(tempdir.path());

    let mut source = TestEventSource::new(vec![
        MonitorEvent::Snapshot,
        MonitorEvent::Snapshot,
        exited(0, 42),
    ]);

    let outcome = event_loop(
        &mut source,
        &pipeline,
        0,
        9999,
        "test_app",
        &noop_reply,
        None,
        None,
    );
    assert_eq!(outcome.exit_code(), 0);

    let json_count = count_json_files(tempdir.path());
    assert!(
        json_count >= 1,
        "Should produce at least 1 snapshot report, got {json_count}"
    );
}
