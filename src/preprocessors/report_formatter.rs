//! Converts raw collected data into report-ready formatted data.
//!
//! All conversion logic lives here:
//! - registers u64 → hex strings
//! - backtrace addresses → resolved frames (using images)
//! - stack bytes → base64
//! - VM regions → human-readable protection + labels
//! - heap stats → computed in-use bytes
//! - images → hex formatted

use crate::collectors::dylib::{self, RawImageData};
use crate::collectors::memory::RawHeapData;
use crate::collectors::thread::RawThreadData;
use crate::pipeline::report::{
    BacktraceFrame, HeapSummary, HeapZoneReport, LoadedImageReport, SessionReport,
    StackMemoryReport, ThreadReport, VmRegionReport,
};
use crate::pipeline::{CollectedData, Diagnostics, PluginStatus};
use crate::platform::VmRegionInfo;
use crate::utils::vm_tags;
use base64::Engine;
use std::collections::BTreeMap;
use std::sync::OnceLock;

// ═══════════════════════════════════════════════════
//  FormattedData — output of the formatter
// ═══════════════════════════════════════════════════

/// Fully formatted data ready for report serialization.
pub struct FormattedData {
    pub threads: Vec<ThreadReport>,
    pub loaded_images: Vec<LoadedImageReport>,
    pub memory_map: Vec<VmRegionReport>,
    pub heap_summary: Option<HeapSummary>,
    pub session: Option<SessionReport>,
    pub diagnostics_json: Option<serde_json::Value>,
    pub breadcrumbs: Vec<serde_json::Value>,
    pub crash_context: Option<serde_json::Value>,
    pub build: Option<serde_json::Value>,
    pub settings_snapshot: Option<serde_json::Value>,
    pub attachments: Vec<serde_json::Value>,
    pub environment: Option<serde_json::Value>,
    pub process_output: Option<crate::platform::ChildOutputSnapshot>,
}

// ═══════════════════════════════════════════════════
//  Main format function
// ═══════════════════════════════════════════════════

/// Convert raw collected data into formatted report-ready structures.
pub fn format(data: &CollectedData, diagnostics: &Diagnostics) -> FormattedData {
    let raw = &data.raw;

    let threads = format_threads(&raw.threads, &raw.images, &raw.symbols);
    let loaded_images = format_images(&raw.images);
    let memory_map = format_memory_map(&raw.memory_map, &raw.images);
    let heap_summary = format_heap_summary(&raw.heap);
    let session = data.session.clone();
    let diagnostics_json = build_diagnostics_json(diagnostics);

    let breadcrumbs = format_breadcrumbs(&raw.breadcrumbs);
    let (crash_context, build, settings_snapshot) =
        format_crash_context(raw.crash_context.as_ref(), raw.settings_snapshot.as_ref());
    let attachments = format_attachments(&raw.attachments);
    let environment = format_environment(raw.environment.as_ref());

    FormattedData {
        threads,
        loaded_images,
        memory_map,
        heap_summary,
        session,
        diagnostics_json,
        breadcrumbs,
        crash_context,
        build,
        settings_snapshot,
        attachments,
        environment,
        process_output: raw.process_output.clone(),
    }
}

// ═══════════════════════════════════════════════════
//  Thread formatting
// ═══════════════════════════════════════════════════

fn format_threads(
    threads: &[RawThreadData],
    images: &[RawImageData],
    symbols: &BTreeMap<u64, String>,
) -> Vec<ThreadReport> {
    threads
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let (registers, backtrace) = match &t.registers {
                Some(regs) => {
                    let formatted_regs: BTreeMap<String, String> = regs
                        .iter()
                        .map(|(k, v)| (k.clone(), format!("{v:#018x}")))
                        .collect();
                    let bt: Vec<BacktraceFrame> = t
                        .backtrace
                        .iter()
                        .map(|&addr| {
                            let (image, offset) = resolve_address(images, addr);
                            let symbol = symbols.get(&addr).cloned();
                            BacktraceFrame {
                                address: format!("{addr:#018x}"),
                                image,
                                offset: offset.map(|o| format!("{o:#x}")),
                                symbol,
                                file: None,
                                line: None,
                                column: None,
                            }
                        })
                        .collect();
                    (formatted_regs, bt)
                }
                None => (BTreeMap::new(), vec![]),
            };

            let stack_memory = t.stack_capture.as_ref().map(|cap| StackMemoryReport {
                sp: format!("{:#x}", cap.sp),
                #[allow(clippy::cast_possible_truncation)]
                size: cap.bytes.len() as u64,
                hex_dump: base64::engine::general_purpose::STANDARD.encode(&cap.bytes),
            });

            #[allow(clippy::cast_possible_truncation)] // thread count fits u32
            ThreadReport {
                index: i as u32,
                id: t.thread_port,
                name: t.name.clone(),
                crashed: t.crashed,
                registers,
                backtrace,
                stack_memory,
            }
        })
        .collect()
}

// ═══════════════════════════════════════════════════
//  Image formatting
// ═══════════════════════════════════════════════════

fn format_images(images: &[RawImageData]) -> Vec<LoadedImageReport> {
    images
        .iter()
        .map(|img| LoadedImageReport {
            path: img.path.clone(),
            base: format!("{:#x}", img.base_address),
            slide: img.slide.map(|s| format!("{s:#x}")),
        })
        .collect()
}

// ═══════════════════════════════════════════════════
//  Memory map formatting
// ═══════════════════════════════════════════════════

fn format_memory_map(regions: &[VmRegionInfo], images: &[RawImageData]) -> Vec<VmRegionReport> {
    regions
        .iter()
        .map(|r| {
            let info = region_label(r, images);
            VmRegionReport {
                address: format!("{:#x}", r.address),
                size: format!("{:#x}", r.size),
                prot: prot_string(r.protection).to_string(),
                info,
            }
        })
        .collect()
}

// ═══════════════════════════════════════════════════
//  Heap summary formatting
// ═══════════════════════════════════════════════════

fn format_heap_summary(heap: &RawHeapData) -> Option<HeapSummary> {
    if heap.malloc_zones.is_empty() && heap.vm_summary.is_none() {
        return None;
    }

    let zones: Vec<HeapZoneReport> = heap
        .malloc_zones
        .iter()
        .map(|z| HeapZoneReport {
            name: z.name.clone(),
            // resident_pages * page_size approximates in-use bytes;
            // total_size is virtual (includes free spans), so not suitable here.
            in_use_bytes: u64::from(z.resident_pages) * page_size(),
            in_use_count: u64::from(z.region_count),
        })
        .collect();

    Some(HeapSummary { zones })
}

// ═══════════════════════════════════════════════════
//  Diagnostics formatting
// ═══════════════════════════════════════════════════

pub(crate) fn build_diagnostics_json(diagnostics: &Diagnostics) -> Option<serde_json::Value> {
    if diagnostics.plugins.is_empty() {
        return None;
    }
    let plugins: serde_json::Map<String, serde_json::Value> = diagnostics
        .plugins
        .iter()
        .map(|d| {
            let status = match &d.status {
                PluginStatus::Ok => serde_json::json!({
                    "status": "ok",
                    "duration_ms": d.duration_ms,
                }),
                PluginStatus::Partial(reason) => serde_json::json!({
                    "status": "partial",
                    "reason": reason,
                    "duration_ms": d.duration_ms,
                }),
                PluginStatus::Rejected(reason) => serde_json::json!({
                    "status": "rejected",
                    "reason": reason,
                    "duration_ms": d.duration_ms,
                }),
                PluginStatus::Error(e) => serde_json::json!({
                    "status": "error",
                    "error": e,
                    "duration_ms": d.duration_ms,
                }),
                PluginStatus::Panic(payload) => serde_json::json!({
                    "status": "panic",
                    "payload": payload,
                    "duration_ms": d.duration_ms,
                }),
                PluginStatus::TimedOut => serde_json::json!({
                    "status": "timed_out",
                    "duration_ms": d.duration_ms,
                }),
                PluginStatus::Skipped(reason) => serde_json::json!({
                    "status": "skipped",
                    "reason": reason,
                }),
            };
            (d.name.clone(), status)
        })
        .collect();
    Some(serde_json::json!({
        "pipeline_duration_ms": diagnostics.pipeline_duration_ms(),
        "plugins": plugins,
    }))
}

// ═══════════════════════════════════════════════════
//  Helper functions
// ═══════════════════════════════════════════════════

/// Resolve a backtrace address to an image name + offset.
fn resolve_address(images: &[RawImageData], address: u64) -> (Option<String>, Option<u64>) {
    dylib::resolve_address(images, address)
}

/// Convert VM protection bits to a human-readable string like "r-x", "rw-".
fn prot_string(prot: i32) -> &'static str {
    match prot & 7 {
        1 => "r--",
        2 => "-w-",
        3 => "rw-",
        4 => "--x",
        5 => "r-x",
        6 => "-wx",
        7 => "rwx",
        _ => "---",
    }
}

/// Generate a human-readable label for a VM region, cross-referencing loaded images.
/// Images must be sorted by `base_address` (as returned by `enumerate_loaded_images`).
fn region_label(region: &VmRegionInfo, images: &[RawImageData]) -> String {
    // Binary search: find the last image whose base_address <= region.address
    let idx = images.partition_point(|img| img.base_address <= region.address);
    if idx > 0 {
        let img = &images[idx - 1];
        let offset = region.address - img.base_address;
        // Only label if the region is reasonably close to the image base (< 256MB)
        if offset < 0x1000_0000 {
            let name = img.path.rsplit('/').next().unwrap_or(&img.path);
            let segment = if region.protection & 4 != 0 {
                "__TEXT"
            } else if region.protection & 2 != 0 {
                "__DATA"
            } else {
                "__OTHER"
            };
            return format!("{segment} {name}");
        }
    }

    // Fall back to user_tag label
    let tag = vm_tags::user_tag_label(region.user_tag);
    if tag.is_empty() {
        String::new()
    } else {
        tag.to_string()
    }
}

// ═══════════════════════════════════════════════════
//  Breadcrumb formatting (Phase 4)
// ═══════════════════════════════════════════════════

/// Map category enum to string (matches `sut_crumb_category_t` order).
fn crumb_category_name(cat: u16) -> &'static str {
    match cat {
        0 => "TOOL",
        1 => "WORLD",
        2 => "UNDO",
        3 => "MESH",
        4 => "IO",
        5 => "RENDER",
        6 => "INPUT",
        7 => "MEMORY",
        8 => "THREAD",
        9 => "LIFECYCLE",
        10 => "USER",
        _ => "UNKNOWN",
    }
}

fn crumb_severity_name(sev: u16) -> &'static str {
    match sev {
        0 => "INFO",
        1 => "WARN",
        2 => "ERROR",
        _ => "UNKNOWN",
    }
}

/// Format breadcrumbs to JSON values (design doc L1111-1116).
fn format_breadcrumbs(crumbs: &[crate::shm::RawBreadcrumb]) -> Vec<serde_json::Value> {
    crumbs
        .iter()
        .map(|c| {
            serde_json::json!({
                "time_ns": c.timestamp_ns,
                "thread": c.thread_id,
                "cat": crumb_category_name(c.category),
                "sev": crumb_severity_name(c.severity),
                "file": c.file,
                "line": c.line,
                "msg": c.message,
            })
        })
        .collect()
}

// ═══════════════════════════════════════════════════
//  Crash context formatting (Phase 4)
// ═══════════════════════════════════════════════════

/// Format crash context, build info, and settings snapshot.
fn format_crash_context(
    ctx: Option<&crate::shm::RawCrashContext>,
    settings: Option<&crate::shm::RawSettingsSnapshot>,
) -> (
    Option<serde_json::Value>,
    Option<serde_json::Value>,
    Option<serde_json::Value>,
) {
    let Some(ctx) = ctx else {
        return (None, None, format_settings(settings));
    };

    // App/domain state is emitted as a generic annotation map (app-agnostic).
    let annotations: serde_json::Map<String, serde_json::Value> = ctx
        .annotations
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
        .collect();

    let crash_context = serde_json::json!({
        "annotations": annotations,
    });

    let build = serde_json::json!({
        "app_version": ctx.app_version,
        "build_number": ctx.build_number,
        "git_hash": ctx.git_hash,
        "git_dirty": ctx.git_dirty,
        "build_type": ctx.build_type,
        "build_preset": ctx.build_preset,
        "build_timestamp": ctx.build_timestamp,
        "compiler": ctx.compiler,
        "os": ctx.os_version,
    });

    (Some(crash_context), Some(build), format_settings(settings))
}

fn format_settings(
    settings: Option<&crate::shm::RawSettingsSnapshot>,
) -> Option<serde_json::Value> {
    let s = settings?;
    Some(serde_json::json!({
        "world_bounds": [
            s.world_bound_min[0], s.world_bound_min[1], s.world_bound_min[2],
            s.world_bound_max[0], s.world_bound_max[1], s.world_bound_max[2],
        ],
        "palette_count": s.palette_count,
        "history_max": s.history_max,
    }))
}

// ═══════════════════════════════════════════════════
//  Attachment formatting (Phase 4b)
// ═══════════════════════════════════════════════════

fn format_attachments(
    attachments: &[crate::collectors::attachment::RawCopiedAttachment],
) -> Vec<serde_json::Value> {
    attachments
        .iter()
        .map(|a| {
            let file = a
                .copied_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("unknown");
            serde_json::json!({
                "label": a.label,
                "file": file,
                "original_path": a.original_path,
                "size": a.size,
            })
        })
        .collect()
}

/// Query the system page size once (typically 16KB on ARM64 macOS).
fn page_size() -> u64 {
    static PAGE_SIZE: OnceLock<u64> = OnceLock::new();
    *PAGE_SIZE.get_or_init(|| {
        // SAFETY: sysconf is a standard POSIX function that returns page size.
        let size = unsafe { nix::libc::sysconf(nix::libc::_SC_PAGESIZE) };
        if size > 0 {
            #[allow(clippy::cast_sign_loss)] // checked > 0 above
            let page = size as u64;
            page
        } else {
            16384 // fallback for ARM64 macOS
        }
    })
}

// ═══════════════════════════════════════════════════
//  Environment formatting
// ═══════════════════════════════════════════════════

fn format_environment(
    env: Option<&crate::collectors::environment::RawEnvironment>,
) -> Option<serde_json::Value> {
    let env = env?;
    let vars: serde_json::Map<String, serde_json::Value> = env
        .env_vars
        .iter()
        .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
        .collect();
    Some(serde_json::json!({
        "os_version": env.os_version,
        "os_build": env.os_build,
        "arch": env.arch,
        "hostname": env.hostname,
        "env_vars": vars,
    }))
}

#[cfg(test)]
#[path = "../../tests/unit/preprocessors/report_formatter_tests.rs"]
mod tests;
