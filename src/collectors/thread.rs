//! Collector: all threads (registers + backtrace) with optional stack bytes.
//!
//! Self-contained — absorbs all logic from `thread_inspector` and `memory_reader::read_u64`.
//! Policy-authorized stack capture is included here because SP comes from
//! thread registers.

use crate::pipeline::{
    CollectedData, Collector, CollectorAccess, CrashEvent, Plugin, PluginContext, PluginExecution,
    Priority,
};
use crate::platform;
use crate::platform::PlatformOps;
use mach2::port::mach_port_t;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Bound per-event register/stack work and retained Mach send rights.
const MAX_CAPTURED_THREADS: usize = 512;
/// Maximum retained stack bytes across one event.
const MAX_TOTAL_STACK_BYTES: usize = 8 * 1024 * 1024;

// ═══════════════════════════════════════════════════
//  Raw data types
// ═══════════════════════════════════════════════════

/// Raw thread data collected during suspension.
#[derive(Default, Serialize, Deserialize)]
pub struct RawThreadData {
    pub thread_port: u32,
    /// System-wide `THREAD_IDENTIFIER_INFO.thread_id`; unlike `thread_port`,
    /// this remains meaningful outside the monitor task.
    pub thread_id: u64,
    pub name: Option<String>,
    pub crashed: bool,
    /// `None` if register inspection failed for this thread.
    pub registers: Option<BTreeMap<String, u64>>,
    pub backtrace: Vec<u64>,
    pub stack_capture: Option<RawStackCapture>,
}

/// Raw stack memory captured from SP upward.
#[derive(Serialize, Deserialize)]
pub struct RawStackCapture {
    pub sp: u64,
    pub bytes: Vec<u8>,
    pub truncated: bool,
}

// ═══════════════════════════════════════════════════
//  Plugin + Collector implementation
// ═══════════════════════════════════════════════════

pub struct ThreadCollector {
    platform: Arc<dyn PlatformOps>,
    capture_stack_memory: bool,
}

impl ThreadCollector {
    pub fn new(platform: Arc<dyn PlatformOps>, capture_stack_memory: bool) -> Self {
        Self {
            platform,
            capture_stack_memory,
        }
    }
}

impl Plugin for ThreadCollector {
    fn name(&self) -> &'static str {
        "ThreadCollector"
    }
    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }
    fn priority(&self) -> Priority {
        Priority::Critical
    }
}

impl Collector for ThreadCollector {
    fn access(&self) -> CollectorAccess {
        CollectorAccess::IsolatedTask
    }

    fn collect(
        &self,
        event: &CrashEvent,
        task: mach_port_t,
        data: &mut CollectedData,
        context: &PluginContext,
    ) -> Result<(), String> {
        context.checkpoint()?;
        let inspection = inspect_all_threads(
            self.platform.as_ref(),
            task,
            event.crashed_thread,
            self.capture_stack_memory,
            context,
        );
        data.raw.threads = inspection.threads;
        context.checkpoint()?;
        if inspection.budget_diagnostics.is_empty() {
            Ok(())
        } else {
            Err(inspection.budget_diagnostics.join("; "))
        }
    }
}

// ═══════════════════════════════════════════════════
//  Thread inspection (absorbed from thread_inspector.rs)
// ═══════════════════════════════════════════════════

/// Inspect all threads in a task. Each thread is inspected independently;
/// failure of one thread does not prevent collection of others.
struct ThreadInspection {
    threads: Vec<RawThreadData>,
    budget_diagnostics: Vec<String>,
}

#[allow(clippy::too_many_lines)] // one loop owns port retention, inspection, and shared budgets
fn inspect_all_threads(
    plat: &dyn PlatformOps,
    task: mach_port_t,
    crashed_thread: Option<mach_port_t>,
    capture_stack_memory: bool,
    context: &PluginContext,
) -> ThreadInspection {
    let mut budget_diagnostics = Vec::new();
    let mut threads = match plat.get_task_threads(task) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[monitor] Failed to enumerate threads: {e}");
            return ThreadInspection {
                threads: Vec::new(),
                budget_diagnostics,
            };
        }
    };

    if threads.len() > MAX_CAPTURED_THREADS {
        // Preserve the crashed thread even when it appears after the cap.
        if let Some(crashed_thread) = crashed_thread
            && let Some(index) = threads.iter().position(|thread| *thread == crashed_thread)
            && index >= MAX_CAPTURED_THREADS
        {
            threads.swap(MAX_CAPTURED_THREADS - 1, index);
        }
        for thread in threads.drain(MAX_CAPTURED_THREADS..) {
            plat.deallocate_thread_port(thread);
        }
        eprintln!(
            "[monitor] ThreadCollector: truncated thread list to {MAX_CAPTURED_THREADS} entries"
        );
        budget_diagnostics.push(format!(
            "thread budget exceeded; retained {MAX_CAPTURED_THREADS} threads"
        ));
    }

    // Inspect and spend stack budget on the crashed thread first, then retain
    // the task enumeration order. This makes truncation deterministic while
    // preserving the most relevant stack.
    if let Some(crashed_thread) = crashed_thread
        && let Some(index) = threads.iter().position(|thread| *thread == crashed_thread)
    {
        threads[..=index].rotate_right(1);
    }

    let mut inspected = Vec::with_capacity(threads.len());
    let mut remaining_stack_bytes = MAX_TOTAL_STACK_BYTES;
    let mut stack_budget_exhausted = false;
    for thread in threads {
        let crashed = crashed_thread == Some(thread);
        let thread_id = plat.get_thread_identifier(thread).unwrap_or(0);
        if context.is_timed_out() {
            // Preserve every acquired port in the payload so the pipeline's
            // PortGuard can still release it after cooperative cancellation.
            inspected.push(RawThreadData {
                thread_port: thread,
                thread_id,
                name: None,
                crashed,
                registers: None,
                backtrace: Vec::new(),
                stack_capture: None,
            });
            continue;
        }

        let name = plat.get_thread_name(thread).unwrap_or(None);
        let result = inspect_thread(plat, task, thread, context);
        if context.is_timed_out() {
            inspected.push(RawThreadData {
                thread_port: thread,
                thread_id,
                name,
                crashed,
                registers: None,
                backtrace: Vec::new(),
                stack_capture: None,
            });
            continue;
        }

        match result {
            Ok((registers, backtrace)) => {
                let stack_capture = if capture_stack_memory {
                    registers.get("sp").copied().and_then(|sp| {
                        if remaining_stack_bytes == 0 {
                            stack_budget_exhausted = true;
                            return None;
                        }
                        let outcome =
                            read_stack_memory(plat, task, sp, remaining_stack_bytes, context)
                                .ok()?;
                        remaining_stack_bytes =
                            remaining_stack_bytes.saturating_sub(outcome.capture.bytes.len());
                        stack_budget_exhausted |= outcome.budget_limited;
                        Some(outcome.capture)
                    })
                } else {
                    None
                };

                inspected.push(RawThreadData {
                    thread_port: thread,
                    thread_id,
                    name,
                    crashed,
                    registers: Some(registers),
                    backtrace,
                    stack_capture,
                });
            }
            Err(_) => inspected.push(RawThreadData {
                thread_port: thread,
                thread_id,
                name,
                crashed,
                registers: None,
                backtrace: Vec::new(),
                stack_capture: None,
            }),
        }
    }
    if stack_budget_exhausted {
        budget_diagnostics.push(format!(
            "stack budget exceeded; retained at most {MAX_TOTAL_STACK_BYTES} bytes"
        ));
    }
    ThreadInspection {
        threads: inspected,
        budget_diagnostics,
    }
}

/// Collect full thread state: registers + backtrace.
fn inspect_thread(
    plat: &dyn PlatformOps,
    task: mach_port_t,
    thread: mach_port_t,
    context: &PluginContext,
) -> Result<(BTreeMap<String, u64>, Vec<u64>), String> {
    context.checkpoint()?;
    let regs = get_registers(plat, thread, context)?;

    let fp = regs.get("fp").copied().unwrap_or(0);
    let pc = regs.get("pc").copied().unwrap_or(0);

    let mut backtrace = vec![pc];
    backtrace.extend(walk_backtrace(plat, task, fp, 128, context)?);

    Ok((regs, backtrace))
}

/// Collect ARM64 registers from a thread port.
fn get_registers(
    plat: &dyn PlatformOps,
    thread: mach_port_t,
    context: &PluginContext,
) -> Result<BTreeMap<String, u64>, String> {
    context.checkpoint()?;
    let state = plat.get_thread_state(thread)?;
    context.checkpoint()?;

    // `ArmThreadState64` construction has already verified that the kernel or
    // mock response covers the highest fixed access (CPSR at word 66).

    let mut regs = BTreeMap::new();

    // ARM64 state layout: 33 u64 values (x0-x28, fp, lr, sp, pc) stored as pairs of u32
    for (i, name) in platform::ARM64_GPR_NAMES.iter().enumerate() {
        context.checkpoint()?;
        let lo = u64::from(state[i * 2]);
        let hi = u64::from(state[i * 2 + 1]);
        regs.insert(name.to_string(), lo | (hi << 32));
    }

    // sp at index 31, pc at 32
    let sp = u64::from(state[31 * 2]) | (u64::from(state[31 * 2 + 1]) << 32);
    regs.insert("sp".to_string(), sp);

    let pc = u64::from(state[32 * 2]) | (u64::from(state[32 * 2 + 1]) << 32);
    regs.insert("pc".to_string(), pc);

    regs.insert("cpsr".to_string(), u64::from(state[33 * 2]));

    Ok(regs)
}

/// Walk the frame pointer chain to build a backtrace.
/// Each frame: [fp] = previous fp, [fp+8] = return address.
fn walk_backtrace(
    plat: &dyn PlatformOps,
    task: mach_port_t,
    fp: u64,
    max_depth: usize,
    context: &PluginContext,
) -> Result<Vec<u64>, String> {
    let mut frames = Vec::new();
    let mut current_fp = fp;

    for _ in 0..max_depth {
        context.checkpoint()?;
        if current_fp == 0 || !current_fp.is_multiple_of(8) {
            break;
        }

        let Some(lr_address) = current_fp.checked_add(8) else {
            break;
        };
        let Ok(lr) = read_u64(plat, task, lr_address) else {
            break;
        };

        if lr == 0 {
            break;
        }

        // arm64e return addresses can carry PAC bits. Keep the canonical
        // userspace address for lookup while the raw stack bytes remain intact.
        frames.push(lr & 0x0000_ffff_ffff_ffff);

        let Ok(prev_fp) = read_u64(plat, task, current_fp) else {
            break;
        };

        if prev_fp <= current_fp {
            break;
        }

        current_fp = prev_fp;
    }

    Ok(frames)
}

// ═══════════════════════════════════════════════════
//  Stack memory capture
// ═══════════════════════════════════════════════════

/// Maximum bytes to read per thread stack.
const MAX_STACK_BYTES: usize = 64 * 1024;

/// Read stack memory from SP upward toward the stack top.
///
/// Uses `vm_region_query` to find the VM region containing SP, then reads
/// from SP to `min(region_top, SP + MAX_STACK_BYTES)`. Falls back to a
/// direct read if the region query fails or the region doesn't contain SP.
fn read_stack_memory(
    plat: &dyn PlatformOps,
    task: mach_port_t,
    sp: u64,
    max_bytes: usize,
    context: &PluginContext,
) -> Result<StackReadOutcome, String> {
    context.checkpoint()?;
    if sp == 0 {
        return Err("SP is null".into());
    }

    let desired_size = compute_read_size(plat, task, sp);
    let read_size = desired_size.min(max_bytes);
    if read_size == 0 {
        return Err("stack byte budget exhausted".into());
    }

    let bytes = plat
        .vm_read(task, sp, read_size)
        .map_err(|e| format!("stack read failed: {e}"))?;
    context.checkpoint()?;

    let truncated = bytes.len() < desired_size;
    Ok(StackReadOutcome {
        capture: RawStackCapture {
            sp,
            bytes,
            truncated,
        },
        budget_limited: read_size < desired_size,
    })
}

struct StackReadOutcome {
    capture: RawStackCapture,
    budget_limited: bool,
}

/// Determine how many bytes to read from SP, using VM region info if available.
fn compute_read_size(plat: &dyn PlatformOps, task: mach_port_t, sp: u64) -> usize {
    let Ok(region) = plat.vm_region_query(task, sp) else {
        return MAX_STACK_BYTES;
    };

    let region_top = region.address.saturating_add(region.size);

    // mach_vm_region returns the region AT OR AFTER the address.
    // Verify that SP actually falls within this region.
    if sp < region.address || sp >= region_top {
        return MAX_STACK_BYTES;
    }

    #[allow(clippy::cast_possible_truncation)] // ARM64 stack sizes fit usize
    let available = (region_top - sp) as usize;
    available.min(MAX_STACK_BYTES)
}

// ═══════════════════════════════════════════════════
//  Memory reading helpers (absorbed from memory_reader.rs)
// ═══════════════════════════════════════════════════

/// Read a single u64 from the target task's address space.
fn read_u64(plat: &dyn PlatformOps, task: mach_port_t, address: u64) -> Result<u64, String> {
    let bytes = plat
        .vm_read(task, address, 8)
        .map_err(|e| format!("read_u64 at {address:#x}: {e}"))?;
    let arr: [u8; 8] = bytes[..8]
        .try_into()
        .map_err(|_| format!("read_u64 at {address:#x}: short read"))?;
    Ok(u64::from_le_bytes(arr))
}

#[cfg(test)]
#[path = "../../tests/unit/collectors/thread_tests.rs"]
mod tests;
