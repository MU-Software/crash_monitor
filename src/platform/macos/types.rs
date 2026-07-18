//! Core types, constants, and name-mapping helpers for macOS Mach APIs.

use mach2::exception_types::{
    EXC_MASK_ARITHMETIC, EXC_MASK_BAD_ACCESS, EXC_MASK_BAD_INSTRUCTION, EXC_MASK_BREAKPOINT,
    EXC_MASK_CRASH, EXC_MASK_GUARD,
};
use mach2::kern_return::KERN_SUCCESS;
use serde::{Deserialize, Serialize};
use std::time::Instant;

use crate::platform::macos::ffi::exceptions::ReceivedMachMessage;

// ═══════════════════════════════════════════════════
//  Constants
// ═══════════════════════════════════════════════════

/// Exception mask covering exception classes that the monitor treats as fatal
/// crash reports. Resource exceptions are intentionally omitted: they may be
/// advisory/non-fatal and have no stable POSIX termination mapping.
pub const CRASH_EXCEPTION_MASK: u32 = EXC_MASK_BAD_ACCESS
    | EXC_MASK_BAD_INSTRUCTION
    | EXC_MASK_ARITHMETIC
    | EXC_MASK_BREAKPOINT
    | EXC_MASK_CRASH
    | EXC_MASK_GUARD;

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

/// Exception info extracted from a Mach exception message. The original
/// receive buffer owns all descriptor and reply rights until the event loop
/// has completed capture and its bounded reply attempt.
pub struct ExceptionInfo {
    /// Monotonic timestamp captured immediately after `mach_msg` received the
    /// request. Capture deadlines must use this instead of a later poll time.
    pub received_at: Instant,
    pub exception_type: u32,
    pub code: u64,
    pub subcode: u64,
    /// Exact MIG `mach_exception_data_t` array, including its original count.
    pub raw_codes: Vec<u64>,
    pub request: ReceivedMachMessage,
}

/// Messages emitted by the Mach exception listener thread.
///
/// A fatal receive/reply failure is explicit instead of being collapsed into
/// an undifferentiated channel disconnect. The supervisor also treats an
/// unexpected sender disconnect as fatal.
pub enum ExceptionListenerEvent {
    Exception(ExceptionInfo),
    Fatal { message: String },
}

// ═══════════════════════════════════════════════════
//  VM region / task info types
// ═══════════════════════════════════════════════════

/// Information about a single VM region.
#[derive(Clone, Serialize, Deserialize)]
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
#[derive(Clone, Serialize, Deserialize)]
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

/// Report category selected for a subscribed Mach exception.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExceptionReportKind {
    Crash,
}

/// Severity written into the exception report schema.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExceptionSeverity {
    #[default]
    Fatal,
}

/// Complete subscription and reporting policy for one Mach exception class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExceptionPolicy {
    pub subscribed: bool,
    pub report_kind: Option<ExceptionReportKind>,
    pub severity: Option<ExceptionSeverity>,
    pub signal: &'static str,
    pub preserves_raw_codes: bool,
}

const fn fatal_crash(signal: &'static str) -> ExceptionPolicy {
    ExceptionPolicy {
        subscribed: true,
        report_kind: Some(ExceptionReportKind::Crash),
        severity: Some(ExceptionSeverity::Fatal),
        signal,
        preserves_raw_codes: true,
    }
}

const fn ignored_resource() -> ExceptionPolicy {
    ExceptionPolicy {
        subscribed: false,
        report_kind: None,
        severity: None,
        signal: "SIGUNKNOWN",
        preserves_raw_codes: false,
    }
}

const fn unknown_exception() -> ExceptionPolicy {
    ExceptionPolicy {
        subscribed: false,
        report_kind: None,
        severity: None,
        signal: "SIGUNKNOWN",
        preserves_raw_codes: false,
    }
}

/// Return the explicit monitor policy for a Mach exception type.
///
/// `EXC_BREAKPOINT` and `EXC_GUARD` are fatal crash subscriptions. The monitor
/// deliberately ignores `EXC_RESOURCE`; treating an advisory resource event as
/// terminal would stop supervision before the child has actually exited.
#[must_use]
pub const fn exception_policy(exc_type: u32) -> ExceptionPolicy {
    match exc_type {
        1 => fatal_crash("SIGSEGV"),
        2 => fatal_crash("SIGILL"),
        3 => fatal_crash("SIGFPE"),
        6 => fatal_crash("SIGTRAP"),
        10 => fatal_crash("SIGABRT"),
        11 => ignored_resource(),
        12 => fatal_crash("SIGKILL"),
        _ => unknown_exception(),
    }
}

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
        0 => "KERN_SUCCESS",
        1 => "KERN_INVALID_ADDRESS",
        2 => "KERN_PROTECTION_FAILURE",
        3 => "KERN_NO_SPACE",
        4 => "KERN_INVALID_ARGUMENT",
        5 => "KERN_FAILURE",
        6 => "KERN_RESOURCE_SHORTAGE",
        7 => "KERN_NOT_RECEIVER",
        8 => "KERN_NO_ACCESS",
        9 => "KERN_MEMORY_FAILURE",
        10 => "KERN_MEMORY_ERROR",
        14 => "KERN_ABORTED",
        15 => "KERN_INVALID_NAME",
        16 => "KERN_INVALID_TASK",
        17 => "KERN_INVALID_RIGHT",
        18 => "KERN_INVALID_VALUE",
        20 => "KERN_INVALID_CAPABILITY",
        _ => "KERN_UNKNOWN",
    }
}

/// Type-aware presentation of a Mach exception code array.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DecodedException {
    pub code_name: Option<&'static str>,
    pub signal: &'static str,
    pub signal_is_approximate: bool,
    pub fault_address: Option<u64>,
}

/// Decode only meanings defined for the specific exception class. Numeric raw
/// values remain authoritative in the report regardless of this projection.
#[must_use]
pub fn decode_exception(exc_type: u32, raw_codes: &[u64]) -> DecodedException {
    let code = raw_codes.first().copied().unwrap_or(0);
    let subcode = raw_codes.get(1).copied();
    match exc_type {
        1 => DecodedException {
            code_name: Some(kern_return_name(code)),
            signal: if code == 2 { "SIGBUS" } else { "SIGSEGV" },
            signal_is_approximate: false,
            fault_address: subcode,
        },
        10 => DecodedException {
            code_name: None,
            // EXC_CRASH encodings vary by producer/kernel and this monitor
            // does not yet have an authoritative signal subfield decoder.
            signal: "SIGABRT",
            signal_is_approximate: true,
            fault_address: None,
        },
        _ => DecodedException {
            code_name: None,
            signal: exception_to_signal(exc_type),
            signal_is_approximate: false,
            fault_address: None,
        },
    }
}

/// Map an exception type code to the equivalent POSIX signal name.
#[must_use]
pub fn exception_to_signal(exc_type: u32) -> &'static str {
    exception_policy(exc_type).signal
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
