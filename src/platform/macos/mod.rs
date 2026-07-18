//! macOS-specific constants, types, and safe wrappers for Mach kernel APIs.
//!
//! All `unsafe` Mach API calls are confined to the `ffi` submodule.
//! The rest of the codebase calls these safe wrappers exclusively.
//!
//! Mach API interop requires casts between Rust types and kernel types (u32↔i32,
//! usize→u32, byte buffers→C structs). These are safe on macOS 64-bit where
//! Mach message sizes and thread counts fit in u32.
#![allow(
    clippy::cast_possible_truncation, // Mach msg sizes / thread counts fit u32
    clippy::cast_possible_wrap,       // kern_return_t is i32, masks are u32
    clippy::cast_sign_loss,           // duration .max(0) guarantees non-negative
    clippy::cast_ptr_alignment,       // byte buffers are from mach_msg, properly aligned by kernel
)]

mod child_output;
mod exceptions;
pub(crate) mod ffi;
mod memory;
mod thread;
pub mod types;

// Re-export testable types and pure functions
#[allow(unused_imports)] // ChildStreamTail is part of the public snapshot schema.
pub use child_output::{
    ChildOutputCapture, ChildOutputSnapshot, ChildStreamTail, DEFAULT_CHILD_OUTPUT_TAIL_BYTES,
};
#[allow(unused_imports)]
pub use exceptions::{
    ExceptionMessageError, MACH_EXCEPTION_RAISE_STATE_IDENTITY_ID, ParsedExceptionMessage,
    message_header, parse_exception_message,
};
pub use memory::VmRegionEnumerationQuality;
#[allow(unused_imports)]
pub(crate) use memory::{MAX_VM_REGIONS, VmEnumAction, VmEnumerationState};
#[allow(unused_imports)]
pub(crate) use thread::extract_thread_name;
pub use types::{
    ARM_THREAD_STATE64, ARM_THREAD_STATE64_COUNT, ARM64_GPR_NAMES, CRASH_EXCEPTION_MASK,
    DecodedException, EXCEPTION_STATE_IDENTITY, ExceptionInfo, ExceptionListenerEvent,
    ExceptionPolicy, ExceptionReportKind, ExceptionSeverity, MACH_EXCEPTION_CODES_FLAG, MachError,
    TaskVmSummary, VmRegionInfo, decode_exception, exception_policy, exception_to_signal,
    exception_type_name, kern_return_name,
};

// Deliberately curated boundary over the private FFI modules.
pub use ffi::exceptions::{
    ReceivedMachMessage, create_exception_port, send_deferred_reply, start_listener,
};
pub use ffi::memory::{
    enumerate_vm_regions, get_task_info_words, get_task_vm_info, vm_read, vm_region_query,
};
pub use ffi::spawn::{
    ParentDeathGuard, SpawnError, SpawnStage, allocate_receive_port, insert_send_right,
    spawn_with_exception_port, spawn_with_exception_port_and_output,
};
pub use ffi::task::{
    deallocate_task_port, deallocate_thread_port, get_task_for_pid, get_task_threads, resume_task,
    retain_task_port, suspend_task, terminate_task,
};
pub use ffi::thread::{get_thread_identifier, get_thread_name, get_thread_state};
pub use ffi::types::{OwnedExceptionPort, OwnedTaskPort};
