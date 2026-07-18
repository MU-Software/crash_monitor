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
    BacktraceFrame, BreadcrumbReport, BuildReport, CrashContextReport, EnvironmentReport,
    FormattedReportData, HeapSummary, HeapZoneReport, LoadedImageReport, LoadedImageSegmentReport,
    ReportValueSource, SettingsSnapshotReport, StackMemoryReport, TaskVmSummaryReport,
    ThreadReport, VmRegionReport,
};
use crate::pipeline::{CollectedData, Diagnostics, PluginStatus};
use crate::platform::VmRegionInfo;
use crate::utils::vm_tags;
use base64::Engine;
use std::collections::BTreeMap;
use std::sync::OnceLock;

// ═══════════════════════════════════════════════════
//  Main format function
// ═══════════════════════════════════════════════════

/// Convert raw collected data into formatted report-ready structures.
pub fn format(data: &CollectedData, diagnostics: &Diagnostics) -> FormattedReportData {
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

    FormattedReportData {
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
                truncated: cap.truncated,
            });

            #[allow(clippy::cast_possible_truncation)] // thread count fits u32
            ThreadReport {
                index: i as u32,
                id: t.thread_id,
                name: t.name.clone(),
                crashed: t.crashed,
                registers,
                backtrace,
                unwind_method: t.registers.as_ref().map(|_| "frame_pointer".to_string()),
                unwind_truncated: t.backtrace.len() >= 129,
                unwind_note: if t.registers.is_none() {
                    Some("register_state_unavailable".to_string())
                } else if t.backtrace.len() >= 129 {
                    Some("frame_pointer_depth_limit".to_string())
                } else {
                    Some("frame_pointer_only; compact_unwind_fallback_unavailable".to_string())
                },
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
            uuid: img.uuid.clone(),
            architecture: img.architecture.clone(),
            text_start: img.text_start.map(|value| format!("{value:#x}")),
            text_end: img.text_end.map(|value| format!("{value:#x}")),
            segments: img
                .segments
                .iter()
                .map(|segment| LoadedImageSegmentReport {
                    name: segment.name.clone(),
                    start: format!("{:#x}", segment.start),
                    end: format!("{:#x}", segment.end),
                })
                .collect(),
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
            resident_bytes_estimate: u64::from(z.resident_pages) * page_size(),
            region_count: u64::from(z.region_count),
            virtual_size_bytes: z.total_size,
        })
        .collect();

    let task_vm = heap.vm_summary.as_ref().map(|vm| TaskVmSummaryReport {
        virtual_size_bytes: vm.virtual_size,
        resident_size_bytes: vm.resident_size,
        physical_footprint_bytes: vm.phys_footprint,
        internal_bytes: vm.internal,
        compressed_bytes: vm.compressed,
    });

    Some(HeapSummary { task_vm, zones })
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
            let mut status = status;
            if let Some(object) = status.as_object_mut() {
                object.insert("started_offset_ms".into(), d.started_offset_ms.into());
                object.insert("finished_offset_ms".into(), d.finished_offset_ms.into());
                if let Some(report_id) = &d.report_id {
                    object.insert("report_id".into(), report_id.as_str().into());
                }
            }
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
    if let Some((img, segment)) = images.iter().find_map(|image| {
        image
            .segments
            .iter()
            .find(|segment| region.address >= segment.start && region.address < segment.end)
            .map(|segment| (image, segment))
    }) {
        let name = img.path.rsplit('/').next().unwrap_or(&img.path);
        return format!("{} {name}", segment.name);
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
        0 => "APPLICATION_0",
        1 => "APPLICATION_1",
        2 => "APPLICATION_2",
        3 => "APPLICATION_3",
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
fn format_breadcrumbs(crumbs: &[crate::shm::RawBreadcrumb]) -> Vec<BreadcrumbReport> {
    crumbs
        .iter()
        .map(|c| BreadcrumbReport {
            time_ns: c.timestamp_ns,
            thread: c.thread_id,
            cat: crumb_category_name(c.category).to_string(),
            sev: crumb_severity_name(c.severity).to_string(),
            file: c.file.clone(),
            line: u32::from(c.line),
            msg: c.message.clone(),
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
    Option<CrashContextReport>,
    Option<BuildReport>,
    Option<SettingsSnapshotReport>,
) {
    let Some(ctx) = ctx else {
        return (None, None, format_settings(settings));
    };

    // App/domain state is emitted as a generic annotation map (app-agnostic).
    let crash_context = CrashContextReport {
        source: ReportValueSource::ProducerSharedMemory,
        annotations: ctx
            .annotations
            .iter()
            .map(|(key, value)| (key.clone(), serde_json::Value::String(value.clone())))
            .collect(),
        session_id: (!ctx.session_id.is_empty()).then(|| ctx.session_id.clone()),
        session_start_ns: (ctx.session_start_ns != 0).then_some(ctx.session_start_ns),
        heartbeat_counter: ctx.heartbeat_counter,
    };

    let build = BuildReport {
        app_version: ctx.app_version.clone(),
        build_number: ctx.build_number,
        git_hash: ctx.git_hash.clone(),
        git_dirty: ctx.git_dirty,
        build_type: ctx.build_type.clone(),
        build_preset: ctx.build_preset.clone(),
        build_timestamp: ctx.build_timestamp.clone(),
        compiler: ctx.compiler.clone(),
        os: ctx.os_version.clone(),
    };

    (Some(crash_context), Some(build), format_settings(settings))
}

fn format_settings(
    settings: Option<&crate::shm::RawSettingsSnapshot>,
) -> Option<SettingsSnapshotReport> {
    let s = settings?;
    Some(SettingsSnapshotReport {
        source: ReportValueSource::ProducerSharedMemory,
        schema_version: s.schema_version,
        values: s.values.iter().cloned().collect(),
    })
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
) -> Option<EnvironmentReport> {
    let env = env?;
    Some(EnvironmentReport {
        kernel_release: env.kernel_release.clone(),
        kernel_version: env.kernel_version.clone(),
        arch: env.arch.clone(),
        hostname: env.hostname.clone(),
        variables_source: env.variables_source.to_string(),
        env_vars: env.env_vars.iter().cloned().collect(),
    })
}

#[cfg(test)]
#[path = "../../tests/unit/preprocessors/report_formatter_tests.rs"]
mod tests;
