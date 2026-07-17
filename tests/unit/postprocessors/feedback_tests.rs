use super::*;
use crate::pipeline::report::{self, CrashReport, ReportHeader};
use crate::pipeline::{CrashEvent, PluginContext, ReportResult, ReportType};
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

fn make_crash_event() -> CrashEvent {
    CrashEvent {
        report_type: ReportType::Crash,
        termination: None,
        exception_type: None,
        exception_code: None,
        exception_subcode: None,
        exception_codes: Vec::new(),
        crashed_thread: None,
        bail_on_suspend_failure: false,
        pid: 9999,
        process_name: "test_app".to_string(),
        hang_duration_ms: None,
    }
}

/// Write a minimal valid report JSON to a temp file and return its path.
fn write_test_report(dir: &std::path::Path) -> PathBuf {
    let report = CrashReport {
        header: ReportHeader {
            version: 1,
            timestamp: "2026-04-10T00:00:00".to_string(),
            pid: 9999,
            process: "test_app".to_string(),
            collector: "test".to_string(),
            report_type: ReportType::Crash,
            trigger: None,
            hang_duration_ms: None,
        },
        termination: None,
        build: None,
        exception: None,
        crash_context: None,
        threads: vec![],
        breadcrumbs: None,
        loaded_images: vec![],
        memory_map: vec![],
        heap_summary: None,
        session: None,
        settings_snapshot: None,
        fingerprint: None,
        environment: None,
        attachments: vec![],
        user_feedback: None,
        diagnostics: None,
    };

    let path = dir.join("test_report.json");
    let json = serde_json::to_string_pretty(&report).unwrap();
    fs::write(&path, json).unwrap();
    path
}

/// Create an executable shell script in `dir` with the given body.
fn make_mock_script(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    let mut f = fs::File::create(&path).unwrap();
    writeln!(f, "#!/bin/bash").unwrap();
    writeln!(f, "{body}").unwrap();
    drop(f);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[test]
fn test_not_available_when_missing() {
    let pp = FeedbackPostProcessor::new(PathBuf::from("/nonexistent/crash_dialog_macos"));
    assert!(!pp.is_available());
}

#[test]
fn test_plugin_dependency_metadata() {
    let pp = FeedbackPostProcessor::new(PathBuf::from("/nonexistent/crash_dialog_macos"));
    assert!(pp.hard_dependencies().is_empty());
    assert_eq!(pp.order_after(), &["PNGConverter"]);
}

#[test]
fn test_submit_patches_report() {
    let dir = tempfile::tempdir().unwrap();
    let script = make_mock_script(
        dir.path(),
        "mock_submit.sh",
        r#"echo -n "crashed during bevel operation""#,
    );
    let report_path = write_test_report(dir.path());

    let pp = FeedbackPostProcessor::new(script);
    let event = make_crash_event();
    let mut result = ReportResult {
        raw_path: None,
        json_path: Some(report_path.clone()),
        session: None,
    };

    let res = pp.process(&event, &mut result, &PluginContext::without_deadline());
    assert!(res.is_ok());

    // Verify the report was patched.
    let patched = report::load_report(&report_path).unwrap();
    let feedback = patched.user_feedback.unwrap();
    assert_eq!(feedback["comment"], "crashed during bevel operation");
}

#[test]
fn test_skip_does_not_modify_report() {
    let dir = tempfile::tempdir().unwrap();
    let script = make_mock_script(dir.path(), "mock_skip.sh", "exit 1");
    let report_path = write_test_report(dir.path());
    let original = fs::read_to_string(&report_path).unwrap();

    let pp = FeedbackPostProcessor::new(script);
    let event = make_crash_event();
    let mut result = ReportResult {
        raw_path: None,
        json_path: Some(report_path.clone()),
        session: None,
    };

    let res = pp.process(&event, &mut result, &PluginContext::without_deadline());
    assert!(res.is_ok());

    // Report should be unchanged.
    let after = fs::read_to_string(&report_path).unwrap();
    assert_eq!(original, after);
}

#[test]
fn test_dialog_crash_is_reported_without_modifying_report() {
    let dir = tempfile::tempdir().unwrap();
    let script = make_mock_script(dir.path(), "mock_crash.sh", "kill -SEGV $$");
    let report_path = write_test_report(dir.path());
    let original = fs::read_to_string(&report_path).unwrap();

    let pp = FeedbackPostProcessor::new(script);
    let event = make_crash_event();
    let mut result = ReportResult {
        raw_path: None,
        json_path: Some(report_path.clone()),
        session: None,
    };

    let res = pp.process(&event, &mut result, &PluginContext::without_deadline());
    assert!(
        matches!(res, Err(error) if error.contains("FeedbackDialog exited")),
        "a crashed dialog must be visible in plugin diagnostics"
    );

    // Report should be unchanged.
    let after = fs::read_to_string(&report_path).unwrap();
    assert_eq!(original, after);
}

#[test]
fn test_dialog_timeout_kills_child() {
    let dir = tempfile::tempdir().unwrap();
    // The mock hangs by spawning a background `sleep` *grandchild*, recording
    // its PID, then waiting. On timeout the post-processor must kill the whole
    // process group, so the grandchild dies too — killing only the direct
    // child (the shell) would orphan the `sleep`.
    let pidfile = dir.path().join("grandchild.pid");
    let body = format!("sleep 999 &\necho $! > {}\nwait", pidfile.display());
    let script = make_mock_script(dir.path(), "mock_hang.sh", &body);
    let report_path = write_test_report(dir.path());

    // 3s (not 1s): under heavy parallel load — e.g. the full instrumented
    // suite during `cargo llvm-cov` — the freshly spawned mock shell can be
    // starved for over a second before it records the grandchild PID. The
    // timeout must comfortably exceed that so the pidfile is written before
    // the group is killed, otherwise the precondition below races the kill.
    let pp = FeedbackPostProcessor::with_timeout(script, Duration::from_secs(3));
    let event = make_crash_event();
    let mut result = ReportResult {
        raw_path: None,
        json_path: Some(report_path.clone()),
        session: None,
    };

    let start = std::time::Instant::now();
    let context = PluginContext::without_deadline();
    let res = pp.process(&event, &mut result, &context);
    let elapsed = start.elapsed();

    assert!(
        matches!(res, Err(error) if error.contains("timed out")),
        "timeout must not be hidden as success"
    );
    assert!(
        context.is_timed_out(),
        "nested subprocess timeout must propagate to pipeline diagnostics"
    );
    // Should have timed out (3s + poll/kill margin), not slept the full 999s.
    assert!(elapsed.as_secs() < 10, "Took too long: {elapsed:?}");

    // The grandchild `sleep` must have been reaped along with the group.
    let gpid: i32 = fs::read_to_string(&pidfile)
        .expect("mock should have recorded grandchild pid")
        .trim()
        .parse()
        .expect("grandchild pid should parse");
    let pid = nix::unistd::Pid::from_raw(gpid);
    let mut gone = false;
    for _ in 0..40 {
        // Signal 0 (None) is an existence check; ESRCH means the pid is gone.
        if matches!(
            nix::sys::signal::kill(pid, None),
            Err(nix::errno::Errno::ESRCH)
        ) {
            gone = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        gone,
        "grandchild sleep (pid {gpid}) leaked after timeout kill"
    );
}

#[test]
fn test_no_report_path_skips_dialog() {
    let dir = tempfile::tempdir().unwrap();
    let script = make_mock_script(
        dir.path(),
        "mock_should_not_run.sh",
        "echo 'ERROR: should not have been called'; exit 99",
    );

    let pp = FeedbackPostProcessor::new(script);
    let event = make_crash_event();
    let mut result = ReportResult {
        raw_path: None,
        json_path: None,
        session: None,
    };

    let res = pp.process(&event, &mut result, &PluginContext::without_deadline());
    assert!(res.is_ok());
}
