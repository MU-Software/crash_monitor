//! Alignment-independent parsing for Mach exception MIG messages.
//!
//! The wire layout comes from the SDK's `mach/mach_exc.defs` and generated
//! `mach_exc_server.h`. MIG packs these request records on four-byte
//! boundaries, so the `int64_t code[]` array begins at byte 68 rather than at
//! the alignment chosen by Rust's `repr(C)`. Keep this module byte-oriented:
//! a receive buffer is not a Rust instance of the generated C structure.

use mach2::kern_return::KERN_FAILURE;
use mach2::message::{
    MACH_MSG_TYPE_MOVE_SEND, MACH_MSG_TYPE_MOVE_SEND_ONCE, MACH_MSGH_BITS, MACH_MSGH_BITS_COMPLEX,
    MACH_MSGH_BITS_REMOTE_MASK, mach_msg_header_t,
};
use mach2::port::{MACH_PORT_DEAD, MACH_PORT_NULL, mach_port_t};

/// MIG routine number for `mach_exception_raise_state_identity`.
///
/// The monitor registers `EXCEPTION_STATE_IDENTITY | MACH_EXCEPTION_CODES`,
/// which selects this routine from the `mach_exc` subsystem (base ID 2405).
pub const MACH_EXCEPTION_RAISE_STATE_IDENTITY_ID: i32 = 2407;

pub(crate) const MACH_HEADER_SIZE: usize = 24;
const BODY_OFFSET: usize = MACH_HEADER_SIZE;
const PORT_DESCRIPTOR_SIZE: usize = 12;
const THREAD_DESCRIPTOR_OFFSET: usize = BODY_OFFSET + 4;
const TASK_DESCRIPTOR_OFFSET: usize = THREAD_DESCRIPTOR_OFFSET + PORT_DESCRIPTOR_SIZE;
const NDR_OFFSET: usize = TASK_DESCRIPTOR_OFFSET + PORT_DESCRIPTOR_SIZE;
const EXCEPTION_OFFSET: usize = NDR_OFFSET + 8;
const CODE_COUNT_OFFSET: usize = EXCEPTION_OFFSET + 4;
const CODE_OFFSET: usize = CODE_COUNT_OFFSET + 4;
const MAX_CODE_COUNT: u32 = 2;
const MAX_THREAD_STATE_COUNT: u32 = 1296;
const MACH_MSG_PORT_DESCRIPTOR: u8 = 0;
const RECEIVED_SEND_RIGHT: u8 = MACH_MSG_TYPE_MOVE_SEND as u8;

const NATIVE_NDR_INT_REP: u8 = if cfg!(target_endian = "little") { 1 } else { 0 };
const NATIVE_NDR_RECORD: [u8; 8] = [0, 0, 0, 0, NATIVE_NDR_INT_REP, 0, 0, 0];

/// A validated `mach_exception_raise_state_identity` request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedExceptionMessage {
    pub reply_header: mach_msg_header_t,
    pub thread_port: mach_port_t,
    pub task_port: mach_port_t,
    pub exception_type: u32,
    /// Exact bit patterns and element count delivered by MIG's `int64_t[]`.
    pub raw_codes: Vec<u64>,
}

impl ParsedExceptionMessage {
    #[must_use]
    pub fn code(&self) -> u64 {
        self.raw_codes.first().copied().unwrap_or(0)
    }

    #[must_use]
    pub fn subcode(&self) -> u64 {
        self.raw_codes.get(1).copied().unwrap_or(0)
    }
}

/// Structural failure while decoding a Mach exception request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExceptionMessageError {
    Truncated {
        field: &'static str,
        needed: usize,
        actual: usize,
    },
    HeaderSizeMismatch {
        declared: usize,
        actual: usize,
    },
    UnexpectedMessageId {
        actual: i32,
    },
    MissingComplexBit,
    UnexpectedDescriptorCount {
        actual: u32,
    },
    InvalidPortDescriptor {
        index: usize,
        descriptor_type: u8,
        disposition: u8,
    },
    UnsupportedNdrIntegerRepresentation {
        actual: u8,
    },
    InvalidCodeCount {
        actual: u32,
    },
    InvalidThreadStateCount {
        actual: u32,
    },
    InvalidPackedSize {
        expected: usize,
        actual: usize,
    },
    UnsafeReplyIdentity,
}

impl std::fmt::Display for ExceptionMessageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Truncated {
                field,
                needed,
                actual,
            } => write!(
                f,
                "Mach exception request is truncated at {field}: need {needed} bytes, got {actual}"
            ),
            Self::HeaderSizeMismatch { declared, actual } => write!(
                f,
                "Mach exception msgh_size mismatch: header says {declared}, received {actual}"
            ),
            Self::UnexpectedMessageId { actual } => write!(
                f,
                "unexpected Mach exception message ID {actual} (expected {MACH_EXCEPTION_RAISE_STATE_IDENTITY_ID})"
            ),
            Self::MissingComplexBit => {
                f.write_str("Mach exception request is missing MACH_MSGH_BITS_COMPLEX")
            }
            Self::UnexpectedDescriptorCount { actual } => {
                write!(
                    f,
                    "Mach exception request has {actual} descriptors (expected 2)"
                )
            }
            Self::InvalidPortDescriptor {
                index,
                descriptor_type,
                disposition,
            } => write!(
                f,
                "Mach exception descriptor {index} has type {descriptor_type} and disposition {disposition} (expected port/send)"
            ),
            Self::UnsupportedNdrIntegerRepresentation { actual } => write!(
                f,
                "Mach exception request uses unsupported NDR integer representation {actual}"
            ),
            Self::InvalidCodeCount { actual } => {
                write!(f, "Mach exception request has invalid code count {actual}")
            }
            Self::InvalidThreadStateCount { actual } => {
                write!(
                    f,
                    "Mach exception request has invalid thread-state count {actual}"
                )
            }
            Self::InvalidPackedSize { expected, actual } => write!(
                f,
                "Mach exception packed size mismatch: expected {expected}, got {actual}"
            ),
            Self::UnsafeReplyIdentity => {
                f.write_str("Mach request does not contain a safe reply identity")
            }
        }
    }
}

impl std::error::Error for ExceptionMessageError {}

fn bytes<const N: usize>(
    buf: &[u8],
    offset: usize,
    field: &'static str,
) -> Result<[u8; N], ExceptionMessageError> {
    let end = offset
        .checked_add(N)
        .ok_or(ExceptionMessageError::Truncated {
            field,
            needed: usize::MAX,
            actual: buf.len(),
        })?;
    let slice = buf
        .get(offset..end)
        .ok_or(ExceptionMessageError::Truncated {
            field,
            needed: end,
            actual: buf.len(),
        })?;
    slice
        .try_into()
        .map_err(|_| ExceptionMessageError::Truncated {
            field,
            needed: end,
            actual: buf.len(),
        })
}

fn u32_at(buf: &[u8], offset: usize, field: &'static str) -> Result<u32, ExceptionMessageError> {
    Ok(u32::from_ne_bytes(bytes(buf, offset, field)?))
}

fn i32_at(buf: &[u8], offset: usize, field: &'static str) -> Result<i32, ExceptionMessageError> {
    Ok(i32::from_ne_bytes(bytes(buf, offset, field)?))
}

fn u64_at(buf: &[u8], offset: usize, field: &'static str) -> Result<u64, ExceptionMessageError> {
    Ok(u64::from_ne_bytes(bytes(buf, offset, field)?))
}

/// Copy the wire header into an owned, correctly aligned Rust value.
///
/// No reference into `buf` is created.
///
/// # Errors
/// Returns [`ExceptionMessageError::Truncated`] unless all fixed header fields
/// are present.
pub fn message_header(buf: &[u8]) -> Result<mach_msg_header_t, ExceptionMessageError> {
    Ok(mach_msg_header_t {
        msgh_bits: u32_at(buf, 0, "msgh_bits")?,
        msgh_size: u32_at(buf, 4, "msgh_size")?,
        msgh_remote_port: u32_at(buf, 8, "msgh_remote_port")?,
        msgh_local_port: u32_at(buf, 12, "msgh_local_port")?,
        msgh_voucher_port: u32_at(buf, 16, "msgh_voucher_port")?,
        msgh_id: i32_at(buf, 20, "msgh_id")?,
    })
}

fn parse_port_descriptor(
    buf: &[u8],
    offset: usize,
    index: usize,
) -> Result<mach_port_t, ExceptionMessageError> {
    let name = u32_at(buf, offset, "port descriptor name")?;
    let descriptor = bytes::<12>(buf, offset, "port descriptor")?;
    let disposition = descriptor[10];
    let descriptor_type = descriptor[11];
    if descriptor_type != MACH_MSG_PORT_DESCRIPTOR || disposition != RECEIVED_SEND_RIGHT {
        return Err(ExceptionMessageError::InvalidPortDescriptor {
            index,
            descriptor_type,
            disposition,
        });
    }
    Ok(name)
}

/// Parse the exact bytes named by a received request's `msgh_size`.
///
/// The caller must pass only the message bytes, excluding any receive trailer.
/// Every variable-length MIG field is accounted for when checking the final
/// packed size.
///
/// # Errors
/// Returns [`ExceptionMessageError`] when any header, descriptor, NDR,
/// variable-count, or packed-size invariant does not match the registered MIG
/// routine.
pub fn parse_exception_message(
    buf: &[u8],
) -> Result<ParsedExceptionMessage, ExceptionMessageError> {
    let header = message_header(buf)?;
    let declared_size = header.msgh_size as usize;
    if declared_size != buf.len() {
        return Err(ExceptionMessageError::HeaderSizeMismatch {
            declared: declared_size,
            actual: buf.len(),
        });
    }
    if header.msgh_id != MACH_EXCEPTION_RAISE_STATE_IDENTITY_ID {
        return Err(ExceptionMessageError::UnexpectedMessageId {
            actual: header.msgh_id,
        });
    }
    if header.msgh_bits & MACH_MSGH_BITS_COMPLEX == 0 {
        return Err(ExceptionMessageError::MissingComplexBit);
    }
    if !has_safe_reply_identity(&header) {
        return Err(ExceptionMessageError::UnsafeReplyIdentity);
    }

    let descriptor_count = u32_at(buf, BODY_OFFSET, "msgh_descriptor_count")?;
    if descriptor_count != 2 {
        return Err(ExceptionMessageError::UnexpectedDescriptorCount {
            actual: descriptor_count,
        });
    }
    let thread_port = parse_port_descriptor(buf, THREAD_DESCRIPTOR_OFFSET, 0)?;
    let task_port = parse_port_descriptor(buf, TASK_DESCRIPTOR_OFFSET, 1)?;

    let ndr = bytes::<8>(buf, NDR_OFFSET, "NDR")?;
    if ndr[4] != NATIVE_NDR_INT_REP {
        return Err(ExceptionMessageError::UnsupportedNdrIntegerRepresentation { actual: ndr[4] });
    }

    let exception_type = u32_at(buf, EXCEPTION_OFFSET, "exception")?;
    let code_count = u32_at(buf, CODE_COUNT_OFFSET, "codeCnt")?;
    if code_count > MAX_CODE_COUNT {
        return Err(ExceptionMessageError::InvalidCodeCount { actual: code_count });
    }

    let code_count = code_count as usize;
    let mut raw_codes = Vec::with_capacity(code_count);
    for index in 0..code_count {
        let offset = CODE_OFFSET + index * 8;
        raw_codes.push(u64_at(buf, offset, "code")?);
    }

    // MIG packs the fields after the variable `code[]` immediately after the
    // actual element count, not after the maximum two-element declaration.
    let flavor_offset = CODE_OFFSET + code_count * 8;
    let _flavor = i32_at(buf, flavor_offset, "flavor")?;
    let state_count_offset = flavor_offset + 4;
    let state_count = u32_at(buf, state_count_offset, "old_stateCnt")?;
    if state_count > MAX_THREAD_STATE_COUNT {
        return Err(ExceptionMessageError::InvalidThreadStateCount {
            actual: state_count,
        });
    }
    let expected_size = state_count_offset
        .checked_add(4)
        .and_then(|base| base.checked_add(state_count as usize * 4))
        .ok_or(ExceptionMessageError::InvalidPackedSize {
            expected: usize::MAX,
            actual: buf.len(),
        })?;
    if expected_size != buf.len() {
        return Err(ExceptionMessageError::InvalidPackedSize {
            expected: expected_size,
            actual: buf.len(),
        });
    }

    Ok(ParsedExceptionMessage {
        reply_header: header,
        thread_port,
        task_port,
        exception_type,
        raw_codes,
    })
}

/// Minimal MIG error reply used to reject malformed exception requests.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ExceptionReply {
    pub(crate) header: mach_msg_header_t,
    pub(crate) ndr: [u8; 8],
    pub(crate) ret_code: i32,
}

/// Construct a `KERN_FAILURE` reply if the received header owns a usable
/// send/send-once reply right.
pub(crate) fn build_exception_reply(
    request_header: &mach_msg_header_t,
) -> Result<ExceptionReply, ExceptionMessageError> {
    let reply_disposition = request_header.msgh_bits & MACH_MSGH_BITS_REMOTE_MASK;
    let reply_id = request_header
        .msgh_id
        .checked_add(100)
        .ok_or(ExceptionMessageError::UnsafeReplyIdentity)?;
    if !has_safe_reply_identity(request_header) {
        return Err(ExceptionMessageError::UnsafeReplyIdentity);
    }

    Ok(ExceptionReply {
        header: mach_msg_header_t {
            msgh_bits: MACH_MSGH_BITS(reply_disposition, 0),
            msgh_size: std::mem::size_of::<ExceptionReply>() as u32,
            msgh_remote_port: request_header.msgh_remote_port,
            msgh_local_port: MACH_PORT_NULL,
            msgh_voucher_port: MACH_PORT_NULL,
            msgh_id: reply_id,
        },
        ndr: NATIVE_NDR_RECORD,
        ret_code: KERN_FAILURE,
    })
}

fn has_safe_reply_identity(header: &mach_msg_header_t) -> bool {
    let reply_disposition = header.msgh_bits & MACH_MSGH_BITS_REMOTE_MASK;
    let valid_disposition = reply_disposition == MACH_MSG_TYPE_MOVE_SEND
        || reply_disposition == MACH_MSG_TYPE_MOVE_SEND_ONCE;
    let valid_port =
        header.msgh_remote_port != MACH_PORT_NULL && header.msgh_remote_port != MACH_PORT_DEAD;
    valid_disposition && valid_port && header.msgh_id.checked_add(100).is_some()
}

/// Recover and construct a failure reply directly from malformed request
/// bytes. Header-independent parse failures can therefore still unblock the
/// faulting thread.
pub(crate) fn failure_reply_for_message(buf: &[u8]) -> Option<ExceptionReply> {
    let header = message_header(buf).ok()?;
    build_exception_reply(&header).ok()
}

#[cfg(test)]
#[path = "../../../tests/unit/platform/macos/exceptions_tests.rs"]
mod tests;
