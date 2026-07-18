use super::*;
use mach2::message::{MACH_MSG_TYPE_MOVE_SEND_ONCE, MACH_MSGH_BITS, MACH_MSGH_BITS_COMPLEX};

const FIXTURE_THREAD_PORT: u32 = 0x1020_3040;
const FIXTURE_TASK_PORT: u32 = 0x5060_7080;
const FIXTURE_REPLY_PORT: u32 = 0x90a0_b0c0;
const FIXTURE_CODE: u64 = 0xfedc_ba98_7654_3210;
const FIXTURE_SUBCODE: u64 = 0x0123_4567_89ab_cdef;
const FIXTURE_STATE_COUNT: u32 = 68;
const FIX_MESSAGE_ID: i32 = 2407;
// Literal offsets from the SDK-generated `__Request__mach_exception_raise_state_identity_t`
// with MIG's four-byte packing. Do not reuse production parser constants here:
// this fixture must fail if those constants drift.
const FIX_HEADER_SIZE: usize = 24;
const FIX_BODY_OFFSET: usize = 24;
const FIX_THREAD_DESCRIPTOR_OFFSET: usize = 28;
const FIX_TASK_DESCRIPTOR_OFFSET: usize = 40;
const FIX_NDR_OFFSET: usize = 52;
const FIX_EXCEPTION_OFFSET: usize = 60;
const FIX_CODE_COUNT_OFFSET: usize = 64;
const FIX_CODE_OFFSET: usize = 68;
const FIX_PORT_DESCRIPTOR_TYPE: u8 = 0;
const FIX_RECEIVED_SEND_DISPOSITION: u8 = 17;
const FIX_NDR_RECORD: [u8; 8] = [0, 0, 0, 0, 1, 0, 0, 0];

fn put_u32(buf: &mut [u8], offset: usize, value: u32) {
    buf[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
}

fn put_i32(buf: &mut [u8], offset: usize, value: i32) {
    buf[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
}

fn put_u64(buf: &mut [u8], offset: usize, value: u64) {
    buf[offset..offset + 8].copy_from_slice(&value.to_ne_bytes());
}

fn put_port_descriptor(buf: &mut [u8], offset: usize, name: u32) {
    put_u32(buf, offset, name);
    put_u32(buf, offset + 4, 0);
    buf[offset + 8..offset + 10].copy_from_slice(&0u16.to_ne_bytes());
    buf[offset + 10] = FIX_RECEIVED_SEND_DISPOSITION;
    buf[offset + 11] = FIX_PORT_DESCRIPTOR_TYPE;
}

/// Fixture following the request emitted by the SDK MIG definition:
///
/// ```text
/// Head(24), body(4), thread desc(12), task desc(12), NDR(8),
/// exception(4), codeCnt(4), code[codeCnt](8 each),
/// flavor(4), old_stateCnt(4), old_state[old_stateCnt](4 each)
/// ```
///
/// Crucially, `code[0]` is at byte 68 under MIG's `#pragma pack(4)`.
fn kernel_mig_fixture(codes: &[u64]) -> Vec<u8> {
    assert!(codes.len() <= 2);
    let state_count_offset = FIX_CODE_OFFSET + codes.len() * 8 + 4;
    let size = state_count_offset + 4 + FIXTURE_STATE_COUNT as usize * 4;
    let mut buf = vec![0u8; size];

    put_u32(
        &mut buf,
        0,
        MACH_MSGH_BITS_COMPLEX | MACH_MSGH_BITS(MACH_MSG_TYPE_MOVE_SEND_ONCE, 0),
    );
    put_u32(&mut buf, 4, size as u32);
    put_u32(&mut buf, 8, FIXTURE_REPLY_PORT);
    put_u32(&mut buf, 12, 0x44);
    put_i32(&mut buf, 20, FIX_MESSAGE_ID);
    put_u32(&mut buf, FIX_BODY_OFFSET, 2);
    put_port_descriptor(&mut buf, FIX_THREAD_DESCRIPTOR_OFFSET, FIXTURE_THREAD_PORT);
    put_port_descriptor(&mut buf, FIX_TASK_DESCRIPTOR_OFFSET, FIXTURE_TASK_PORT);
    buf[FIX_NDR_OFFSET..FIX_NDR_OFFSET + 8].copy_from_slice(&FIX_NDR_RECORD);
    put_u32(&mut buf, FIX_EXCEPTION_OFFSET, 1);
    put_u32(&mut buf, FIX_CODE_COUNT_OFFSET, codes.len() as u32);
    for (index, code) in codes.iter().enumerate() {
        put_u64(&mut buf, FIX_CODE_OFFSET + index * 8, *code);
    }
    let flavor_offset = FIX_CODE_OFFSET + codes.len() * 8;
    put_i32(&mut buf, flavor_offset, 6);
    put_u32(&mut buf, state_count_offset, FIXTURE_STATE_COUNT);
    for index in 0..FIXTURE_STATE_COUNT as usize {
        put_u32(&mut buf, state_count_offset + 4 + index * 4, index as u32);
    }
    buf
}

#[test]
fn parses_real_mig_pack4_layout_and_preserves_raw_codes() {
    let fixture = kernel_mig_fixture(&[FIXTURE_CODE, FIXTURE_SUBCODE]);

    assert_eq!(FIX_CODE_OFFSET, 68);
    assert_eq!(FIX_HEADER_SIZE, 24);
    assert_eq!(
        &fixture[FIX_CODE_OFFSET..FIX_CODE_OFFSET + 8],
        &FIXTURE_CODE.to_ne_bytes()
    );
    let parsed = parse_exception_message(&fixture).expect("valid kernel MIG fixture");

    assert_eq!(parsed.thread_port, FIXTURE_THREAD_PORT);
    assert_eq!(parsed.task_port, FIXTURE_TASK_PORT);
    assert_eq!(parsed.exception_type, 1);
    assert_eq!(parsed.raw_codes, vec![FIXTURE_CODE, FIXTURE_SUBCODE]);
    assert_eq!(parsed.code(), FIXTURE_CODE);
    assert_eq!(parsed.subcode(), FIXTURE_SUBCODE);
    assert_eq!(parsed.reply_header.msgh_id, FIX_MESSAGE_ID);
}

#[test]
fn parses_a_deliberately_misaligned_slice_without_struct_references() {
    let fixture = kernel_mig_fixture(&[FIXTURE_CODE, FIXTURE_SUBCODE]);
    let mut storage = vec![0xa5];
    storage.extend_from_slice(&fixture);

    let parsed = parse_exception_message(&storage[1..]).expect("alignment-independent parser");
    assert_eq!(parsed.raw_codes, vec![FIXTURE_CODE, FIXTURE_SUBCODE]);
}

#[test]
fn variable_code_array_is_packed_tight() {
    for codes in [
        vec![],
        vec![FIXTURE_CODE],
        vec![FIXTURE_CODE, FIXTURE_SUBCODE],
    ] {
        let fixture = kernel_mig_fixture(&codes);
        let parsed = parse_exception_message(&fixture).expect("valid variable code count");
        assert_eq!(parsed.raw_codes, codes);
        assert_eq!(parsed.code(), codes.first().copied().unwrap_or(0));
        assert_eq!(parsed.subcode(), codes.get(1).copied().unwrap_or(0));
    }
}

#[test]
fn every_truncation_of_the_kernel_fixture_is_rejected() {
    let fixture = kernel_mig_fixture(&[FIXTURE_CODE, FIXTURE_SUBCODE]);

    for length in 0..fixture.len() {
        let mut truncated = fixture[..length].to_vec();
        if length >= 8 {
            put_u32(&mut truncated, 4, length as u32);
        }
        assert!(
            parse_exception_message(&truncated).is_err(),
            "accepted truncation at {length} bytes"
        );
    }
}

#[test]
fn rejects_header_identity_and_received_size_mismatches() {
    let fixture = kernel_mig_fixture(&[FIXTURE_CODE, FIXTURE_SUBCODE]);

    let mut wrong_id = fixture.clone();
    put_i32(&mut wrong_id, 20, FIX_MESSAGE_ID - 1);
    assert!(matches!(
        parse_exception_message(&wrong_id),
        Err(ExceptionMessageError::UnexpectedMessageId { .. })
    ));

    let mut wrong_size = fixture;
    let declared = wrong_size.len() - 1;
    put_u32(&mut wrong_size, 4, declared as u32);
    assert_eq!(
        parse_exception_message(&wrong_size),
        Err(ExceptionMessageError::HeaderSizeMismatch {
            declared,
            actual: wrong_size.len(),
        })
    );
}

#[test]
fn rejects_non_complex_and_wrong_descriptor_count() {
    let fixture = kernel_mig_fixture(&[FIXTURE_CODE, FIXTURE_SUBCODE]);

    let mut simple = fixture.clone();
    put_u32(
        &mut simple,
        0,
        MACH_MSGH_BITS(MACH_MSG_TYPE_MOVE_SEND_ONCE, 0),
    );
    assert_eq!(
        parse_exception_message(&simple),
        Err(ExceptionMessageError::MissingComplexBit)
    );

    let mut wrong_count = fixture;
    put_u32(&mut wrong_count, FIX_BODY_OFFSET, 1);
    assert_eq!(
        parse_exception_message(&wrong_count),
        Err(ExceptionMessageError::UnexpectedDescriptorCount { actual: 1 })
    );
}

#[test]
fn rejects_each_descriptor_type_and_disposition_mismatch() {
    let fixture = kernel_mig_fixture(&[FIXTURE_CODE, FIXTURE_SUBCODE]);
    for (index, offset) in [FIX_THREAD_DESCRIPTOR_OFFSET, FIX_TASK_DESCRIPTOR_OFFSET]
        .into_iter()
        .enumerate()
    {
        let mut wrong_type = fixture.clone();
        wrong_type[offset + 11] = 1;
        assert!(matches!(
            parse_exception_message(&wrong_type),
            Err(ExceptionMessageError::InvalidPortDescriptor {
                index: actual_index,
                descriptor_type: 1,
                ..
            }) if actual_index == index
        ));

        let mut wrong_disposition = fixture.clone();
        wrong_disposition[offset + 10] = 19;
        assert!(matches!(
            parse_exception_message(&wrong_disposition),
            Err(ExceptionMessageError::InvalidPortDescriptor {
                index: actual_index,
                disposition: 19,
                ..
            }) if actual_index == index
        ));
    }
}

#[test]
fn rejects_invalid_ndr_code_count_and_state_count() {
    let fixture = kernel_mig_fixture(&[FIXTURE_CODE, FIXTURE_SUBCODE]);

    let mut foreign_ndr = fixture.clone();
    foreign_ndr[FIX_NDR_OFFSET + 4] ^= 1;
    assert!(matches!(
        parse_exception_message(&foreign_ndr),
        Err(ExceptionMessageError::UnsupportedNdrIntegerRepresentation { .. })
    ));

    let mut too_many_codes = fixture.clone();
    put_u32(&mut too_many_codes, FIX_CODE_COUNT_OFFSET, 3);
    assert_eq!(
        parse_exception_message(&too_many_codes),
        Err(ExceptionMessageError::InvalidCodeCount { actual: 3 })
    );

    let mut too_much_state = fixture;
    let state_count_offset = FIX_CODE_OFFSET + 2 * 8 + 4;
    put_u32(&mut too_much_state, state_count_offset, 1297);
    assert_eq!(
        parse_exception_message(&too_much_state),
        Err(ExceptionMessageError::InvalidThreadStateCount { actual: 1297 })
    );
}

#[test]
fn rejects_trailing_bytes_even_when_msgh_size_includes_them() {
    let mut fixture = kernel_mig_fixture(&[FIXTURE_CODE, FIXTURE_SUBCODE]);
    fixture.push(0);
    let size = fixture.len() as u32;
    put_u32(&mut fixture, 4, size);

    assert!(matches!(
        parse_exception_message(&fixture),
        Err(ExceptionMessageError::InvalidPackedSize { .. })
    ));
}

#[test]
fn malformed_request_with_safe_identity_gets_kern_failure_reply() {
    let mut fixture = kernel_mig_fixture(&[FIXTURE_CODE, FIXTURE_SUBCODE]);
    let bad_id = FIX_MESSAGE_ID + 40;
    put_i32(&mut fixture, 20, bad_id);
    assert!(parse_exception_message(&fixture).is_err());

    let reply = failure_reply_for_message(&fixture).expect("recoverable reply identity");
    assert_eq!(reply.header.msgh_remote_port, FIXTURE_REPLY_PORT);
    assert_eq!(reply.header.msgh_id, bad_id + 100);
    assert_eq!(
        reply.header.msgh_size as usize,
        std::mem::size_of::<ExceptionReply>()
    );
    assert_eq!(reply.ret_code, KERN_FAILURE);
    assert_eq!(reply.ndr, FIX_NDR_RECORD);
}

#[test]
fn malformed_request_without_reply_right_is_not_replied_to() {
    let mut fixture = kernel_mig_fixture(&[FIXTURE_CODE, FIXTURE_SUBCODE]);
    put_u32(&mut fixture, 8, MACH_PORT_NULL);
    assert_eq!(
        parse_exception_message(&fixture),
        Err(ExceptionMessageError::UnsafeReplyIdentity)
    );
    assert!(failure_reply_for_message(&fixture).is_none());

    put_u32(&mut fixture, 8, FIXTURE_REPLY_PORT);
    put_u32(&mut fixture, 0, MACH_MSGH_BITS_COMPLEX);
    assert_eq!(
        parse_exception_message(&fixture),
        Err(ExceptionMessageError::UnsafeReplyIdentity)
    );
    assert!(failure_reply_for_message(&fixture).is_none());
}

#[test]
fn message_header_returns_owned_value_from_unaligned_bytes() {
    let fixture = kernel_mig_fixture(&[FIXTURE_CODE, FIXTURE_SUBCODE]);
    let mut storage = vec![0];
    storage.extend_from_slice(&fixture);

    let header = message_header(&storage[1..]).expect("copy wire header");
    assert_eq!(header.msgh_size as usize, fixture.len());
    assert_eq!(header.msgh_remote_port, FIXTURE_REPLY_PORT);
    assert_eq!(header.msgh_id, FIX_MESSAGE_ID);
}

#[test]
fn arbitrary_mach_message_bytes_never_panic() {
    let mut state = 0x9e37_79b9_u32;
    for len in 0..512 {
        let mut bytes = vec![0_u8; len];
        for byte in &mut bytes {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *byte = (state >> 24) as u8;
        }
        let _ = parse_exception_message(&bytes);
        let _ = failure_reply_for_message(&bytes);
    }
}
