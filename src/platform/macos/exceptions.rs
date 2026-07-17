//! Pure functions for Mach exception message parsing.

use mach2::message::{mach_msg_body_t, mach_msg_header_t, mach_msg_port_descriptor_t};
use mach2::port::mach_port_t;

use super::types::MachError;

/// Parse a raw exception message buffer into its components.
/// Returns (`thread_port`, `task_port`, `exception_type`, code, subcode).
///
/// # Errors
/// Returns `MachError` if the buffer is too small or the `code_count` field is invalid.
pub fn parse_exception_message(
    buf: &[u8],
) -> Result<(mach_port_t, mach_port_t, u32, u64, u64), MachError> {
    #[repr(C)]
    struct ExcMsg {
        header: mach_msg_header_t,
        body: mach_msg_body_t,
        thread_port: mach_msg_port_descriptor_t,
        task_port: mach_msg_port_descriptor_t,
        ndr: [u8; 8],
        exception: u32,
        code_count: u32,
        code: [u64; 2],
    }

    if buf.len() < std::mem::size_of::<ExcMsg>() {
        return Err(MachError {
            function: "parse_exception_message",
            kern_return: -1,
        });
    }

    // SAFETY: bounds checked above; buf was filled by mach_msg with a valid exception message.
    let msg = unsafe { &*buf.as_ptr().cast::<ExcMsg>() };

    // Validate code_count to guard against unexpected kernel message formats
    if msg.code_count > 2 {
        return Err(MachError {
            function: "parse_exception_message(unexpected code_count)",
            kern_return: -1,
        });
    }

    Ok((
        msg.thread_port.name,
        msg.task_port.name,
        msg.exception,
        if msg.code_count >= 1 { msg.code[0] } else { 0 },
        if msg.code_count >= 2 { msg.code[1] } else { 0 },
    ))
}

/// Get the raw `mach_msg_header_t` from a message buffer.
///
/// # Errors
/// Returns `MachError` if the buffer is smaller than `mach_msg_header_t`.
pub fn message_header(buf: &[u8]) -> Result<&mach_msg_header_t, MachError> {
    if buf.len() < std::mem::size_of::<mach_msg_header_t>() {
        return Err(MachError {
            function: "message_header(buffer too small)",
            kern_return: -1,
        });
    }
    // SAFETY: bounds checked above; buf starts with a valid mach_msg_header_t (filled by mach_msg).
    Ok(unsafe { &*buf.as_ptr().cast::<mach_msg_header_t>() })
}

#[cfg(test)]
#[path = "../../../tests/unit/platform/macos/exceptions_tests.rs"]
mod tests;
