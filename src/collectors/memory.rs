//! Collector: VM memory map + heap summary.
//!
//! Self-contained — absorbs `collect_memory_map` from `memory_map` and heap collection.
//! Heap summary uses out-of-process VM region analysis + `TASK_VM_INFO`.
//! `malloc_get_all_zones()` is in-process only — not usable from the monitor.

use crate::pipeline::{
    CollectedData, Collector, CollectorAccess, CrashEvent, Plugin, PluginContext, PluginExecution,
    Priority,
};
use crate::platform::{PlatformOps, TaskVmSummary, VmRegionEnumerationQuality, VmRegionInfo};
use crate::utils::vm_tags;
use mach2::port::mach_port_t;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

// ═══════════════════════════════════════════════════
//  Raw data types
// ═══════════════════════════════════════════════════

/// Aggregated heap data for the target process.
#[derive(Default, Serialize, Deserialize)]
pub struct RawHeapData {
    #[allow(dead_code)] // Phase 4+: will be included in report for phys_footprint etc.
    pub vm_summary: Option<TaskVmSummary>,
    pub malloc_zones: Vec<RawMallocZone>,
}

/// Per-zone (tag) statistics derived from VM regions.
#[derive(Serialize, Deserialize)]
pub struct RawMallocZone {
    pub name: String,
    pub region_count: u32,
    #[allow(dead_code)] // available for richer reporting (virtual size vs resident)
    pub total_size: u64,
    pub resident_pages: u32,
}

// ═══════════════════════════════════════════════════
//  Plugin + Collector implementation
// ═══════════════════════════════════════════════════

pub struct MemoryCollector {
    platform: Arc<dyn PlatformOps>,
}

impl MemoryCollector {
    pub fn new(platform: Arc<dyn PlatformOps>) -> Self {
        Self { platform }
    }
}

impl Plugin for MemoryCollector {
    fn name(&self) -> &'static str {
        "MemoryCollector"
    }
    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }
    fn priority(&self) -> Priority {
        Priority::High
    }
}

impl Collector for MemoryCollector {
    fn access(&self) -> CollectorAccess {
        CollectorAccess::IsolatedTask
    }

    fn collect(
        &self,
        _event: &CrashEvent,
        task: mach_port_t,
        data: &mut CollectedData,
        context: &PluginContext,
    ) -> Result<(), String> {
        context.checkpoint()?;
        let platform = self.platform.as_ref();
        let (memory_map, quality) = collect_memory_map(platform, task, context)?;
        data.raw.memory_map = memory_map;
        data.raw.heap = collect_heap_summary(platform, task, &data.raw.memory_map, context)?;
        context.checkpoint()?;
        if quality.is_complete() {
            Ok(())
        } else {
            Err(format!(
                "VM region enumeration returned partial data: {quality}"
            ))
        }
    }
}

// ═══════════════════════════════════════════════════
//  VM memory map collection (absorbed from memory_map.rs)
// ═══════════════════════════════════════════════════

/// Collect the full VM memory map of the target process.
fn collect_memory_map(
    platform: &dyn PlatformOps,
    task: mach_port_t,
    context: &PluginContext,
) -> Result<(Vec<VmRegionInfo>, VmRegionEnumerationQuality), String> {
    context.checkpoint()?;
    platform.enumerate_vm_regions(task, context)
}

// ═══════════════════════════════════════════════════
//  Heap summary
// ═══════════════════════════════════════════════════

/// Collect heap summary from task VM info and memory map regions.
fn collect_heap_summary(
    platform: &dyn PlatformOps,
    task: mach_port_t,
    regions: &[VmRegionInfo],
    context: &PluginContext,
) -> Result<RawHeapData, String> {
    context.checkpoint()?;
    let vm_summary = platform
        .get_task_vm_info(task)
        .map_err(|e| eprintln!("[monitor] get_task_vm_info failed: {e}"))
        .ok();

    let mut tag_groups: BTreeMap<u32, (u32, u64, u32)> = BTreeMap::new();
    for region in regions {
        context.checkpoint()?;
        if vm_tags::is_malloc_tag(region.user_tag) {
            let entry = tag_groups.entry(region.user_tag).or_insert((0, 0, 0));
            entry.0 += 1;
            entry.1 += region.size;
            entry.2 += region.pages_resident;
        }
    }

    let mut malloc_zones = Vec::with_capacity(tag_groups.len());
    for (tag, (count, size, resident)) in tag_groups {
        context.checkpoint()?;
        malloc_zones.push(RawMallocZone {
            name: vm_tags::user_tag_label(tag).to_string(),
            region_count: count,
            total_size: size,
            resident_pages: resident,
        });
    }

    Ok(RawHeapData {
        vm_summary,
        malloc_zones,
    })
}

#[cfg(test)]
#[path = "../../tests/unit/collectors/memory_tests.rs"]
mod tests;
