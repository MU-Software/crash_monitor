//! Core types, constants, and name-mapping helpers for macOS Mach APIs.

use mach2::exception_types::{
    EXC_MASK_ARITHMETIC, EXC_MASK_BAD_ACCESS, EXC_MASK_BAD_INSTRUCTION, EXC_MASK_CRASH,
};
use mach2::kern_return::KERN_SUCCESS;
use mach2::message::mach_msg_header_t;
use mach2::port::mach_port_t;

// ═══════════════════════════════════════════════════
//  Constants
// ═══════════════════════════════════════════════════

/// Exception mask covering crash-like exceptions.
pub const CRASH_EXCEPTION_MASK: u32 =
    EXC_MASK_BAD_ACCESS | EXC_MASK_BAD_INSTRUCTION | EXC_MASK_ARITHMETIC | EXC_MASK_CRASH;

/// ARM64 thread state flavor.
pub const ARM_THREAD_STATE64: i32 = 6;

/// ARM64 thread state count (in u32 units).
/// `arm_thread_state64_t` has 33 GPR (x0-x28, fp, lr, sp, pc) + cpsr + pad = 68 u32s
pub const ARM_THREAD_STATE64_COUNT: u32 = 68;

/// ARM64 register names in order.
pub const ARM64_GPR_NAMES: &[&str] = &[
    "x0", "x1", "x2", "x3", "x4", "x5", "x6", "x7", "x8", "x9", "x10", "x11", "x12", "x13", "x14",
    "x15", "x16", "x17", "x18", "x19", "x20", "x21", "x22", "x23", "x24", "x25", "x26", "x27",
    "x28", "fp", "lr",
];

/// Exception behavior flags.
pub const EXCEPTION_STATE_IDENTITY: i32 = 3;
pub const MACH_EXCEPTION_CODES_FLAG: i32 = 0x8000_0000_u32 as i32;

// ═══════════════════════════════════════════════════
//  Mach error type
// ═══════════════════════════════════════════════════

#[derive(Debug)]
pub struct MachError {
    pub function: &'static str,
    pub kern_return: i32,
}

impl std::fmt::Display for MachError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} failed: kr={}", self.function, self.kern_return)
    }
}

impl std::error::Error for MachError {}

// ═══════════════════════════════════════════════════
//  Exception info (from mach_exception.rs)
// ═══════════════════════════════════════════════════

/// Exception info extracted from a Mach exception message.
/// Includes the raw reply header so the main thread can send the reply
/// after crash data collection is complete.
pub struct ExceptionInfo {
    pub thread_port: mach_port_t,
    #[allow(dead_code)] // available for Phase 4+ (shared memory via task port)
    pub task_port: mach_port_t,
    pub exception_type: u32,
    pub code: u64,
    pub subcode: u64,
    /// Copy of the request header needed to construct the reply.
    /// Stored as a value copy so the listener's buffer can be reused.
    pub reply_header: mach_msg_header_t,
}

// ═══════════════════════════════════════════════════
//  VM region / task info types
// ═══════════════════════════════════════════════════

/// Information about a single VM region.
#[derive(Clone)]
#[allow(dead_code)] // share_mode, pages_swapped_out reserved for Phase 4+ analysis
pub struct VmRegionInfo {
    pub address: u64,
    pub size: u64,
    pub protection: i32,
    pub user_tag: u32,
    pub share_mode: u8,
    pub pages_resident: u32,
    pub pages_swapped_out: u32,
}

/// High-level VM statistics for a task.
#[derive(Clone)]
#[allow(dead_code)] // fields available for Phase 4+ report enrichment
pub struct TaskVmSummary {
    pub virtual_size: u64,
    pub resident_size: u64,
    pub phys_footprint: u64,
    pub internal: u64,
    pub compressed: u64,
}

// ═══════════════════════════════════════════════════
//  Name mapping helpers (pure, no unsafe)
// ═══════════════════════════════════════════════════

/// Map an exception type code to its human-readable name.
#[must_use]
pub fn exception_type_name(exc_type: u32) -> &'static str {
    match exc_type {
        1 => "EXC_BAD_ACCESS",
        2 => "EXC_BAD_INSTRUCTION",
        3 => "EXC_ARITHMETIC",
        4 => "EXC_EMULATION",
        5 => "EXC_SOFTWARE",
        6 => "EXC_BREAKPOINT",
        10 => "EXC_CRASH",
        11 => "EXC_RESOURCE",
        12 => "EXC_GUARD",
        _ => "EXC_UNKNOWN",
    }
}

/// Map a `kern_return_t` code to its human-readable name.
#[must_use]
pub fn kern_return_name(code: u64) -> &'static str {
    match code {
        1 => "KERN_INVALID_ADDRESS",
        2 => "KERN_PROTECTION_FAILURE",
        _ => "KERN_UNKNOWN",
    }
}

/// Map an exception type code to the equivalent POSIX signal name.
#[must_use]
pub fn exception_to_signal(exc_type: u32) -> &'static str {
    match exc_type {
        1 => "SIGSEGV",
        2 => "SIGILL",
        3 => "SIGFPE",
        6 => "SIGTRAP",
        10 => "SIGABRT",
        _ => "SIGUNKNOWN",
    }
}

// ═══════════════════════════════════════════════════
//  Internal helpers
// ═══════════════════════════════════════════════════

pub(crate) fn mach_result(function: &'static str, kr: i32) -> Result<(), MachError> {
    if kr == KERN_SUCCESS {
        Ok(())
    } else {
        Err(MachError {
            function,
            kern_return: kr,
        })
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/platform/macos/types_tests.rs"]
mod tests;
