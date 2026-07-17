//! Integration test: `crash_dialog_macos` binary with `--mock-input` and `--dry-run`.
//!
//! These tests exercise the real binary end-to-end without showing any UI
//! (`AppKit` is never initialized). They verify argument parsing, exit codes,
//! and stdout output.

use std::io::Write;
use std::process::Command;

/// Path to the built binary (debug profile, same workspace target dir).
fn dialog_bin() -> std::path::PathBuf {
    let mut path = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    path.push("crash_dialog_macos");
    path
}

fn base_cmd() -> Command {
    let mut cmd = Command::new(dialog_bin());
    cmd.arg("--type")
        .arg("crash")
        .arg("--process")
        .arg("test_app")
        .arg("--timestamp")
        .arg("2026-04-10T00:00:00");
    cmd
}

#[test]
fn test_mock_input_submit() {
    let output = base_cmd()
        .arg("--mock-input")
        .arg("crashed during bevel")
        .output()
        .expect("failed to run dialog binary");

    assert!(output.status.success(), "expected exit 0");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout, "crashed during bevel");
}

#[test]
fn test_mock_input_empty_is_skip() {
    let output = base_cmd()
        .arg("--mock-input")
        .arg("")
        .output()
        .expect("failed to run dialog binary");

    assert_eq!(
        output.status.code(),
        Some(1),
        "empty input should exit 1 (skip)"
    );
    assert!(output.stdout.is_empty());
}

#[test]
fn test_mock_input_whitespace_only_is_skip() {
    let output = base_cmd()
        .arg("--mock-input")
        .arg("   ")
        .output()
        .expect("failed to run dialog binary");

    assert_eq!(
        output.status.code(),
        Some(1),
        "whitespace-only input should exit 1 (skip)"
    );
}

#[test]
fn test_dry_run_exits_zero() {
    let output = base_cmd()
        .arg("--dry-run")
        .output()
        .expect("failed to run dialog binary");

    assert!(output.status.success(), "dry-run should exit 0");
    assert!(output.stdout.is_empty(), "dry-run should produce no stdout");
}

#[test]
fn test_missing_required_args() {
    // --type is missing.
    let output = Command::new(dialog_bin())
        .arg("--process")
        .arg("test")
        .arg("--timestamp")
        .arg("now")
        .arg("--dry-run")
        .output()
        .expect("failed to run dialog binary");

    assert!(
        !output.status.success(),
        "missing --type should fail (clap error)"
    );
}

#[test]
fn test_mock_input_multiline() {
    let text = "line one\nline two\nline three";
    let output = base_cmd()
        .arg("--mock-input")
        .arg(text)
        .output()
        .expect("failed to run dialog binary");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout, text);
}

#[test]
fn test_feedback_post_processor_with_real_binary() {
    // End-to-end: FeedbackPostProcessor spawns the real dialog binary
    // with --mock-input, and patches a report JSON file.
    use crash_monitor::pipeline::report;
    use crash_monitor::pipeline::{CrashEvent, PostProcessor, ReportResult, ReportType};

    let dir = tempfile::tempdir().unwrap();

    // Write a minimal report JSON.
    let report_path = dir.path().join("report.json");
    let report = serde_json::json!({
        "header": {
            "version": 1,
            "timestamp": "2026-04-10T00:00:00",
            "pid": 9999,
            "process": "test_app",
            "collector": "test",
            "type": "crash"
        },
        "threads": [],
        "loaded_images": [],
        "memory_map": [],
        "attachments": []
    });
    let mut f = std::fs::File::create(&report_path).unwrap();
    f.write_all(serde_json::to_string_pretty(&report).unwrap().as_bytes())
        .unwrap();
    drop(f);

    // Create a wrapper script that calls the real binary with --mock-input.
    let wrapper_path = dir.path().join("mock_dialog.sh");
    let dialog = dialog_bin();
    let script = format!(
        "#!/bin/bash\n\
         exec \"{}\" --type crash --process test_app --timestamp now --mock-input \"e2e feedback test\"\n",
        dialog.display()
    );
    std::fs::write(&wrapper_path, script).unwrap();
    std::fs::set_permissions(
        &wrapper_path,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();

    let pp = crash_monitor::postprocessors::FeedbackPostProcessor::new(wrapper_path);
    let event = CrashEvent {
        report_type: ReportType::Crash,
        termination: None,
        exception_type: None,
        exception_code: None,
        exception_subcode: None,
        crashed_thread: None,
        bail_on_suspend_failure: false,
        pid: 9999,
        process_name: "test_app".to_string(),
        hang_duration_ms: None,
    };
    let mut result = ReportResult {
        raw_path: None,
        json_path: Some(report_path.clone()),
        session: None,
    };

    let res = pp.process(
        &event,
        &mut result,
        &crash_monitor::pipeline::PluginContext::without_deadline(),
    );
    assert!(res.is_ok());

    // Verify the report was patched with the feedback.
    let patched = report::load_report(&report_path).unwrap();
    let feedback = patched.user_feedback.expect("user_feedback should be set");
    assert_eq!(feedback["comment"], "e2e feedback test");
}
