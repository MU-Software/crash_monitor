//! In-process integration tests for the extracted event loop.
//!
//! Uses `TestEventSource` to inject predetermined events, exercising the full
//! pipeline in-process (no external binary, no real Mach ports). This allows
//! `cargo llvm-cov` to instrument all code paths.

use std::collections::VecDeque;
use std::sync::Arc;

use crash_monitor::event_loop::{
    EXIT_CHILD_FAILURE, EXIT_DETECTED_CRASH, EXIT_MONITOR_INTERNAL, EventSource, MonitorEvent,
    MonitorOutcome, event_loop,
};
use crash_monitor::pipeline::{Pipeline, TerminationReason};
use crash_monitor::platform::mock::MockPlatform;

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

fn make_test_pipeline(tempdir: &std::path::Path) -> Pipeline {
    Pipeline {
        filters: vec![],
        collectors: vec![],
        pre_processors: vec![],
        post_processors: vec![],
        notifiers: vec![],
        shm: None,
        platform: Arc::new(MockPlatform::default()),
        output_dir: Some(tempdir.to_path_buf()),
    }
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

// ═══════════════════════════════════════════════════
//  Tests
// ═══════════════════════════════════════════════════

#[test]
fn test_crash_event_produces_report_and_exits() {
    let tempdir = tempfile::tempdir().unwrap();
    let pipeline = make_test_pipeline(tempdir.path());

    let mut source = TestEventSource::new(vec![
        MonitorEvent::Crash {
            exception_type: 1,
            code: 0xDEAD,
            subcode: 0xBEEF,
            thread_port: 42,
            reply_header: None,
        },
        exited(0, 10),
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
    let report_path = match &outcome {
        MonitorOutcome::DetectedCrash {
            termination,
            report_path: Some(path),
        } => {
            assert!(termination.is_none());
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
fn test_termination_report_never_touches_dead_task_port() {
    let tempdir = tempfile::tempdir().unwrap();
    let platform = Arc::new(MockPlatform::default());
    let pipeline = Pipeline {
        filters: vec![],
        collectors: vec![],
        pre_processors: vec![],
        post_processors: vec![],
        notifiers: vec![],
        shm: None,
        platform: platform.clone(),
        output_dir: Some(tempdir.path().to_path_buf()),
    };
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
