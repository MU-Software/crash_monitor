use super::*;
use crate::pipeline::report::CrashReport;
use crate::pipeline::{ReportType, TerminationReason};

fn minimal_report_json() -> String {
    serde_json::json!({
        "header": {
            "version": 1,
            "timestamp": "2026-04-05T12:00:00.000000000+09:00",
            "pid": 1234,
            "process": "voxelcore_desktop",
            "collector": "crash_monitor",
            "type": "crash"
        },
        "threads": [],
        "loaded_images": [],
        "memory_map": [],
        "attachments": []
    })
    .to_string()
}

fn full_crash_report_json() -> String {
    serde_json::json!({
        "header": {
            "version": 1,
            "timestamp": "2026-04-05T12:00:00.000000000+09:00",
            "pid": 1234,
            "process": "voxelcore_desktop",
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
        "crash_context": {
            "active_tool": "face_pull_drag",
            "frame_number": 2847,
            "region_count": 42
        },
        "threads": [{
            "index": 0,
            "id": 100,
            "name": "main",
            "crashed": true,
            "registers": {"x0": "0x0000000000000008"},
            "backtrace": [
                {"address": "0x0000000100a3b1a8", "image": "/path/to/voxelcore_desktop", "offset": "0x1a8", "symbol": "vxc_face_drag_step"},
                {"address": "0x0000000100a2f094", "image": "/path/to/voxelcore_desktop", "offset": "0x94", "symbol": "abi_dispatch_action"},
                {"address": "0x00000001a2b3c000", "image": "/usr/lib/system/libsystem_pthread.dylib", "offset": "0x20"}
            ]
        }],
        "session": {
            "id": "abc-123",
            "start": "2026-04-05T09:26:00+09:00",
            "duration_s": 9240
        },
        "fingerprint": "a1b2c3d4e5f67890",
        "breadcrumbs": [
            {"time_ns": 1000, "thread": 1, "cat": "crumb", "sev": "info", "file": "tool.c", "line": 42, "msg": "face_drag step dx=3"},
            {"time_ns": 2000, "thread": 1, "cat": "crumb", "sev": "info", "file": "tool.c", "line": 43, "msg": "set_voxel (1,2,3)=5"}
        ],
        "loaded_images": [],
        "memory_map": [],
        "attachments": [],
        "_diagnostics": {
            "pipeline_duration_ms": 105,
            "plugins": {
                "ThreadCollector": {"status": "ok", "duration_ms": 5},
                "MemoryCollector": {"status": "error", "duration_ms": 100, "error": "vm_read failed"},
                "SessionEnricher": {"status": "skipped", "reason": "no session.lock"}
            }
        }
    })
    .to_string()
}

#[test]
fn test_parse_minimal_report() {
    let json = minimal_report_json();
    let report: CrashReport = serde_json::from_str(&json).unwrap();
    assert_eq!(report.header.report_type, ReportType::Crash);
    assert_eq!(report.header.pid, 1234);
    assert!(report.threads.is_empty());
    assert!(report.exception.is_none());
    assert!(report.fingerprint.is_none());
}

#[test]
fn test_exit_failure_header_includes_termination_details() {
    let json = serde_json::json!({
        "header": {
            "version": 1,
            "timestamp": "2026-04-05T12:00:00+09:00",
            "pid": 1234,
            "process": "test_app",
            "collector": "crash_monitor",
            "type": "exit_failure"
        },
        "termination": {
            "kind": "exited",
            "exit_code": 17,
            "runtime_ms": 250
        }
    });
    let report: CrashReport = serde_json::from_value(json).unwrap();

    assert_eq!(
        header_summary(&report),
        "Exit Failure Report: exit code 17 after 250ms  (PID 1234, test_app)"
    );
    assert_eq!(
        report.termination,
        Some(TerminationReason::Exited {
            exit_code: 17,
            runtime_ms: 250,
        })
    );
}

#[test]
fn test_signal_failure_header_includes_core_dump_and_runtime() {
    let json = serde_json::json!({
        "header": {
            "version": 1,
            "timestamp": "2026-04-05T12:00:00+09:00",
            "pid": 5678,
            "process": "test_app",
            "collector": "crash_monitor",
            "type": "signal_failure"
        },
        "termination": {
            "kind": "signaled",
            "signal": 11,
            "core_dumped": true,
            "runtime_ms": 999
        }
    });
    let report: CrashReport = serde_json::from_value(json).unwrap();

    assert_eq!(
        header_summary(&report),
        "Signal Failure Report: signal 11, core dumped after 999ms  (PID 5678, test_app)"
    );
}

#[test]
fn test_parse_full_crash_report() {
    let json = full_crash_report_json();
    let report: CrashReport = serde_json::from_str(&json).unwrap();
    assert_eq!(report.header.report_type, ReportType::Crash);
    assert!(report.exception.is_some());
    assert_eq!(report.exception.as_ref().unwrap().signal, "SIGSEGV");
    assert_eq!(report.threads.len(), 1);
    assert!(report.threads[0].crashed);
    assert_eq!(report.threads[0].backtrace.len(), 3);
    assert_eq!(
        report.threads[0].backtrace[0].symbol.as_deref(),
        Some("vxc_face_drag_step")
    );
    assert_eq!(report.fingerprint.as_deref(), Some("a1b2c3d4e5f67890"));
    assert!(report.session.is_some());
    assert_eq!(report.session.as_ref().unwrap().duration_s, 9240);
}

#[test]
fn test_parse_snapshot_report() {
    let json = serde_json::json!({
        "header": {
            "version": 1,
            "timestamp": "2026-04-05T12:00:00+09:00",
            "pid": 5678,
            "process": "voxelcore_desktop",
            "collector": "crash_monitor",
            "type": "snapshot"
        },
        "threads": [{
            "index": 0,
            "id": 100,
            "name": "main",
            "crashed": false,
            "registers": {},
            "backtrace": []
        }],
        "loaded_images": [],
        "memory_map": [],
        "attachments": []
    })
    .to_string();

    let report: CrashReport = serde_json::from_str(&json).unwrap();
    assert_eq!(report.header.report_type, ReportType::Snapshot);
    assert!(report.exception.is_none());
    assert!(!report.threads[0].crashed);
}

#[test]
fn test_parse_anr_report() {
    let json = serde_json::json!({
        "header": {
            "version": 1,
            "timestamp": "2026-04-05T12:00:00+09:00",
            "pid": 9999,
            "process": "voxelcore_desktop",
            "collector": "crash_monitor",
            "type": "anr",
            "trigger": "watchdog_5s",
            "hang_duration_ms": 6200
        },
        "threads": [],
        "loaded_images": [],
        "memory_map": [],
        "attachments": []
    })
    .to_string();

    let report: CrashReport = serde_json::from_str(&json).unwrap();
    assert_eq!(report.header.report_type, ReportType::Anr);
    assert_eq!(report.header.trigger.as_deref(), Some("watchdog_5s"));
    assert_eq!(report.header.hang_duration_ms, Some(6200));
}

#[test]
fn test_report_roundtrip() {
    // Deserialize then re-serialize, then deserialize again — fields should match
    let json = full_crash_report_json();
    let report: CrashReport = serde_json::from_str(&json).unwrap();
    let reserialized = serde_json::to_string(&report).unwrap();
    let report2: CrashReport = serde_json::from_str(&reserialized).unwrap();
    assert_eq!(report.header.pid, report2.header.pid);
    assert_eq!(report.fingerprint, report2.fingerprint);
    assert_eq!(report.threads.len(), report2.threads.len());
}

#[test]
fn test_diagnostics_field_layout() {
    // Verify diagnostics has nested structure: { pipeline_duration_ms, plugins: { name: { status } } }
    // This format must match report_formatter::build_diagnostics_json
    let json = full_crash_report_json();
    let report: CrashReport = serde_json::from_str(&json).unwrap();
    let diag = report.diagnostics.as_ref().unwrap();

    // Top-level fields
    assert_eq!(
        diag.get("pipeline_duration_ms")
            .and_then(serde_json::Value::as_u64),
        Some(105)
    );

    // Plugins is a nested object
    let plugins = diag.get("plugins").and_then(|v| v.as_object()).unwrap();
    assert_eq!(plugins.len(), 3);

    // Status values are lowercase
    assert_eq!(
        plugins
            .get("ThreadCollector")
            .and_then(|p| p.get("status"))
            .and_then(|v| v.as_str()),
        Some("ok")
    );
    assert_eq!(
        plugins
            .get("MemoryCollector")
            .and_then(|p| p.get("status"))
            .and_then(|v| v.as_str()),
        Some("error")
    );
    assert_eq!(
        plugins
            .get("SessionEnricher")
            .and_then(|p| p.get("status"))
            .and_then(|v| v.as_str()),
        Some("skipped")
    );
}

#[test]
fn test_breadcrumb_field_layout() {
    // Verify breadcrumb format matches report_formatter::format_breadcrumbs
    let json = full_crash_report_json();
    let report: CrashReport = serde_json::from_str(&json).unwrap();
    let crumbs = report.breadcrumbs.as_ref().unwrap().as_array().unwrap();
    assert_eq!(crumbs.len(), 2);
    let first = &crumbs[0];
    // Field names: time_ns, cat, sev, file, line, msg
    assert!(first.get("time_ns").is_some());
    assert_eq!(first.get("cat").and_then(|v| v.as_str()), Some("crumb"));
    assert_eq!(
        first.get("msg").and_then(|v| v.as_str()),
        Some("face_drag step dx=3")
    );
}

#[test]
fn test_format_duration() {
    assert_eq!(format_duration(0), "0s");
    assert_eq!(format_duration(45), "45s");
    assert_eq!(format_duration(90), "1m 30s");
    assert_eq!(format_duration(3661), "1h 01m");
    assert_eq!(format_duration(9240), "2h 34m");
}
