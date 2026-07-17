//! End-to-end tests: spawn `mbb_monitor` with a `crash_app` child, verify report output.
//!
//! These tests require:
//! 1. `mbb_monitor` binary built (`cargo build --release` or `make crash-monitor`)
//! 2. `crash_app` test child built (`make crash-monitor-e2e-child` or cc directly)
//! 3. Debugger entitlement on `mbb_monitor` (codesign)
//! 4. Debug build (`cargo build`) for `test_e2e_unsigned_binary_fails_fast`
//!
//! Each test uses its own temporary directory via `MBB_CRASH_DATA_DIR` so that
//! tests can run in parallel without interfering with each other.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use tempfile::TempDir;

/// Locate the `crash_app` test child binary.
fn crash_app_path() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.join("tests/e2e/fixtures/crash_app")
}

/// Locate the `mbb_monitor` binary (release build).
fn monitor_path() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.join("target/release/mbb_monitor")
}

/// Locate the `mbb_crash_dialog_mock` binary (release build).
fn mock_dialog_path() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.join("target/release/mbb_crash_dialog_mock")
}

/// Create a `Command` for the monitor with test-safe defaults.
///
/// Each test passes its own `data_dir` so reports are isolated.
/// Sets `MBB_CRASH_DIALOG_BIN` to the mock dialog binary so the pipeline
/// exercises the full feedback flow (spawn → read stdout → patch JSON)
/// without showing any interactive UI.
fn monitor_cmd(data_dir: &Path) -> Command {
    let mut cmd = Command::new(monitor_path());
    cmd.env("MBB_CRASH_DATA_DIR", data_dir);
    let mock = mock_dialog_path();
    if mock.exists() {
        cmd.env("MBB_CRASH_DIALOG_BIN", &mock);
    }
    cmd
}

/// Get the sent crashes directory within a test's data dir.
/// After Phase 7-B, finished reports live here (MoveToSent relocates them).
fn archive_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("crashes/sent")
}

/// List report files in the sent directory matching a prefix.
/// Accepts both `.json` (raw) and `.zip` (archived by ZIPArchiver).
fn find_reports(dir: &Path, prefix: &str) -> Vec<PathBuf> {
    if !dir.exists() {
        return vec![];
    }
    let mut reports: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .is_some_and(|ext| ext == "json" || ext == "zip")
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(prefix))
        })
        .collect();
    reports.sort();
    reports
}

/// Check prerequisites. Skip test if binaries don't exist or lack entitlements.
fn check_prerequisites() -> bool {
    let monitor = monitor_path();
    let child = crash_app_path();
    if !monitor.exists() {
        eprintln!("SKIP: mbb_monitor not found at {}", monitor.display());
        eprintln!("      Run: make crash-monitor");
        return false;
    }
    if !child.exists() {
        eprintln!("SKIP: crash_app not found at {}", child.display());
        eprintln!("      Run: make crash-monitor-e2e-child");
        return false;
    }
    // Verify the monitor binary has the debugger entitlement.
    // Without it, task_for_pid() will fail and the monitor will exit immediately.
    let codesign = std::process::Command::new("codesign")
        .args(["-d", "--entitlements", "-", "--xml"])
        .arg(&monitor)
        .output();
    match codesign {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if !stdout.contains("com.apple.security.cs.debugger") {
                eprintln!("SKIP: mbb_monitor lacks com.apple.security.cs.debugger entitlement");
                eprintln!("      Run: make crash-monitor");
                return false;
            }
        }
        Err(e) => {
            eprintln!("SKIP: codesign check failed: {e}");
            return false;
        }
    }
    true
}

/// Read report JSON from a `.json` file or extract it from a `.zip` archive.
fn read_report_json(path: &Path) -> serde_json::Value {
    match path.extension().and_then(|e| e.to_str()) {
        Some("json") => {
            let content = std::fs::read_to_string(path).expect("read report");
            serde_json::from_str(&content).expect("parse JSON")
        }
        Some("zip") => {
            let file = std::fs::File::open(path).expect("open ZIP");
            let mut archive = zip::ZipArchive::new(file).expect("parse ZIP");
            // Find the .json entry inside the ZIP
            for i in 0..archive.len() {
                let mut entry = archive.by_index(i).expect("ZIP entry");
                if entry.name().ends_with(".json") {
                    let mut content = String::new();
                    entry.read_to_string(&mut content).expect("read ZIP entry");
                    return serde_json::from_str(&content).expect("parse JSON from ZIP");
                }
            }
            panic!("no .json file found inside ZIP: {}", path.display());
        }
        _ => panic!("unexpected report extension: {}", path.display()),
    }
}

#[test]
fn test_e2e_crash_sigsegv() {
    if !check_prerequisites() {
        return;
    }
    let data_dir = TempDir::new().expect("create temp dir");
    let archive = archive_dir(data_dir.path());

    let output = monitor_cmd(data_dir.path())
        .arg("run")
        .arg(crash_app_path())
        .arg("sigsegv")
        .output()
        .expect("failed to run mbb_monitor");

    // Monitor should exit with non-zero (child crashed)
    assert!(
        !output.status.success(),
        "monitor should exit non-zero on crash"
    );

    // Find the crash report
    let reports = find_reports(&archive, "crash_");
    assert!(
        !reports.is_empty(),
        "expected at least one crash report in {archive:?}",
    );

    // Verify JSON content (may be inside a ZIP archive)
    let json = read_report_json(&reports[0]);
    assert_eq!(json["header"]["type"], "crash");
    assert!(json["exception"].is_object(), "expected exception field");
}

#[test]
fn test_e2e_crash_sigabrt() {
    if !check_prerequisites() {
        return;
    }
    let data_dir = TempDir::new().expect("create temp dir");
    let archive = archive_dir(data_dir.path());

    let output = monitor_cmd(data_dir.path())
        .arg("run")
        .arg(crash_app_path())
        .arg("sigabrt")
        .output()
        .expect("failed to run mbb_monitor");

    assert!(!output.status.success());

    let reports = find_reports(&archive, "crash_");
    assert!(!reports.is_empty(), "expected crash report");

    let json = read_report_json(&reports[0]);
    assert_eq!(json["header"]["type"], "crash");
}

#[test]
fn test_e2e_clean_exit() {
    if !check_prerequisites() {
        return;
    }
    let data_dir = TempDir::new().expect("create temp dir");
    let archive = archive_dir(data_dir.path());

    let output = monitor_cmd(data_dir.path())
        .arg("run")
        .arg(crash_app_path())
        .arg("clean")
        .output()
        .expect("failed to run mbb_monitor");

    // Monitor should exit 0 on clean exit
    assert!(
        output.status.success(),
        "monitor should exit 0 on clean child exit, got: {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    // No reports should exist in a fresh temp dir
    let reports = find_reports(&archive, "crash_").len()
        + find_reports(&archive, "snapshot_").len()
        + find_reports(&archive, "anr_").len();
    assert_eq!(reports, 0, "no reports on clean exit");
}

#[test]
fn test_e2e_anr() {
    if !check_prerequisites() {
        return;
    }
    let data_dir = TempDir::new().expect("create temp dir");
    let archive = archive_dir(data_dir.path());

    // ANR: the child loops forever. Monitor should detect ANR after warmup + threshold,
    // generate a report, and the child keeps running. We kill the monitor after timeout.
    // Override ANR timings via env vars to keep the test fast.
    let mut child = monitor_cmd(data_dir.path())
        .env("MBB_ANR_WARMUP_MS", "500")
        .env("MBB_ANR_THRESHOLD_MS", "500")
        .env("MBB_ANR_CHECK_INTERVAL_MS", "250")
        .arg("run")
        .arg(crash_app_path())
        .arg("anr")
        .spawn()
        .expect("failed to spawn mbb_monitor");

    // Wait for ANR detection (warmup=500ms + threshold=500ms + buffer)
    std::thread::sleep(Duration::from_secs(3));

    // Kill the monitor (which also kills the child)
    let _ = child.kill();
    let _ = child.wait();

    let reports = find_reports(&archive, "anr_");
    assert!(!reports.is_empty(), "expected ANR report in {archive:?}");

    let json = read_report_json(&reports[0]);
    assert_eq!(json["header"]["type"], "anr");
}

/// The debug build binary lacks the debugger entitlement (only `make crash-monitor`
/// applies it via codesign). Verify that the monitor detects this and exits
/// immediately with a clear error instead of hanging or producing a confusing
/// task_for_pid failure.
#[test]
fn test_e2e_unsigned_binary_fails_fast() {
    let child = crash_app_path();
    if !child.exists() {
        eprintln!("SKIP: crash_app not found");
        return;
    }

    // Use the debug build which is ad-hoc signed but lacks the entitlement.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let debug_monitor = manifest.join("target/debug/mbb_monitor");
    if !debug_monitor.exists() {
        eprintln!("SKIP: debug mbb_monitor not found (run `cargo build` first)");
        return;
    }

    // Confirm the debug binary actually lacks the entitlement (otherwise this
    // test is pointless — someone may have manually signed it).
    let ent_check = Command::new("codesign")
        .args(["-d", "--entitlements", "-", "--xml"])
        .arg(&debug_monitor)
        .output();
    match ent_check {
        Ok(out)
            if String::from_utf8_lossy(&out.stdout).contains("com.apple.security.cs.debugger") =>
        {
            eprintln!("SKIP: debug binary already has debugger entitlement");
            return;
        }
        _ => {}
    }

    let output = Command::new(&debug_monitor)
        .arg("run")
        .arg(&child)
        .arg("clean")
        .output()
        .expect("failed to run debug mbb_monitor");

    assert!(
        !output.status.success(),
        "unsigned monitor should exit non-zero"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Entitlement check failed"),
        "expected entitlement error message in stderr, got:\n{stderr}"
    );
}
