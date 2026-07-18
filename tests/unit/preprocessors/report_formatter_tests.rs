use super::*;
use crate::collectors::attachment::RawCopiedAttachment;
use crate::collectors::dylib::RawImageData;
use crate::collectors::environment::RawEnvironment;
use crate::collectors::memory::{RawHeapData, RawMallocZone};
use crate::collectors::thread::{RawStackCapture, RawThreadData};
use crate::pipeline::{Diagnostics, PluginStatus};
use crate::platform::VmRegionInfo;
use crate::shm::{RawBreadcrumb, RawCrashContext, RawSettingsSnapshot};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

#[test]
fn test_prot_string() {
    assert_eq!(prot_string(0), "---");
    assert_eq!(prot_string(1), "r--");
    assert_eq!(prot_string(2), "-w-");
    assert_eq!(prot_string(3), "rw-");
    assert_eq!(prot_string(4), "--x");
    assert_eq!(prot_string(5), "r-x");
    assert_eq!(prot_string(6), "-wx");
    assert_eq!(prot_string(7), "rwx");
}

#[test]
fn test_crumb_category_name() {
    assert_eq!(crumb_category_name(0), "TOOL");
    assert_eq!(crumb_category_name(1), "WORLD");
    assert_eq!(crumb_category_name(2), "UNDO");
    assert_eq!(crumb_category_name(3), "MESH");
    assert_eq!(crumb_category_name(4), "IO");
    assert_eq!(crumb_category_name(5), "RENDER");
    assert_eq!(crumb_category_name(6), "INPUT");
    assert_eq!(crumb_category_name(7), "MEMORY");
    assert_eq!(crumb_category_name(8), "THREAD");
    assert_eq!(crumb_category_name(9), "LIFECYCLE");
    assert_eq!(crumb_category_name(10), "USER");
    assert_eq!(crumb_category_name(99), "UNKNOWN");
}

#[test]
fn test_crumb_severity_name() {
    assert_eq!(crumb_severity_name(0), "INFO");
    assert_eq!(crumb_severity_name(1), "WARN");
    assert_eq!(crumb_severity_name(2), "ERROR");
    assert_eq!(crumb_severity_name(3), "UNKNOWN");
}

#[test]
fn test_format_breadcrumbs_empty() {
    let result = format_breadcrumbs(&[]);
    assert!(result.is_empty());
}

#[test]
fn test_format_breadcrumbs_single() {
    let crumbs = vec![RawBreadcrumb {
        timestamp_ns: 123_456_789,
        thread_id: 42,
        category: 0,
        severity: 1,
        file: "test.c".to_string(),
        line: 100,
        message: "hello".to_string(),
    }];
    let result = format_breadcrumbs(&crumbs);
    assert_eq!(result.len(), 1);
    let v = &result[0];
    assert_eq!(v["time_ns"], 123_456_789_u64);
    assert_eq!(v["thread"], 42);
    assert_eq!(v["cat"], "TOOL");
    assert_eq!(v["sev"], "WARN");
    assert_eq!(v["file"], "test.c");
    assert_eq!(v["line"], 100);
    assert_eq!(v["msg"], "hello");
}

#[test]
fn test_format_attachments_with_original_path() {
    let attachments = vec![RawCopiedAttachment {
        label: "screenshot".to_string(),
        original_path: "/tmp/shot.png".to_string(),
        copied_path: PathBuf::from("/reports/123/shot.png"),
        size: 4096,
    }];
    let result = format_attachments(&attachments);
    assert_eq!(result.len(), 1);
    let v = &result[0];
    assert_eq!(v["original_path"], "/tmp/shot.png");
    assert_eq!(v["label"], "screenshot");
    assert_eq!(v["size"], 4096);
}

// ═══════════════════════════════════════════════════
//  Phase 9: raw → formatted conversion coverage
// ═══════════════════════════════════════════════════

fn image(path: &str, base: u64, slide: Option<u64>) -> RawImageData {
    RawImageData {
        path: path.to_string(),
        base_address: base,
        slide,
        uuid: None,
        architecture: Some("arm64".into()),
        text_start: Some(base),
        text_end: base.checked_add(0x1_0000),
    }
}

fn region(address: u64, protection: i32, user_tag: u32) -> VmRegionInfo {
    VmRegionInfo {
        address,
        size: 0x1000,
        protection,
        user_tag,
        share_mode: 0,
        pages_resident: 0,
        pages_swapped_out: 0,
    }
}

// ── format_images ──

#[test]
fn test_format_images() {
    let images = vec![
        image("/usr/lib/libSystem.dylib", 0x1000, Some(0x4000)),
        image("/bin/app", 0x8000, None),
    ];
    let out = format_images(&images);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].path, "/usr/lib/libSystem.dylib");
    assert_eq!(out[0].base, "0x1000");
    assert_eq!(out[0].slide.as_deref(), Some("0x4000"));
    assert_eq!(out[1].base, "0x8000");
    assert_eq!(out[1].slide, None);
}

// ── region_label branches ──

#[test]
fn test_region_label_text_segment() {
    let images = vec![image("/bin/app", 0x1_0000, None)];
    // protection r-x (bit 4 set) → __TEXT, near the image base.
    let label = region_label(&region(0x1_0000, 5, 0), &images);
    assert_eq!(label, "__TEXT app");
}

#[test]
fn test_region_label_data_segment() {
    let images = vec![image("/bin/app", 0x1_0000, None)];
    // protection rw- (bit 2 set, bit 4 clear) → __DATA.
    let label = region_label(&region(0x1_0000, 3, 0), &images);
    assert_eq!(label, "__DATA app");
}

#[test]
fn test_region_label_other_segment() {
    let images = vec![image("/bin/app", 0x1_0000, None)];
    // protection r-- (neither exec nor write) → __OTHER.
    let label = region_label(&region(0x1_0000, 1, 0), &images);
    assert_eq!(label, "__OTHER app");
}

#[test]
fn test_region_label_falls_back_to_user_tag_when_far_from_image() {
    let images = vec![image("/bin/app", 0x1000, None)];
    // Region is >256MB past the image base → no image label; user_tag=1 → "malloc".
    let label = region_label(&region(0x2000_0000, 3, 1), &images);
    assert_eq!(label, "malloc");
}

#[test]
fn test_region_label_empty_when_no_image_and_no_tag() {
    // No images and user_tag=0 → empty label.
    let label = region_label(&region(0x5000, 3, 0), &[]);
    assert_eq!(label, "");
}

#[test]
fn test_format_memory_map_combines_prot_and_label() {
    let images = vec![image("/bin/app", 0x1_0000, None)];
    let out = format_memory_map(&[region(0x1_0000, 5, 0)], &images);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].address, "0x10000");
    assert_eq!(out[0].prot, "r-x");
    assert_eq!(out[0].info, "__TEXT app");
}

// ── format_heap_summary ──

#[test]
fn test_format_heap_summary_empty_is_none() {
    let heap = RawHeapData {
        vm_summary: None,
        malloc_zones: vec![],
    };
    assert!(format_heap_summary(&heap).is_none());
}

#[test]
fn test_format_heap_summary_with_zones() {
    let heap = RawHeapData {
        vm_summary: Some(crate::platform::TaskVmSummary {
            virtual_size: 10,
            resident_size: 20,
            phys_footprint: 30,
            internal: 40,
            compressed: 50,
        }),
        malloc_zones: vec![RawMallocZone {
            name: "DefaultMallocZone".to_string(),
            region_count: 3,
            total_size: 1_000_000,
            resident_pages: 2,
        }],
    };
    let summary = format_heap_summary(&heap).expect("zones present → Some");
    assert_eq!(summary.zones.len(), 1);
    assert_eq!(summary.zones[0].name, "DefaultMallocZone");
    assert_eq!(summary.zones[0].region_count, 3);
    assert_eq!(summary.zones[0].virtual_size_bytes, 1_000_000);
    assert!(summary.zones[0].resident_bytes_estimate >= 2 * 4096);
    let vm = summary.task_vm.expect("task VM summary");
    assert_eq!(vm.physical_footprint_bytes, 30);
    assert_eq!(vm.internal_bytes, 40);
    assert_eq!(vm.compressed_bytes, 50);
}

// ── build_diagnostics_json ──

#[test]
fn test_build_diagnostics_json_empty_is_none() {
    let diag = Diagnostics::new();
    assert!(build_diagnostics_json(&diag).is_none());
}

#[test]
fn test_build_diagnostics_json_records_all_statuses() {
    let mut diag = Diagnostics::new();
    diag.record("OkPlugin", PluginStatus::Ok, Duration::from_millis(5));
    diag.record(
        "ErrPlugin",
        PluginStatus::Error("boom".into()),
        Duration::from_millis(2),
    );
    diag.record(
        "TimeoutPlugin",
        PluginStatus::TimedOut,
        Duration::from_millis(9),
    );
    diag.record_immediate("SkipPlugin", PluginStatus::Skipped("n/a".into()));

    let json = build_diagnostics_json(&diag).expect("non-empty → Some");
    let plugins = &json["plugins"];
    assert_eq!(plugins["OkPlugin"]["status"], "ok");
    assert_eq!(plugins["ErrPlugin"]["status"], "error");
    assert_eq!(plugins["ErrPlugin"]["error"], "boom");
    assert_eq!(plugins["TimeoutPlugin"]["status"], "timed_out");
    assert_eq!(plugins["TimeoutPlugin"]["duration_ms"], 9);
    assert_eq!(plugins["SkipPlugin"]["status"], "skipped");
    assert_eq!(plugins["SkipPlugin"]["reason"], "n/a");
}

// ── format_threads ──

#[test]
fn test_format_threads_with_registers_backtrace_and_stack() {
    let mut regs = BTreeMap::new();
    regs.insert("pc".to_string(), 0xDEAD_BEEF_u64);
    let threads = vec![RawThreadData {
        thread_port: 7,
        thread_id: 7_007,
        name: Some("main".to_string()),
        crashed: true,
        registers: Some(regs),
        backtrace: vec![0x1000],
        stack_capture: Some(RawStackCapture {
            sp: 0x0001_6d4f_e000,
            bytes: vec![1, 2, 3, 4],
            truncated: false,
        }),
    }];
    let mut symbols = BTreeMap::new();
    symbols.insert(0x1000_u64, "my_func".to_string());

    let out = format_threads(&threads, &[], &symbols);
    assert_eq!(out.len(), 1);
    let t = &out[0];
    assert_eq!(t.index, 0);
    assert_eq!(t.id, 7_007);
    assert!(t.crashed);
    assert_eq!(t.registers["pc"], "0x00000000deadbeef");
    assert_eq!(t.backtrace.len(), 1);
    assert_eq!(t.backtrace[0].address, "0x0000000000001000");
    assert_eq!(t.backtrace[0].symbol.as_deref(), Some("my_func"));
    let stack = t.stack_memory.as_ref().expect("stack captured");
    assert_eq!(stack.size, 4);
    assert!(!stack.hex_dump.is_empty());
}

#[test]
fn test_format_threads_without_registers_has_empty_backtrace() {
    let threads = vec![RawThreadData {
        thread_port: 1,
        thread_id: 1_001,
        name: None,
        crashed: false,
        registers: None,
        backtrace: vec![0x1000, 0x2000], // ignored when registers are absent
        stack_capture: None,
    }];
    let out = format_threads(&threads, &[], &BTreeMap::new());
    assert_eq!(out.len(), 1);
    assert!(out[0].registers.is_empty());
    assert!(out[0].backtrace.is_empty());
    assert!(out[0].stack_memory.is_none());
}

// ── format_settings ──

#[test]
fn test_format_settings_none() {
    assert!(format_settings(None).is_none());
}

#[test]
fn test_format_settings_some() {
    let s = RawSettingsSnapshot {
        world_bound_min: [-10, -20, -30],
        world_bound_max: [10, 20, 30],
        palette_count: 8,
        history_max: 64,
        extra: "opaque-settings-v1".to_string(),
    };
    let json = format_settings(Some(&s)).expect("Some");
    assert_eq!(json.palette_count, 8);
    assert_eq!(json.history_max, 64);
    assert_eq!(json.world_bounds[0], -10);
    assert_eq!(json.world_bounds[5], 30);
    assert_eq!(json.extra.as_deref(), Some("opaque-settings-v1"));
}

// ── format_environment ──

#[test]
fn test_format_environment_none() {
    assert!(format_environment(None).is_none());
}

#[test]
fn test_format_environment_some() {
    let env = RawEnvironment {
        kernel_release: "24.3.0".to_string(),
        kernel_version: "Darwin Kernel Version 24.3.0".to_string(),
        arch: "arm64".to_string(),
        hostname: "mac".to_string(),
        variables_source: "spawn_environment_snapshot",
        env_vars: vec![("LANG".to_string(), "en_US".to_string())],
    };
    let json = format_environment(Some(&env)).expect("Some");
    assert_eq!(json["kernel_release"], "24.3.0");
    assert_eq!(json["variables_source"], "spawn_environment_snapshot");
    assert_eq!(json["arch"], "arm64");
    assert_eq!(json["env_vars"]["LANG"], "en_US");
}

// ── format_crash_context ──

fn sample_crash_context() -> RawCrashContext {
    RawCrashContext {
        heartbeat_counter: 55,
        session_start_ns: 123,
        session_id: "sess-1".to_string(),
        annotations: vec![
            ("active_tool".to_string(), "bevel".to_string()),
            ("phase".to_string(), "bevel".to_string()),
        ],
        app_version: "1.2.3".to_string(),
        build_number: 100,
        git_hash: "abc123".to_string(),
        git_dirty: true,
        build_type: "debug".to_string(),
        build_preset: "default".to_string(),
        build_timestamp: "2026-01-01".to_string(),
        compiler: "clang".to_string(),
        os_version: "macOS 15".to_string(),
    }
}

#[test]
fn test_format_crash_context_none_passes_settings_through() {
    let settings = RawSettingsSnapshot {
        world_bound_min: [0, 0, 0],
        world_bound_max: [1, 1, 1],
        palette_count: 2,
        history_max: 4,
        extra: String::new(),
    };
    let (ctx, build, snap) = format_crash_context(None, Some(&settings));
    assert!(ctx.is_none());
    assert!(build.is_none());
    assert!(snap.is_some(), "settings should still be formatted");
}

#[test]
fn test_format_crash_context_full() {
    let ctx = sample_crash_context();
    let (context, build, _snap) = format_crash_context(Some(&ctx), None);

    let context = context.expect("ctx present → Some");
    // App/domain state is now a generic annotation map.
    assert_eq!(context.annotations["active_tool"], "bevel");
    assert_eq!(context.annotations["phase"], "bevel");
    assert_eq!(context.session_id.as_deref(), Some("sess-1"));
    assert_eq!(context.session_start_ns, Some(123));
    assert_eq!(context.heartbeat_counter, 55);

    let build = build.expect("ctx present → build Some");
    assert_eq!(build["app_version"], "1.2.3");
    assert_eq!(build["git_hash"], "abc123");
    assert_eq!(build["git_dirty"], true);
    assert_eq!(build["os"], "macOS 15");
}
