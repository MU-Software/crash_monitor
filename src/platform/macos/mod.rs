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
pub mod ffi;
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
pub use types::*;

// Re-export FFI functions so callers' paths don't change
pub use ffi::exceptions::*;
pub use ffi::memory::*;
pub use ffi::spawn::*;
pub use ffi::task::*;
pub use ffi::thread::*;
pub use ffi::types::*;
