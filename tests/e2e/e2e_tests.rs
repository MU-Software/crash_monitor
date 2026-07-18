//! End-to-end tests: spawn `crash_monitor` with a `crash_app` child, verify report output.
//!
//! These tests require:
//! 1. `crash_monitor` binary built and signed (`make e2e-build`)
//! 2. `crash_app` test child built (`make e2e-child`)
//! 3. Debugger entitlement on `crash_monitor` (codesign)
//! 4. Debug build (`cargo build`) for `test_e2e_unsigned_binary_fails_fast`
//!
//! Lifecycle coverage includes fast clean and non-zero exits, an uncaught
//! SIGTERM, and an exec failure. The signed release monitor reserves exit 70
//! for its own failures and exit 80 for a child-reported failure; signal exits
//! preserve the conventional `128 + signal` status.
//!
//! Each test uses its own temporary directory via `CRASH_MONITOR_DATA_DIR` so that
//! tests can run in parallel without interfering with each other.

use crash_monitor::shm::{SHM_TOTAL_SIZE, SHM_VERSION, SharedMemory, ShmHeader};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::io::Read;
use std::mem::{offset_of, size_of};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};
use tempfile::TempDir;

const MONITOR_INTERNAL_FAILURE_EXIT_CODE: i32 = 70;
const CHILD_FAILURE_EXIT_CODE: i32 = 80;
const DETECTED_CRASH_EXIT_CODE: i32 = 81;
const SIGABRT_NUMBER: i32 = 6;
const SIGSEGV_NUMBER: i32 = 11;
const SIGTERM_NUMBER: i32 = 15;
const REPORT_MANIFEST_SCHEMA_VERSION: u32 = 1;
const REPORT_MANIFEST_FILE_NAME: &str = "manifest.json";
const MAX_REPORT_MANIFEST_BYTES: u64 = 1024 * 1024;

#[derive(Deserialize)]
struct ReportManifest {
    schema_version: u32,
    report_id: String,
    report_type: String,
    destination: ManifestDestination,
    artifacts: Vec<ManifestArtifact>,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ManifestDestination {
    OutputRoot,
    Sibling { directory: String },
}

#[derive(Deserialize)]
struct ManifestArtifact {
    path: String,
    kind: String,
    size: u64,
}

/// Locate the `crash_app` test child binary.
fn crash_app_path() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.join("tests/e2e/fixtures/crash_app")
}

fn unique_shm_test_id() -> u32 {
    static NEXT_ID: AtomicU32 = AtomicU32::new(1);
    0xE000_0000 | NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

fn copy_mapping(shm: &SharedMemory) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(SHM_TOTAL_SIZE);
    // SAFETY: `SharedMemory::create` maps exactly `SHM_TOTAL_SIZE` bytes. The
    // producer process is not running when this helper is called.
    unsafe {
        std::ptr::copy_nonoverlapping(shm.base_ptr(), bytes.as_mut_ptr(), SHM_TOTAL_SIZE);
        bytes.set_len(SHM_TOTAL_SIZE);
    }
    bytes
}

/// Locate the `crash_monitor` binary (release build).
fn monitor_path() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.join("target/release/crash_monitor")
}

/// Locate the `crash_dialog_mock` binary (release build).
fn mock_dialog_path() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.join("target/release/crash_dialog_mock")
}

/// Create a `Command` for the monitor with test-safe defaults.
///
/// Each test passes its own `data_dir` so reports are isolated.
/// Sets `CRASH_MONITOR_DIALOG_BIN` to the mock dialog binary so the pipeline
/// exercises the full feedback flow (spawn → read stdout → patch JSON)
/// without showing any interactive UI.
fn monitor_cmd(data_dir: &Path) -> Command {
    let mut cmd = Command::new(monitor_path());
    cmd.env("CRASH_MONITOR_DATA_DIR", data_dir);
    cmd.env("CRASH_MONITOR_DIALOG_BIN", mock_dialog_path());
    cmd
}

/// Get the sent crashes directory within a test's data dir.
/// After Phase 7-B, finished reports live here (`MoveToSent` relocates them).
fn archive_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("crashes/sent")
}

/// List canonical report artifacts from committed report directories.
///
/// A directory is visible to this test reader only when its identity matches a
/// valid manifest and the manifest describes the directory's exact regular-file
/// set. The returned path comes from the canonical `report` or `archive`
/// manifest entry rather than from a basename scan.
fn find_reports(dir: &Path, report_type: &str) -> Vec<PathBuf> {
    find_committed_reports(dir, Some(report_type))
}

fn find_committed_reports(dir: &Path, report_type: Option<&str>) -> Vec<PathBuf> {
    if !dir.exists() {
        return vec![];
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return vec![];
    };
    let mut reports: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| committed_report_artifact(&entry, report_type))
        .collect();
    reports.sort();
    reports
}

fn committed_report_artifact(
    entry: &std::fs::DirEntry,
    requested_type: Option<&str>,
) -> Option<PathBuf> {
    if !entry.file_type().ok()?.is_dir() {
        return None;
    }
    let report_id = entry.file_name().into_string().ok()?;
    if !is_report_id(&report_id) {
        return None;
    }

    let report_dir = entry.path();
    let manifest_path = report_dir.join(REPORT_MANIFEST_FILE_NAME);
    let metadata = std::fs::symlink_metadata(&manifest_path).ok()?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_REPORT_MANIFEST_BYTES {
        return None;
    }
    let manifest: ReportManifest =
        serde_json::from_slice(&std::fs::read(&manifest_path).ok()?).ok()?;
    if manifest.schema_version != REPORT_MANIFEST_SCHEMA_VERSION
        || manifest.report_id != report_id
        || !is_snake_case(&manifest.report_type)
        || requested_type.is_some_and(|kind| kind != manifest.report_type)
        || !manifest.destination.is_safe()
    {
        return None;
    }

    let mut registered = BTreeMap::new();
    let mut report_artifact = None;
    let mut archive_artifact = None;
    for artifact in &manifest.artifacts {
        if !is_safe_component(&artifact.path)
            || artifact.path == REPORT_MANIFEST_FILE_NAME
            || registered
                .insert(artifact.path.as_str(), artifact)
                .is_some()
        {
            return None;
        }
        match (artifact.kind.as_str(), artifact.path.as_str()) {
            ("report", "report.json") if report_artifact.is_none() => {
                report_artifact = Some(artifact);
            }
            ("archive", "report.zip") if archive_artifact.is_none() => {
                archive_artifact = Some(artifact);
            }
            ("report" | "archive", _) => return None,
            _ => {}
        }
    }

    let mut actual = BTreeMap::new();
    for artifact_entry in std::fs::read_dir(&report_dir).ok()? {
        let artifact_entry = artifact_entry.ok()?;
        let name = artifact_entry.file_name().into_string().ok()?;
        if name == REPORT_MANIFEST_FILE_NAME {
            continue;
        }
        if !artifact_entry.file_type().ok()?.is_file() {
            return None;
        }
        let size = artifact_entry.metadata().ok()?.len();
        if actual.insert(name, size).is_some() {
            return None;
        }
    }
    if registered.len() != actual.len()
        || registered
            .iter()
            .any(|(name, artifact)| actual.get(*name).is_none_or(|size| *size != artifact.size))
    {
        return None;
    }

    archive_artifact
        .or(report_artifact)
        .map(|artifact| report_dir.join(&artifact.path))
}

impl ManifestDestination {
    fn is_safe(&self) -> bool {
        match self {
            Self::OutputRoot => true,
            Self::Sibling { directory } => is_safe_component(directory),
        }
    }
}

fn is_report_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn is_snake_case(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('_')
        && !value.ends_with('_')
        && !value.contains("__")
        && value
            .bytes()
            .all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'_'))
}

fn is_safe_component(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('.')
        && matches!(
            Path::new(value).components().collect::<Vec<_>>().as_slice(),
            [std::path::Component::Normal(_)]
        )
}

/// List every finalized report, regardless of report type.
fn find_all_reports(dir: &Path) -> Vec<PathBuf> {
    find_committed_reports(dir, None)
}

/// List every file left anywhere in the crash artifact lifecycle tree.
fn find_all_crash_artifacts(data_dir: &Path) -> Vec<PathBuf> {
    let mut pending_dirs = vec![data_dir.join("crashes")];
    let mut artifacts = Vec::new();

    while let Some(dir) = pending_dirs.pop() {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending_dirs.push(path);
            } else {
                artifacts.push(path);
            }
        }
    }

    artifacts.sort();
    artifacts
}

fn e2e_required() -> bool {
    std::env::var("E2E_REQUIRED").as_deref() == Ok("1")
}

/// Check privileged prerequisites. Missing requirements are a hard failure for
/// the release gate and an explicit skip for opt-in local runs.
fn check_prerequisites() -> Result<(), String> {
    let monitor = monitor_path();
    let child = crash_app_path();
    if !monitor.exists() {
        return Err(format!(
            "crash_monitor not found at {}; run `make e2e-build`",
            monitor.display()
        ));
    }
    if !child.exists() {
        return Err(format!(
            "crash_app not found at {}; run `make e2e-child`",
            child.display()
        ));
    }
    let mock = mock_dialog_path();
    if !mock.exists() || mock.metadata().is_err() {
        return Err(format!(
            "mock dialog not found at {}; run `make e2e-build`",
            mock.display()
        ));
    }
    if mock
        .metadata()
        .map_or(true, |metadata| metadata.permissions().mode() & 0o111 == 0)
    {
        return Err(format!("mock dialog is not executable: {}", mock.display()));
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
                return Err(format!(
                    "crash_monitor lacks com.apple.security.cs.debugger; run `make e2e-build` with a valid SIGN_IDENTITY"
                ));
            }
        }
        Err(e) => {
            return Err(format!("codesign prerequisite check failed: {e}"));
        }
    }
    Ok(())
}

fn require_prerequisites() -> bool {
    match check_prerequisites() {
        Ok(()) => true,
        Err(reason) => skip_or_fail(&reason),
    }
}

fn require_file(path: &Path, preparation: &str) -> bool {
    if path.is_file() {
        true
    } else {
        skip_or_fail(&format!(
            "required fixture not found at {}; {preparation}",
            path.display()
        ))
    }
}

fn skip_or_fail(reason: &str) -> bool {
    if e2e_required() {
        panic!("required E2E prerequisite missing: {reason}")
    } else {
        eprintln!("SKIP: {reason}");
        false
    }
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
            let mut entry = archive
                .by_name("report.json")
                .expect("canonical report.json entry in ZIP");
            let mut content = String::new();
            entry.read_to_string(&mut content).expect("read ZIP entry");
            serde_json::from_str(&content).expect("parse JSON from ZIP")
        }
        _ => panic!("unexpected report extension: {}", path.display()),
    }
}

fn test_manifest(
    report_id: &str,
    report_type: &str,
    artifacts: &[serde_json::Value],
) -> serde_json::Value {
    serde_json::json!({
        "schema_version": REPORT_MANIFEST_SCHEMA_VERSION,
        "report_id": report_id,
        "report_type": report_type,
        "destination": { "kind": "sibling", "directory": "sent" },
        "artifacts": artifacts,
    })
}

fn write_test_manifest(report_dir: &Path, manifest: &serde_json::Value) {
    std::fs::create_dir_all(report_dir).expect("create report directory");
    std::fs::write(
        report_dir.join(REPORT_MANIFEST_FILE_NAME),
        serde_json::to_vec(manifest).expect("serialize test manifest"),
    )
    .expect("write test manifest");
}

#[test]
fn find_reports_uses_the_manifest_type_and_canonical_artifact() {
    let root = TempDir::new().expect("create temp dir");
    let crash_id = "11111111111111111111111111111111";
    let crash_dir = root.path().join(crash_id);
    std::fs::create_dir_all(&crash_dir).expect("create crash report directory");
    std::fs::write(crash_dir.join("report.json"), b"{}").expect("write JSON report");
    write_test_manifest(
        &crash_dir,
        &test_manifest(
            crash_id,
            "crash",
            &[serde_json::json!({
                "path": "report.json",
                "kind": "report",
                "size": 2,
            })],
        ),
    );

    let snapshot_id = "22222222222222222222222222222222";
    let snapshot_dir = root.path().join(snapshot_id);
    std::fs::create_dir_all(&snapshot_dir).expect("create snapshot report directory");
    std::fs::write(snapshot_dir.join("report.zip"), b"ZIP").expect("write ZIP report");
    write_test_manifest(
        &snapshot_dir,
        &test_manifest(
            snapshot_id,
            "snapshot",
            &[serde_json::json!({
                "path": "report.zip",
                "kind": "archive",
                "size": 3,
            })],
        ),
    );

    assert_eq!(
        find_reports(root.path(), "crash"),
        vec![crash_dir.join("report.json")]
    );
    assert_eq!(
        find_reports(root.path(), "snapshot"),
        vec![snapshot_dir.join("report.zip")]
    );
    assert_eq!(find_all_reports(root.path()).len(), 2);
}

#[test]
fn find_reports_ignores_hidden_incomplete_mismatched_and_extra_reports() {
    let root = TempDir::new().expect("create temp dir");

    let hidden_id = "33333333333333333333333333333333";
    let hidden_dir = root.path().join(format!(".report-{hidden_id}.pending"));
    std::fs::create_dir_all(&hidden_dir).expect("create hidden staging directory");
    std::fs::write(hidden_dir.join("report.json"), b"{}").expect("write hidden report");
    write_test_manifest(
        &hidden_dir,
        &test_manifest(
            hidden_id,
            "crash",
            &[serde_json::json!({
                "path": "report.json",
                "kind": "report",
                "size": 2,
            })],
        ),
    );

    let incomplete_dir = root.path().join("44444444444444444444444444444444");
    std::fs::create_dir_all(&incomplete_dir).expect("create incomplete report directory");
    std::fs::write(incomplete_dir.join("report.json"), b"{}").expect("write partial report");

    let mismatched_dir = root.path().join("55555555555555555555555555555555");
    std::fs::create_dir_all(&mismatched_dir).expect("create mismatched report directory");
    std::fs::write(mismatched_dir.join("report.json"), b"{}").expect("write report");
    write_test_manifest(
        &mismatched_dir,
        &test_manifest(
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "crash",
            &[serde_json::json!({
                "path": "report.json",
                "kind": "report",
                "size": 2,
            })],
        ),
    );

    let malformed_dir = root.path().join("66666666666666666666666666666666");
    std::fs::create_dir_all(&malformed_dir).expect("create malformed report directory");
    std::fs::write(malformed_dir.join(REPORT_MANIFEST_FILE_NAME), b"{")
        .expect("write malformed manifest");

    let extra_id = "77777777777777777777777777777777";
    let extra_dir = root.path().join(extra_id);
    std::fs::create_dir_all(&extra_dir).expect("create extra-artifact report directory");
    std::fs::write(extra_dir.join("report.json"), b"{}").expect("write report");
    std::fs::write(extra_dir.join("unregistered.bin"), b"extra")
        .expect("write unregistered artifact");
    write_test_manifest(
        &extra_dir,
        &test_manifest(
            extra_id,
            "crash",
            &[serde_json::json!({
                "path": "report.json",
                "kind": "report",
                "size": 2,
            })],
        ),
    );

    assert!(find_reports(root.path(), "crash").is_empty());
}

#[test]
fn find_reports_rejects_traversal_duplicates_and_non_regular_artifacts() {
    let root = TempDir::new().expect("create temp dir");

    let traversal_id = "88888888888888888888888888888888";
    let traversal_dir = root.path().join(traversal_id);
    write_test_manifest(
        &traversal_dir,
        &test_manifest(
            traversal_id,
            "crash",
            &[serde_json::json!({
                "path": "../report.json",
                "kind": "report",
                "size": 2,
            })],
        ),
    );

    let duplicate_id = "99999999999999999999999999999999";
    let duplicate_dir = root.path().join(duplicate_id);
    std::fs::create_dir_all(&duplicate_dir).expect("create duplicate report directory");
    std::fs::write(duplicate_dir.join("report.json"), b"{}").expect("write report");
    write_test_manifest(
        &duplicate_dir,
        &test_manifest(
            duplicate_id,
            "crash",
            &[
                serde_json::json!({
                    "path": "report.json",
                    "kind": "report",
                    "size": 2,
                }),
                serde_json::json!({
                    "path": "report.json",
                    "kind": "report",
                    "size": 2,
                }),
            ],
        ),
    );

    let non_regular_id = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    let non_regular_dir = root.path().join(non_regular_id);
    std::fs::create_dir_all(non_regular_dir.join("report.json"))
        .expect("create non-regular report artifact");
    write_test_manifest(
        &non_regular_dir,
        &test_manifest(
            non_regular_id,
            "crash",
            &[serde_json::json!({
                "path": "report.json",
                "kind": "report",
                "size": 0,
            })],
        ),
    );

    assert!(find_reports(root.path(), "crash").is_empty());
}

#[test]
#[ignore = "requires a signed monitor with debugger entitlement; run make e2e-required"]
fn test_e2e_crash_sigsegv() {
    if !require_prerequisites() {
        return;
    }
    let data_dir = TempDir::new().expect("create temp dir");
    let archive = archive_dir(data_dir.path());

    let output = monitor_cmd(data_dir.path())
        .arg("run")
        .arg(crash_app_path())
        .arg("sigsegv")
        .output()
        .expect("failed to run crash_monitor");

    assert_eq!(
        output.status.code(),
        Some(DETECTED_CRASH_EXIT_CODE),
        "detected Mach crash should use the crash namespace; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Find the crash report
    let reports = find_reports(&archive, "crash");
    assert!(
        !reports.is_empty(),
        "expected at least one crash report in {archive:?}",
    );

    // Verify JSON content (may be inside a ZIP archive)
    let json = read_report_json(&reports[0]);
    assert_eq!(json["header"]["type"], "crash");
    assert!(json["exception"].is_object(), "expected exception field");
    let raw_codes = json["exception"]["raw_codes"]
        .as_array()
        .expect("real Mach request must preserve its code array");
    assert!(
        !raw_codes.is_empty(),
        "Mach exception code array must not be empty"
    );
    assert!(raw_codes.iter().all(serde_json::Value::is_string));
    assert_eq!(json["termination"]["kind"], "signaled");
    assert_eq!(json["termination"]["signal"], SIGSEGV_NUMBER);
    assert!(json["termination"]["core_dumped"].is_boolean());
    assert!(json["termination"]["runtime_ms"].as_u64().is_some());
    let breadcrumbs = json["breadcrumbs"]
        .as_array()
        .expect("v4 C producer breadcrumb payload");
    assert!(
        breadcrumbs.iter().any(|breadcrumb| {
            breadcrumb["cat"] == "LIFECYCLE"
                && breadcrumb["sev"] == "INFO"
                && breadcrumb["file"] == "crash_app.c"
                && breadcrumb["msg"] == "scenario=sigsegv"
        }),
        "expected schema-v4 C producer breadcrumb, got {breadcrumbs:?}"
    );
    assert_eq!(
        json["crash_context"]["annotations"]["active_tool"], "e2e_producer",
        "schema-v4 C producer context must survive strict wire validation"
    );
}

#[test]
#[ignore = "requires a signed monitor with debugger entitlement; run make e2e-required"]
fn test_e2e_producer_rejects_schema_mismatch_without_writing() {
    let child = crash_app_path();
    if !require_file(&child, "run `make e2e-child`") {
        return;
    }

    let shm = SharedMemory::create(unique_shm_test_id()).expect("create test SHM");
    let legacy_version = SHM_VERSION
        .checked_sub(1)
        .expect("the current schema must have a predecessor");
    let version_offset = offset_of!(ShmHeader, version);
    // SAFETY: the child has not started, the header is in bounds and mmap is
    // sufficiently aligned. Copying bytes avoids materializing a wire struct.
    unsafe {
        std::ptr::copy_nonoverlapping(
            legacy_version.to_ne_bytes().as_ptr(),
            shm.base_ptr().add(version_offset),
            size_of::<u32>(),
        );
    }
    let before = copy_mapping(&shm);

    let status = Command::new(child)
        .arg("clean")
        .env("CRASH_MONITOR_SHM", shm.name())
        .status()
        .expect("run crash_app");
    assert!(
        status.success(),
        "clean fixture scenario must exit successfully"
    );

    assert_eq!(
        copy_mapping(&shm),
        before,
        "a producer must leave an unsupported SHM schema completely untouched"
    );
}

#[test]
#[ignore = "requires a signed monitor with debugger entitlement; run make e2e-required"]
fn test_e2e_crash_sigabrt() {
    if !require_prerequisites() {
        return;
    }
    let data_dir = TempDir::new().expect("create temp dir");
    let archive = archive_dir(data_dir.path());

    let output = monitor_cmd(data_dir.path())
        .arg("run")
        .arg(crash_app_path())
        .arg("sigabrt")
        .output()
        .expect("failed to run crash_monitor");

    assert_eq!(
        output.status.code(),
        Some(DETECTED_CRASH_EXIT_CODE),
        "detected Mach crash should use the crash namespace; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let reports = find_reports(&archive, "crash");
    assert!(!reports.is_empty(), "expected crash report");

    let json = read_report_json(&reports[0]);
    assert_eq!(json["header"]["type"], "crash");
    assert_eq!(json["termination"]["kind"], "signaled");
    assert_eq!(json["termination"]["signal"], SIGABRT_NUMBER);
    assert!(json["termination"]["core_dumped"].is_boolean());
    assert!(json["termination"]["runtime_ms"].as_u64().is_some());
}

#[test]
#[ignore = "requires a signed monitor with debugger entitlement; run make e2e-required"]
fn test_e2e_fast_clean_exit() {
    if !require_prerequisites() {
        return;
    }
    let data_dir = TempDir::new().expect("create temp dir");
    let archive = archive_dir(data_dir.path());

    let output = monitor_cmd(data_dir.path())
        .arg("run")
        .arg(crash_app_path())
        .arg("clean")
        .output()
        .expect("failed to run crash_monitor");

    // Monitor should exit 0 on clean exit
    assert!(
        output.status.success(),
        "monitor should exit 0 on clean child exit, got: {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    // No report of any type should exist in the finalized report directory.
    assert!(
        find_all_reports(&archive).is_empty(),
        "no reports expected on clean exit in {archive:?}"
    );
}

#[test]
#[ignore = "requires a signed monitor with debugger entitlement; run make e2e-required"]
fn test_e2e_nonzero_exit_reports_termination() {
    if !require_prerequisites() {
        return;
    }
    let data_dir = TempDir::new().expect("create temp dir");
    let archive = archive_dir(data_dir.path());

    let output = monitor_cmd(data_dir.path())
        .arg("run")
        .arg(crash_app_path())
        .arg("exit42")
        .output()
        .expect("failed to run crash_monitor");

    assert_eq!(
        output.status.code(),
        Some(CHILD_FAILURE_EXIT_CODE),
        "non-zero child exit should use the child-failure namespace; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let reports = find_reports(&archive, "exit_failure");
    assert_eq!(
        reports.len(),
        1,
        "expected exactly one exit-failure report in {archive:?}"
    );
    let json = read_report_json(&reports[0]);
    assert_eq!(json["header"]["type"], "exit_failure");
    assert_eq!(json["termination"]["kind"], "exited");
    assert_eq!(json["termination"]["exit_code"], 42);
    assert!(json["termination"]["runtime_ms"].as_u64().is_some());
}

#[test]
#[ignore = "requires a signed monitor with debugger entitlement; run make e2e-required"]
fn test_e2e_sigterm_preserves_signal_semantics() {
    if !require_prerequisites() {
        return;
    }
    let data_dir = TempDir::new().expect("create temp dir");
    let archive = archive_dir(data_dir.path());

    let output = monitor_cmd(data_dir.path())
        .arg("run")
        .arg(crash_app_path())
        .arg("sigterm")
        .output()
        .expect("failed to run crash_monitor");

    assert_eq!(
        output.status.code(),
        Some(128 + SIGTERM_NUMBER),
        "SIGTERM should retain 128 + signal semantics; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let reports = find_reports(&archive, "signal_failure");
    assert_eq!(
        reports.len(),
        1,
        "expected exactly one signal-failure report in {archive:?}"
    );
    let json = read_report_json(&reports[0]);
    assert_eq!(json["header"]["type"], "signal_failure");
    assert_eq!(json["termination"]["kind"], "signaled");
    assert_eq!(json["termination"]["signal"], SIGTERM_NUMBER);
    assert_eq!(json["termination"]["core_dumped"], false);
    assert!(json["termination"]["runtime_ms"].as_u64().is_some());
}

#[test]
#[ignore = "requires a signed monitor with debugger entitlement; run make e2e-required"]
fn test_e2e_nonexistent_executable_is_monitor_failure() {
    if !require_prerequisites() {
        return;
    }
    let data_dir = TempDir::new().expect("create temp dir");
    let archive = archive_dir(data_dir.path());
    let nonexistent = data_dir.path().join("executable-that-does-not-exist");
    assert!(!nonexistent.exists(), "test fixture path must not exist");

    let output = monitor_cmd(data_dir.path())
        .arg("run")
        .arg(&nonexistent)
        .output()
        .expect("failed to run crash_monitor");

    assert_eq!(
        output.status.code(),
        Some(MONITOR_INTERNAL_FAILURE_EXIT_CODE),
        "exec failure must be distinct from a fast clean exit; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        find_all_reports(&archive).is_empty(),
        "monitor-internal exec failure must not produce a child termination report"
    );
}

#[test]
#[ignore = "requires a signed monitor with debugger entitlement; run make e2e-required"]
fn test_e2e_uninstrumented_child_does_not_trigger_anr() {
    if !require_prerequisites() {
        return;
    }
    let data_dir = TempDir::new().expect("create temp dir");

    let output = monitor_cmd(data_dir.path())
        .env("CRASH_MONITOR_ANR_WARMUP_MS", "100")
        .env("CRASH_MONITOR_ANR_THRESHOLD_MS", "100")
        .env("CRASH_MONITOR_ANR_CHECK_INTERVAL_MS", "50")
        .arg("run")
        .arg(crash_app_path())
        .arg("uninstrumented")
        .output()
        .expect("failed to run crash_monitor");

    assert_eq!(
        output.status.code(),
        Some(0),
        "uninstrumented child should exit cleanly; stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let artifacts = find_all_crash_artifacts(data_dir.path());
    assert!(
        artifacts.is_empty(),
        "uninstrumented child must not leave ANR or termination artifacts: {artifacts:?}"
    );
}

#[test]
#[ignore = "requires a signed monitor with debugger entitlement; run make e2e-required"]
fn test_e2e_anr() {
    if !require_prerequisites() {
        return;
    }
    let data_dir = TempDir::new().expect("create temp dir");
    let archive = archive_dir(data_dir.path());

    // ANR: the child loops forever. Monitor should detect ANR after warmup + threshold,
    // generate a report, and the child keeps running. We kill the monitor after timeout.
    // Override ANR timings via env vars to keep the test fast.
    let mut child = monitor_cmd(data_dir.path())
        .env("CRASH_MONITOR_ANR_WARMUP_MS", "500")
        .env("CRASH_MONITOR_ANR_THRESHOLD_MS", "500")
        .env("CRASH_MONITOR_ANR_CHECK_INTERVAL_MS", "250")
        .arg("run")
        .arg(crash_app_path())
        .arg("anr")
        .spawn()
        .expect("failed to spawn crash_monitor");

    // Poll for the committed report instead of sleeping for a fixed interval:
    // slower CI hosts may cross a three-second boundary after ANR detection
    // but before the manifest-backed directory becomes visible.
    let deadline = Instant::now() + Duration::from_secs(10);
    let reports = loop {
        let reports = find_reports(&archive, "anr");
        if !reports.is_empty() {
            break reports;
        }
        if let Some(status) = child.try_wait().expect("poll crash_monitor") {
            panic!("crash_monitor exited before publishing an ANR report: {status}");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("timed out waiting for an ANR report in {archive:?}");
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    // Kill the monitor (which also kills the child)
    let _ = child.kill();
    let _ = child.wait();

    let json = read_report_json(&reports[0]);
    assert_eq!(json["header"]["type"], "anr");
}

/// The debug build binary lacks the debugger entitlement (only `make e2e-build`
/// applies it via codesign). Verify that the monitor detects this and exits
/// immediately with a clear error instead of hanging or producing a confusing
/// `task_for_pid` failure.
#[test]
#[ignore = "requires a signed monitor with debugger entitlement; run make e2e-required"]
fn test_e2e_unsigned_binary_fails_fast() {
    let child = crash_app_path();
    if !require_file(&child, "run `make e2e-child`") {
        return;
    }

    // Use the debug build which is ad-hoc signed but lacks the entitlement.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let debug_monitor = manifest.join("target/debug/crash_monitor");
    if !require_file(&debug_monitor, "run `cargo build --bin crash_monitor`") {
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
            let _ = skip_or_fail("debug crash_monitor unexpectedly has debugger entitlement");
            return;
        }
        _ => {}
    }

    let output = Command::new(&debug_monitor)
        .arg("run")
        .arg(&child)
        .arg("clean")
        .output()
        .expect("failed to run debug crash_monitor");

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
