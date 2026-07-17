//! In-process integration tests for the extracted event loop.
//!
//! Uses `TestEventSource` to inject predetermined events, exercising the full
//! pipeline in-process (no external binary, no real Mach ports). This allows
//! `cargo llvm-cov` to instrument all code paths.

use std::collections::VecDeque;
use std::sync::Arc;

use mbb_monitor::event_loop::{EventSource, MonitorEvent, event_loop};
use mbb_monitor::pipeline::Pipeline;
use mbb_monitor::platform::mock::MockPlatform;

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
        MonitorEvent::ChildExited { status: 0 },
    ]);

    let exit_code = event_loop(
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
    assert_eq!(exit_code, 1, "Crash should return exit code 1");

    assert!(
        count_json_files(tempdir.path()) >= 1,
        "Should produce a JSON report"
    );
}

#[test]
fn test_snapshot_event_continues() {
    let tempdir = tempfile::tempdir().unwrap();
    let pipeline = make_test_pipeline(tempdir.path());

    let mut source = TestEventSource::new(vec![
        MonitorEvent::Snapshot,
        MonitorEvent::ChildExited { status: 0 },
    ]);

    let exit_code = event_loop(
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
    assert_eq!(exit_code, 0, "Snapshot should not terminate");

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

    let mut source = TestEventSource::new(vec![MonitorEvent::ChildExited { status: 0 }]);

    let exit_code = event_loop(
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
    assert_eq!(exit_code, 0);

    let json_count = count_json_files(tempdir.path());
    assert_eq!(json_count, 0, "Clean exit should produce no reports");
}

#[test]
fn test_child_signaled_returns_128_plus_signal() {
    let tempdir = tempfile::tempdir().unwrap();
    let pipeline = make_test_pipeline(tempdir.path());

    let mut source = TestEventSource::new(vec![MonitorEvent::ChildSignaled { signal: 9 }]);

    let exit_code = event_loop(
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
    assert_eq!(exit_code, 137, "128 + 9 = 137");
    assert_eq!(
        count_json_files(tempdir.path()),
        0,
        "OOM detection off: SIGKILL should not produce a report"
    );
}

#[test]
fn test_sigkill_produces_oom_report_when_enabled() {
    let tempdir = tempfile::tempdir().unwrap();
    let pipeline = make_test_pipeline(tempdir.path());

    let mut source = TestEventSource::new(vec![MonitorEvent::ChildSignaled { signal: 9 }]);

    let exit_code = event_loop(
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
    assert_eq!(exit_code, 137, "Exit code still 128 + 9");
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
}

#[test]
fn test_non_sigkill_signal_does_not_produce_oom_report() {
    let tempdir = tempfile::tempdir().unwrap();
    let pipeline = make_test_pipeline(tempdir.path());

    // SIGTERM (15) — not a SIGKILL, so OOM dispatch should not fire even with
    // oom_detection enabled.
    let mut source = TestEventSource::new(vec![MonitorEvent::ChildSignaled { signal: 15 }]);

    let exit_code = event_loop(
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
    assert_eq!(exit_code, 143, "128 + 15 = 143");
    assert_eq!(
        count_json_files(tempdir.path()),
        0,
        "Non-SIGKILL signals must not produce OOM reports"
    );
}

#[test]
fn test_multiple_snapshots_before_exit() {
    let tempdir = tempfile::tempdir().unwrap();
    let pipeline = make_test_pipeline(tempdir.path());

    let mut source = TestEventSource::new(vec![
        MonitorEvent::Snapshot,
        MonitorEvent::Snapshot,
        MonitorEvent::ChildExited { status: 0 },
    ]);

    let exit_code = event_loop(
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
    assert_eq!(exit_code, 0);

    let json_count = count_json_files(tempdir.path());
    assert!(
        json_count >= 1,
        "Should produce at least 1 snapshot report, got {json_count}"
    );
}
