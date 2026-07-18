//! Process-level contract tests for the `symbolicate` subcommand.

use std::process::Command;

#[test]
fn symbolicate_cli_parses_arguments_and_reports_the_selected_dsym() {
    let dir = tempfile::tempdir().unwrap();
    let report = dir.path().join("report.json");
    std::fs::write(
        &report,
        serde_json::json!({
            "header": {
                "version": 1,
                "timestamp": "2026-04-05T12:00:00+09:00",
                "pid": 1234,
                "process": "selected_process",
                "collector": "crash_monitor",
                "type": "crash"
            },
            "threads": [],
            "loaded_images": []
        })
        .to_string(),
    )
    .unwrap();
    let selected_dsym = dir.path().join("Selected.dSYM");

    let output = Command::new(env!("CARGO_BIN_EXE_crash_monitor"))
        .arg("symbolicate")
        .arg(&report)
        .arg("--dsym")
        .arg(&selected_dsym)
        .output()
        .expect("run symbolicate CLI");

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("Selected.dSYM"), "{stderr}");
    assert!(stderr.contains("cannot read"), "{stderr}");
}

#[test]
fn symbolicate_help_describes_human_readable_default_output() {
    let output = Command::new(env!("CARGO_BIN_EXE_crash_monitor"))
        .args(["symbolicate", "--help"])
        .output()
        .expect("run symbolicate help");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("human-readable summary"), "{stdout}");
    assert!(stdout.contains("--output"), "{stdout}");
}
