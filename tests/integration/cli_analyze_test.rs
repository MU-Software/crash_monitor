//! Integration test: `analyze` subcommand on a sample report file.

use std::io::Write;
use std::process::Command;
use tempfile::NamedTempFile;

#[test]
fn test_analyze_valid_report() {
    let json = serde_json::json!({
        "header": {
            "version": 1,
            "timestamp": "2026-04-05T12:00:00.000000000+09:00",
            "pid": 1234,
            "process": "sample_app",
            "collector": "crash_monitor",
            "type": "crash"
        },
        "exception": {
            "type": "EXC_BAD_ACCESS",
            "code": "KERN_INVALID_ADDRESS",
            "subcode": "0x8",
            "signal": "SIGSEGV",
            "fault_address": "0x8"
        },
        "threads": [{
            "index": 0,
            "id": 100,
            "name": "main",
            "crashed": true,
            "registers": {},
            "backtrace": [
                {"address": "0x0000000100a3b1a8", "symbol": "render_frame"}
            ]
        }],
        "loaded_images": [],
        "memory_map": [],
        "attachments": []
    });

    let mut f = NamedTempFile::new().unwrap();
    f.write_all(json.to_string().as_bytes()).unwrap();
    f.flush().unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_crash_monitor"))
        .arg("analyze")
        .arg(f.path())
        .output()
        .expect("run analyze CLI");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Crash Report: SIGSEGV at 0x8"), "{stdout}");
    assert!(stdout.contains("sample_app"), "{stdout}");
    assert!(stdout.contains("render_frame"), "{stdout}");
}

#[test]
fn test_analyze_malformed_json() {
    let mut f = NamedTempFile::new().unwrap();
    f.write_all(b"{ not valid json }").unwrap();
    f.flush().unwrap();

    let exit_code = crash_monitor::cli::analyze::run(f.path().to_str().unwrap());
    assert_eq!(exit_code, 1);
}

#[test]
fn test_analyze_file_not_found() {
    let exit_code = crash_monitor::cli::analyze::run("/nonexistent/report.json");
    assert_eq!(exit_code, 1);
}
