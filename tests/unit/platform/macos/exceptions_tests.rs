use super::*;
use mach2::message::{mach_msg_body_t, mach_msg_header_t, mach_msg_port_descriptor_t};

/// Mirror of the `ExcMsg` struct used inside `parse_exception_message`.
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

/// Create a zeroed `ExcMsg` and return it as a byte vector.
fn make_exc_msg(
    exception: u32,
    code_count: u32,
    code0: u64,
    code1: u64,
    thread: u32,
    task: u32,
) -> Vec<u8> {
    // SAFETY: ExcMsg is repr(C) and all-zero is a valid bit pattern for its fields.
    let mut msg: ExcMsg = unsafe { std::mem::zeroed() };
    msg.thread_port.name = thread;
    msg.task_port.name = task;
    msg.exception = exception;
    msg.code_count = code_count;
    msg.code[0] = code0;
    msg.code[1] = code1;

    let ptr = (&raw const msg).cast::<u8>();
    let len = std::mem::size_of::<ExcMsg>();
    // SAFETY: copying repr(C) struct bytes.
    unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec()
}

#[test]
fn test_parse_valid_exception_message() {
    let buf = make_exc_msg(1, 2, 0xDEAD, 0xBEEF, 42, 99);
    let (thread, task, exc, code, subcode) = parse_exception_message(&buf).unwrap();
    assert_eq!(thread, 42);
    assert_eq!(task, 99);
    assert_eq!(exc, 1);
    assert_eq!(code, 0xDEAD);
    assert_eq!(subcode, 0xBEEF);
}

#[test]
fn test_parse_buffer_too_small() {
    let buf = [0u8; 4];
    let err = parse_exception_message(&buf).unwrap_err();
    assert_eq!(err.function, "parse_exception_message");
    assert_eq!(err.kern_return, -1);
}

#[test]
fn test_parse_code_count_too_large() {
    let buf = make_exc_msg(1, 3, 0, 0, 1, 1);
    let err = parse_exception_message(&buf).unwrap_err();
    assert!(err.function.contains("unexpected code_count"));
    assert_eq!(err.kern_return, -1);
}

#[test]
fn test_parse_code_count_zero() {
    let buf = make_exc_msg(5, 0, 0xAAAA, 0xBBBB, 10, 20);
    let (thread, task, exc, code, subcode) = parse_exception_message(&buf).unwrap();
    assert_eq!(thread, 10);
    assert_eq!(task, 20);
    assert_eq!(exc, 5);
    assert_eq!(code, 0); // code_count=0 → default 0
    assert_eq!(subcode, 0);
}

#[test]
fn test_parse_code_count_one() {
    let buf = make_exc_msg(3, 1, 0x1234, 0x5678, 7, 8);
    let (_, _, _, code, subcode) = parse_exception_message(&buf).unwrap();
    assert_eq!(code, 0x1234);
    assert_eq!(subcode, 0); // code_count=1 → subcode default 0
}

#[test]
fn test_message_header_valid() {
    let buf = [0u8; std::mem::size_of::<mach_msg_header_t>()];
    let header = message_header(&buf).unwrap();
    assert_eq!(header.msgh_size, 0);
    assert_eq!(header.msgh_id, 0);
}

#[test]
fn test_message_header_too_small() {
    let buf = [0u8; 0];
    let err = message_header(&buf).unwrap_err();
    assert!(err.function.contains("buffer too small"));
    assert_eq!(err.kern_return, -1);
}

#[test]
fn test_message_header_exact_size() {
    // Buffer exactly the size of the header should work
    let size = std::mem::size_of::<mach_msg_header_t>();
    let buf = vec![0u8; size];
    assert!(message_header(&buf).is_ok());
}

#[test]
fn test_message_header_one_byte_short() {
    let size = std::mem::size_of::<mach_msg_header_t>();
    let buf = vec![0u8; size - 1];
    assert!(message_header(&buf).is_err());
}
