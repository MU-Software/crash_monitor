//! In-process integration tests for the extracted event loop.
//!
//! Uses `TestEventSource` to inject predetermined events, exercising the full
//! pipeline in-process (no external binary, no real Mach ports). This allows
//! `cargo llvm-cov` to instrument all code paths.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use crash_monitor::event_loop::{
    EXIT_CHILD_FAILURE, EXIT_DETECTED_CRASH, EXIT_MONITOR_INTERNAL, EventSource, MonitorEvent,
    MonitorOutcome, event_loop,
};
use crash_monitor::pipeline::{
    CollectedData, Collector, CrashEvent, Notifier, Pipeline, Plugin, PluginContext,
    PluginExecution, PostProcessor, Priority, ReportResult, TerminationReason,
};
use crash_monitor::platform::mock::MockPlatform;
use crash_monitor::postprocessors::ZIPArchiver;

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

// ═══════════════════════════════════════════════════
//  Helpers
// ═══════════════════════════════════════════════════

fn make_test_pipeline(tempdir: &std::path::Path) -> Arc<Pipeline> {
    Arc::new(Pipeline {
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

fn count_json_files(dir: &std::path::Path) -> usize {
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "json"))
        .count()
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

fn noop_reply(_header: &mach2::message::mach_msg_header_t) {}

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
        thread_port: 42,
        reply_header: Some(mach2::message::mach_msg_header_t::default()),
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
        },
        None,
        None,
        false,
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
            thread_port: 42,
            reply_header: None,
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
        false,
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
        thread_port: 42,
        reply_header: Some(mach2::message::mach_msg_header_t::default()),
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
        },
        None,
        None,
        false,
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
        thread_port: 42,
        reply_header: None,
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
        false,
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
        false,
    );
    assert_eq!(outcome.exit_code(), 0, "Snapshot should not terminate");

    let json_count = count_json_files(tempdir.path());
    assert!(
        json_count >= 1,
        "Should produce a snapshot report, got {json_count}"
    );
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
        false,
    );
    assert_eq!(outcome.exit_code(), 0);

    let json_count = count_json_files(tempdir.path());
    assert_eq!(json_count, 0, "Clean exit should produce no reports");
}

#[test]
fn test_sigkill_without_oom_detection_produces_signal_failure() {
    let tempdir = tempfile::tempdir().unwrap();
    let pipeline = make_test_pipeline(tempdir.path());

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
        false,
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
    let pipeline = make_test_pipeline(tempdir.path());

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
        true, // oom_detection enabled
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
        true,
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
        false,
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
        false,
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
        false,
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
        false,
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
        false,
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
        false,
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
        false,
    );
    assert_eq!(outcome.exit_code(), 0);

    let json_count = count_json_files(tempdir.path());
    assert!(
        json_count >= 1,
        "Should produce at least 1 snapshot report, got {json_count}"
    );
}
