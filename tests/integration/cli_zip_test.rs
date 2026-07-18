//! Integration test: `analyze` / `stack` subcommands reading a report that
//! has been bundled into a `.zip` archive by `ZIPArchiver` + `MoveToSent`.

use base64::Engine;
use std::io::Write;
use std::path::Path;
use std::process::Command;
use tempfile::tempdir;

/// A schema-valid crash report with stack memory (so `stack` has data to dump).
fn sample_report_json() -> String {
    let stack = vec![0x48u8, 0x65, 0x6c, 0x6c, 0x6f]; // "Hello"
    let encoded = base64::engine::general_purpose::STANDARD.encode(&stack);
    serde_json::json!({
        "header": {
            "version": 1,
            "timestamp": "2026-04-05T12:00:00.000000000+09:00",
            "pid": 1234,
            "process": "sample_app",
            "collector": "crash_monitor",
            "type": "crash"
        },
        "threads": [{
            "index": 0,
            "id": 100,
            "name": "main",
            "crashed": true,
            "registers": {},
            "backtrace": [
                {"address": "0x0000000100a3b1a8", "symbol": "vxc_face_drag_step"}
            ],
            "stack_memory": {
                "sp": "0x16d4fe000",
                "size": 5,
                "hex_dump": encoded
            }
        }],
        "loaded_images": [],
        "memory_map": [],
        "attachments": []
    })
    .to_string()
}

/// Build a ZIP mirroring `ZIPArchiver` output: entries stored by basename.
fn build_zip(path: &Path, entries: &[(&str, &[u8])]) {
    let file = std::fs::File::create(path).unwrap();
    let mut w = zip::ZipWriter::new(file);
    let opts = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for (name, bytes) in entries {
        w.start_file(*name, opts).unwrap();
        w.write_all(bytes).unwrap();
    }
    w.finish().unwrap();
}

#[test]
fn test_analyze_reads_report_from_zip() {
    let dir = tempdir().unwrap();
    let zip_path = dir.path().join("crash_20260405_120000_1234.zip");
    let json = sample_report_json();
    build_zip(
        &zip_path,
        &[
            ("crash_20260405_120000_1234.json", json.as_bytes()),
            (
                "crash_20260405_120000_1234_screenshot_000.rgba",
                b"\x00\x01\x02\x03",
            ),
        ],
    );

    let output = Command::new(env!("CARGO_BIN_EXE_crash_monitor"))
        .arg("analyze")
        .arg(&zip_path)
        .output()
        .expect("run analyze ZIP CLI");
    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("voxelcore_desktop"), "{stdout}");
    assert!(stdout.contains("vxc_face_drag_step"), "{stdout}");
}

#[test]
fn test_stack_reads_report_from_zip() {
    let dir = tempdir().unwrap();
    let zip_path = dir.path().join("crash_20260405_120000_1234.zip");
    let json = sample_report_json();
    build_zip(
        &zip_path,
        &[("crash_20260405_120000_1234.json", json.as_bytes())],
    );

    let output = Command::new(env!("CARGO_BIN_EXE_crash_monitor"))
        .arg("stack")
        .arg(&zip_path)
        .args(["--thread", "0"])
        .output()
        .expect("run stack ZIP CLI");
    assert!(output.status.success());
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("|Hello|")
    );
}

#[test]
fn test_analyze_zip_without_report_json_errors() {
    let dir = tempdir().unwrap();
    let zip_path = dir.path().join("crash_x.zip");
    build_zip(&zip_path, &[("crash_x_screenshot_000.rgba", b"raw pixels")]);

    let exit_code = crash_monitor::cli::analyze::run(zip_path.to_str().unwrap());
    assert_eq!(exit_code, 1);
}
