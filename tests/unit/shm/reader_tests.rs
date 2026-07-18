use super::*;

use std::mem::size_of;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Generate a unique fake PID per test to avoid shm name collisions.
fn unique_pid() -> u32 {
    static COUNTER: AtomicU32 = AtomicU32::new(900_000);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Helper: write a value of type T at the given byte pointer offset.
unsafe fn write_val<T: Copy>(base: *mut u8, offset: usize, val: T) {
    unsafe {
        let src = (&raw const val).cast::<u8>();
        std::ptr::copy_nonoverlapping(src, base.add(offset), size_of::<T>());
    }
}

unsafe fn store_u32_release(base: *mut u8, offset: usize, val: u32) {
    #[allow(clippy::cast_ptr_alignment)]
    let value = unsafe { &*base.add(offset).cast::<AtomicU32>() };
    value.store(val, Ordering::Release);
}

unsafe fn store_u64_release(base: *mut u8, offset: usize, val: u64) {
    #[allow(clippy::cast_ptr_alignment)]
    let value = unsafe { &*base.add(offset).cast::<AtomicU64>() };
    value.store(val, Ordering::Release);
}

/// Helper: write a NUL-terminated string into the shm buffer at `offset`.
unsafe fn write_cstr(base: *mut u8, offset: usize, s: &str) {
    let bytes = s.as_bytes();
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), base.add(offset), bytes.len());
        *base.add(offset + bytes.len()) = 0;
    }
}

unsafe fn write_bytes(base: *mut u8, offset: usize, bytes: &[u8]) {
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), base.add(offset), bytes.len());
    }
}

struct BreadcrumbWireFixture<'a> {
    timestamp_ns: u64,
    category: u16,
    severity: u16,
    file: &'a [u8],
    message: &'a [u8],
}

unsafe fn write_breadcrumb_wire(
    base: *mut u8,
    offset: usize,
    timestamp_ns: u64,
    category: u16,
    severity: u16,
    file: &[u8],
    message: &[u8],
) {
    assert!(file.len() <= BREADCRUMB_FILE_LEN);
    assert!(message.len() <= BREADCRUMB_MESSAGE_LEN);
    unsafe {
        write_val::<u64>(
            base,
            offset + std::mem::offset_of!(SutBreadcrumb, timestamp_ns),
            timestamp_ns,
        );
        write_val::<u16>(
            base,
            offset + std::mem::offset_of!(SutBreadcrumb, category),
            category,
        );
        write_val::<u16>(
            base,
            offset + std::mem::offset_of!(SutBreadcrumb, severity),
            severity,
        );
        write_bytes(
            base,
            offset + std::mem::offset_of!(SutBreadcrumb, file),
            file,
        );
        write_bytes(
            base,
            offset + std::mem::offset_of!(SutBreadcrumb, message),
            message,
        );
    }
}

fn snapshot(shm: &SharedMemory) -> OwnedShmSnapshot {
    shm.snapshot_owned_until(None).expect("shm snapshot")
}

#[test]
fn test_shm_create_snapshot_drop() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    let snapshot = snapshot(&shm);
    assert!(shm.name().starts_with("/cm_"));
    assert_eq!(shm.read_live_heartbeat(), 0);
    assert!(snapshot.read_breadcrumbs().is_empty());
    // Drop cleans up
}

#[test]
fn test_snapshot_policy_omits_sensitive_sections_and_full_policy_preserves_them() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    let attachment_label = b"private-log\0";
    let attachment_path = b"/private/user/secret.log\0";
    let screenshot_marker = [0x11, 0x22, 0x33, 0x44];
    unsafe {
        let base = shm.base_ptr();
        store_u32_release(base, ATTACHMENTS_GENERATION_OFFSET, 2);
        write_val::<u32>(
            base,
            ATTACHMENT_OFFSET + std::mem::offset_of!(ShmAttachmentSection, count),
            1,
        );
        let first_attachment =
            ATTACHMENT_OFFSET + std::mem::offset_of!(ShmAttachmentSection, slots);
        write_bytes(
            base,
            first_attachment + std::mem::offset_of!(ShmAttachmentSlot, label),
            attachment_label,
        );
        write_bytes(
            base,
            first_attachment + std::mem::offset_of!(ShmAttachmentSlot, path),
            attachment_path,
        );

        store_u32_release(base, SCREENSHOT_GENERATIONS_OFFSET, 2);
        write_val::<u64>(base, SCREENSHOT_TIMESTAMPS_OFFSET, 99);
        write_bytes(base, SCREENSHOT_DATA_OFFSET, &screenshot_marker);
    }

    let minimal = shm
        .snapshot_owned_until_with_policy(
            None,
            ShmSnapshotPolicy {
                breadcrumbs: true,
                context: true,
                attachments: false,
                screenshots: false,
            },
        )
        .expect("minimal snapshot");
    assert!(minimal.read_attachments().is_empty());
    assert!(minimal.read_screenshots().is_empty());
    assert!(
        minimal.bytes[ATTACHMENT_OFFSET..SECTION4_OFFSET]
            .iter()
            .all(|byte| *byte == 0)
    );
    assert!(
        minimal.bytes[SECTION4_OFFSET..FOOTER_OFFSET]
            .iter()
            .all(|byte| *byte == 0)
    );

    let full = shm
        .snapshot_owned_until_with_policy(None, ShmSnapshotPolicy::ALL)
        .expect("full snapshot");
    let attachments = full.read_attachments();
    assert_eq!(attachments.len(), 1);
    assert_eq!(attachments[0].label, "private-log");
    assert_eq!(attachments[0].path, "/private/user/secret.log");
    let screenshots = full.read_screenshots();
    assert_eq!(screenshots.len(), 1);
    assert_eq!(
        &screenshots[0].rgba[..screenshot_marker.len()],
        &screenshot_marker
    );
}

#[test]
fn test_section_offsets_are_consistent() {
    // Ensure sections don't overlap and footer is at the end (compile-time checks)
    const { assert!(SECTION2_OFFSET > SECTION1_OFFSET) };
    const { assert!(SECTION3_OFFSET > SECTION2_OFFSET) };
    const { assert!(SECTION4_OFFSET > SECTION3_OFFSET) };
    const { assert!(FOOTER_OFFSET > SECTION4_OFFSET) };
    assert_eq!(SHM_TOTAL_SIZE, FOOTER_OFFSET + 4);
}

#[test]
fn test_validate_corrupted_magic() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    // Corrupt magic
    unsafe {
        write_val::<u32>(shm.base_ptr(), 0, 0xDEAD_DEAD);
    }
    assert_eq!(
        shm.snapshot_owned_until(None).unwrap_err(),
        ShmSnapshotError::Validation(ShmValidationError::InvalidMagic { found: 0xDEAD_DEAD })
    );
}

#[test]
fn test_validate_rejects_legacy_schema_version() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    let legacy_version = SHM_VERSION
        .checked_sub(1)
        .expect("the current schema must have a legacy predecessor");
    unsafe {
        write_val::<u32>(
            shm.base_ptr(),
            SECTION1_OFFSET + std::mem::offset_of!(ShmHeader, version),
            legacy_version,
        );
    }
    assert_eq!(
        shm.snapshot_owned_until(None).unwrap_err(),
        ShmSnapshotError::Validation(ShmValidationError::UnsupportedVersion {
            found: legacy_version
        })
    );
}

#[test]
fn test_validate_corrupted_canary() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    // Corrupt canary
    unsafe {
        write_val::<u32>(shm.base_ptr(), FOOTER_OFFSET, 0x0000_0000);
    }
    assert_eq!(
        shm.snapshot_owned_until(None).unwrap_err(),
        ShmSnapshotError::Validation(ShmValidationError::InvalidCanary { found: 0 })
    );
}

#[test]
fn test_read_breadcrumbs_empty_rings() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    let crumbs = snapshot(&shm).read_breadcrumbs();
    assert!(crumbs.is_empty());
}

#[test]
fn test_read_context_fresh_shm() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    let ctx = snapshot(&shm).read_context();
    assert!(ctx.is_some());
    let ctx = ctx.unwrap();
    assert_eq!(ctx.heartbeat_counter, 0);
    assert!(ctx.annotations.is_empty());
    assert!(ctx.session_id.is_empty());
}

#[test]
fn test_snapshot_rejects_elapsed_deadline() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    assert_eq!(
        shm.snapshot_owned_until(Some(Instant::now())).unwrap_err(),
        ShmSnapshotError::DeadlineExceeded
    );
}

#[test]
fn test_snapshot_rejects_short_mapping_without_pointer_arithmetic() {
    let mut shm = SharedMemory::create(unique_pid()).expect("shm create");
    let actual_size = shm.mapping.len();
    shm.set_mapped_size_for_test(FOOTER_OFFSET);
    assert_eq!(
        shm.snapshot_owned_until(None).unwrap_err(),
        ShmSnapshotError::Validation(ShmValidationError::MappingTooSmall {
            mapped: FOOTER_OFFSET,
            required: SHM_TOTAL_SIZE,
        })
    );
    // Restore the true mmap length before Drop calls munmap.
    shm.set_mapped_size_for_test(actual_size);
}

#[test]
fn test_read_live_heartbeat_write_read() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    assert_eq!(shm.read_live_heartbeat(), 0);
    // Write a known heartbeat value at the correct offset
    unsafe {
        write_val::<u64>(
            shm.base_ptr(),
            CONTEXT_OFFSET + std::mem::offset_of!(SutCrashContext, heartbeat_counter),
            42,
        );
    }
    assert_eq!(shm.read_live_heartbeat(), 42);
}

#[test]
fn test_live_anr_heartbeat_requires_exact_ready_publication() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    let ready = SECTION1_OFFSET + std::mem::offset_of!(ShmHeader, producer_ready);
    let heartbeat = CONTEXT_OFFSET + std::mem::offset_of!(SutCrashContext, heartbeat_counter);

    assert_eq!(shm.read_live_anr_heartbeat(), None);
    unsafe {
        store_u64_release(shm.base_ptr(), heartbeat, 42);
        store_u32_release(shm.base_ptr(), ready, 2);
    }
    assert_eq!(
        shm.read_live_anr_heartbeat(),
        None,
        "malformed readiness must fail closed"
    );

    unsafe {
        // The first published heartbeat may legitimately be zero. The ready
        // release is the handshake that distinguishes it from zero-filled SHM.
        store_u64_release(shm.base_ptr(), heartbeat, 0);
        store_u32_release(shm.base_ptr(), ready, SHM_PRODUCER_READY);
    }
    assert_eq!(shm.read_live_anr_heartbeat(), Some(0));

    unsafe {
        store_u64_release(shm.base_ptr(), heartbeat, 43);
    }
    assert_eq!(shm.read_live_anr_heartbeat(), Some(43));
}

// ── owned byte parser tests ──

#[test]
fn test_bounded_c_string_decoder_enforces_wire_contract() {
    assert_eq!(decode_bounded_c_string(b"\0"), Some(String::new()));
    assert_eq!(
        decode_bounded_c_string("안전\0ignored".as_bytes()),
        Some("안전".to_owned())
    );
    assert_eq!(
        decode_bounded_c_string(b"valid\0\xff\n"),
        Some("valid".to_owned())
    );

    assert_eq!(decode_bounded_c_string(b"missing terminator"), None);
    assert_eq!(decode_bounded_c_string(b"invalid \xff\0"), None);
    assert_eq!(decode_bounded_c_string(b"line\nfeed\0"), None);
    assert_eq!(decode_bounded_c_string(b"c1 \xc2\x85 control\0"), None);
}

#[test]
fn test_read_context_decodes_primitive_bytes_without_typed_materialization() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    let annotations = CONTEXT_OFFSET + std::mem::offset_of!(SutCrashContext, annotations);
    unsafe {
        let base = shm.base_ptr();
        write_val::<u64>(
            base,
            CONTEXT_OFFSET + std::mem::offset_of!(SutCrashContext, heartbeat_counter),
            999,
        );
        write_val::<u64>(
            base,
            CONTEXT_OFFSET + std::mem::offset_of!(SutCrashContext, session_start_ns),
            123_456_789,
        );
        write_val::<u32>(
            base,
            CONTEXT_OFFSET + std::mem::offset_of!(SutCrashContext, build_number),
            7,
        );
        // A non-canonical C bool byte must be decoded as a byte, never as Rust bool.
        write_val::<u8>(
            base,
            CONTEXT_OFFSET + std::mem::offset_of!(SutCrashContext, git_dirty),
            2,
        );
        write_val::<i32>(
            base,
            CONTEXT_OFFSET + std::mem::offset_of!(SutCrashContext, annotation_count),
            2,
        );
        write_cstr(
            base,
            annotations + std::mem::offset_of!(SutCrashAnnotation, key),
            "active_tool",
        );
        write_cstr(
            base,
            annotations + std::mem::offset_of!(SutCrashAnnotation, value),
            "brush",
        );
        let second = annotations + size_of::<SutCrashAnnotation>();
        write_cstr(
            base,
            second + std::mem::offset_of!(SutCrashAnnotation, key),
            "voxel_count",
        );
        write_cstr(
            base,
            second + std::mem::offset_of!(SutCrashAnnotation, value),
            "1024",
        );
    }

    let context = snapshot(&shm).read_context().expect("context");
    assert_eq!(context.heartbeat_counter, 999);
    assert_eq!(context.session_start_ns, 123_456_789);
    assert_eq!(context.build_number, 7);
    assert!(context.git_dirty);
    assert_eq!(
        context.annotations,
        vec![
            ("active_tool".to_string(), "brush".to_string()),
            ("voxel_count".to_string(), "1024".to_string()),
        ]
    );
}

#[test]
fn test_read_context_decodes_all_git_dirty_wire_values_as_u8() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    let offset = CONTEXT_OFFSET + std::mem::offset_of!(SutCrashContext, git_dirty);

    for (wire_value, expected) in [(0, false), (1, true), (2, true), (255, true)] {
        unsafe {
            write_val::<u8>(shm.base_ptr(), offset, wire_value);
        }
        assert_eq!(
            snapshot(&shm).read_context().expect("context").git_dirty,
            expected,
            "git_dirty wire byte {wire_value}"
        );
    }
}

#[test]
fn test_read_context_rejects_out_of_range_annotation_counts() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    let offset = CONTEXT_OFFSET + std::mem::offset_of!(SutCrashContext, annotation_count);
    for count in [-1, i32::try_from(MAX_ANNOTATIONS).unwrap() + 1] {
        unsafe {
            write_val::<i32>(shm.base_ptr(), offset, count);
        }
        assert!(
            snapshot(&shm).read_context().is_none(),
            "annotation count {count}"
        );
    }
}

#[test]
fn test_read_context_accepts_annotation_count_boundaries() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    let offset = CONTEXT_OFFSET + std::mem::offset_of!(SutCrashContext, annotation_count);
    for (count, expected_len) in [(0, 0), (MAX_ANNOTATIONS, MAX_ANNOTATIONS)] {
        unsafe {
            write_val::<i32>(shm.base_ptr(), offset, i32::try_from(count).unwrap());
        }
        assert_eq!(
            snapshot(&shm)
                .read_context()
                .expect("boundary count must be accepted")
                .annotations
                .len(),
            expected_len
        );
    }
}

#[test]
fn test_read_context_string_fields_from_owned_bytes() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    unsafe {
        let base = shm.base_ptr();
        for (offset, value) in [
            (
                std::mem::offset_of!(SutCrashContext, session_id),
                "sess-abc",
            ),
            (std::mem::offset_of!(SutCrashContext, app_version), "1.2.3"),
            (std::mem::offset_of!(SutCrashContext, git_hash), "deadbeef"),
            (std::mem::offset_of!(SutCrashContext, build_type), "release"),
            (
                std::mem::offset_of!(SutCrashContext, build_preset),
                "default",
            ),
            (
                std::mem::offset_of!(SutCrashContext, build_timestamp),
                "2026-07-17",
            ),
            (std::mem::offset_of!(SutCrashContext, compiler), "clang-17"),
            (
                std::mem::offset_of!(SutCrashContext, os_version),
                "macOS 15.3",
            ),
        ] {
            write_cstr(base, CONTEXT_OFFSET + offset, value);
        }
    }

    let context = snapshot(&shm).read_context().expect("context");
    assert_eq!(context.session_id, "sess-abc");
    assert_eq!(context.app_version, "1.2.3");
    assert_eq!(context.git_hash, "deadbeef");
    assert_eq!(context.build_type, "release");
    assert_eq!(context.build_preset, "default");
    assert_eq!(context.build_timestamp, "2026-07-17");
    assert_eq!(context.compiler, "clang-17");
    assert_eq!(context.os_version, "macOS 15.3");
}

#[test]
fn test_malformed_context_string_rejects_context_and_preserves_raw_bytes() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    let session_id = std::mem::offset_of!(SutCrashContext, session_id);
    unsafe {
        write_bytes(
            shm.base_ptr(),
            CONTEXT_OFFSET + session_id,
            &[b'X'; CONTEXT_SESSION_ID_LEN],
        );
    }

    let owned = snapshot(&shm);
    assert!(owned.read_context().is_none());
    assert_eq!(
        &owned.raw_context_bytes_owned()[session_id..session_id + CONTEXT_SESSION_ID_LEN],
        &[b'X'; CONTEXT_SESSION_ID_LEN]
    );
}

#[test]
fn test_malformed_context_annotation_rejects_complete_context() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    let annotations = std::mem::offset_of!(SutCrashContext, annotations);
    let key = annotations + std::mem::offset_of!(SutCrashAnnotation, key);
    unsafe {
        write_val::<i32>(
            shm.base_ptr(),
            CONTEXT_OFFSET + std::mem::offset_of!(SutCrashContext, annotation_count),
            1,
        );
        write_bytes(
            shm.base_ptr(),
            CONTEXT_OFFSET + key,
            &[b'K'; ANNOTATION_KEY_LEN],
        );
    }

    let owned = snapshot(&shm);
    assert!(owned.read_context().is_none());
    assert_eq!(owned.raw_context_bytes_owned()[key], b'K');
}

// ── read_settings / read_attachments / read_screenshots / read_breadcrumbs with data ──

#[test]
fn test_read_settings_populated() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    unsafe {
        let base = shm.base_ptr();
        write_val::<u32>(
            base,
            SETTINGS_OFFSET + std::mem::offset_of!(SutCrashSettingsSnapshot, entry_count),
            1,
        );
        let entry = SETTINGS_OFFSET + std::mem::offset_of!(SutCrashSettingsSnapshot, entries);
        write_cstr(base, entry, "mode");
        write_cstr(base, entry + EXTENSION_KEY_LEN, "diagnostic");
    }

    let s = snapshot(&shm)
        .read_settings()
        .expect("settings should read");
    assert_eq!(s.schema_version, 1);
    assert_eq!(s.values, [("mode".into(), "diagnostic".into())]);
}

#[test]
fn test_malformed_extension_key_rejects_extension_and_preserves_context() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    let entries = std::mem::offset_of!(SutCrashSettingsSnapshot, entries);
    unsafe {
        write_val::<u32>(
            shm.base_ptr(),
            SETTINGS_OFFSET + std::mem::offset_of!(SutCrashSettingsSnapshot, entry_count),
            1,
        );
        write_bytes(
            shm.base_ptr(),
            SETTINGS_OFFSET + entries,
            &[b'E'; EXTENSION_KEY_LEN],
        );
        write_cstr(
            shm.base_ptr(),
            CONTEXT_OFFSET + std::mem::offset_of!(SutCrashContext, app_version),
            "stable",
        );
    }

    let owned = snapshot(&shm);
    assert!(owned.read_settings().is_none());
    assert_eq!(
        owned.read_context().expect("stable context").app_version,
        "stable"
    );
    let raw = owned.raw_context_bytes_owned();
    let raw_extra = size_of::<SutCrashContext>() + entries;
    assert_eq!(
        &raw[raw_extra..raw_extra + EXTENSION_KEY_LEN],
        &[b'E'; EXTENSION_KEY_LEN]
    );
}

#[test]
fn test_read_attachments_populated() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    unsafe {
        let base = shm.base_ptr();
        write_val::<u32>(
            base,
            ATTACHMENT_OFFSET + std::mem::offset_of!(ShmAttachmentSection, count),
            1,
        );
        let slot0 = ATTACHMENT_OFFSET + std::mem::offset_of!(ShmAttachmentSection, slots);
        write_cstr(
            base,
            slot0 + std::mem::offset_of!(ShmAttachmentSlot, label),
            "log",
        );
        write_cstr(
            base,
            slot0 + std::mem::offset_of!(ShmAttachmentSlot, path),
            "/tmp/app.log",
        );
    }

    let att = snapshot(&shm).read_attachments();
    assert_eq!(att.len(), 1);
    assert_eq!(att[0].label, "log");
    assert_eq!(att[0].path, "/tmp/app.log");
}

#[test]
fn test_read_attachments_skips_empty_path() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    // count=1 but slot path left empty → skipped.
    unsafe {
        write_val::<u32>(
            shm.base_ptr(),
            ATTACHMENT_OFFSET + std::mem::offset_of!(ShmAttachmentSection, count),
            1,
        );
    }
    assert!(snapshot(&shm).read_attachments().is_empty());
}

#[test]
fn test_malformed_attachment_slot_is_skipped_and_later_slot_survives() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    let slots = ATTACHMENT_OFFSET + std::mem::offset_of!(ShmAttachmentSection, slots);
    let first_path = slots + std::mem::offset_of!(ShmAttachmentSlot, path);
    let second = slots + size_of::<ShmAttachmentSlot>();
    unsafe {
        let base = shm.base_ptr();
        write_val::<u32>(
            base,
            ATTACHMENT_OFFSET + std::mem::offset_of!(ShmAttachmentSection, count),
            2,
        );
        write_cstr(
            base,
            slots + std::mem::offset_of!(ShmAttachmentSlot, label),
            "malformed",
        );
        write_bytes(base, first_path, &[b'P'; ATTACHMENT_PATH_LEN]);
        write_cstr(
            base,
            second + std::mem::offset_of!(ShmAttachmentSlot, label),
            "stable",
        );
        write_cstr(
            base,
            second + std::mem::offset_of!(ShmAttachmentSlot, path),
            "/tmp/stable.log",
        );
    }

    let owned = snapshot(&shm);
    let attachments = owned.read_attachments();
    assert_eq!(attachments.len(), 1);
    assert_eq!(attachments[0].label, "stable");
    assert_eq!(attachments[0].path, "/tmp/stable.log");
    assert_eq!(owned.bytes[first_path], b'P');
}

#[test]
fn test_attachment_count_accepts_max_and_rejects_above_max() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    let count = ATTACHMENT_OFFSET + std::mem::offset_of!(ShmAttachmentSection, count);
    let last_slot = ATTACHMENT_OFFSET
        + std::mem::offset_of!(ShmAttachmentSection, slots)
        + (MAX_ATTACHMENTS - 1) * size_of::<ShmAttachmentSlot>();
    unsafe {
        let base = shm.base_ptr();
        write_val::<u32>(base, count, u32::try_from(MAX_ATTACHMENTS).unwrap());
        write_cstr(
            base,
            last_slot + std::mem::offset_of!(ShmAttachmentSlot, label),
            "last",
        );
        write_cstr(
            base,
            last_slot + std::mem::offset_of!(ShmAttachmentSlot, path),
            "/tmp/last.log",
        );
    }

    let accepted = snapshot(&shm).read_attachments();
    assert_eq!(accepted.len(), 1);
    assert_eq!(accepted[0].path, "/tmp/last.log");

    unsafe {
        write_val::<u32>(
            shm.base_ptr(),
            count,
            u32::try_from(MAX_ATTACHMENTS).unwrap() + 1,
        );
    }
    assert!(snapshot(&shm).read_attachments().is_empty());
}

#[test]
fn test_read_screenshots_one_valid_slot() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    unsafe {
        let base = shm.base_ptr();
        write_val::<u32>(
            base,
            SECTION4_OFFSET + std::mem::offset_of!(SutScreenshotSection, valid),
            2,
        );
        write_val::<u64>(
            base,
            SECTION4_OFFSET + std::mem::offset_of!(SutScreenshotSection, timestamp),
            777_000,
        );
        write_val::<u32>(
            base,
            SECTION4_OFFSET + std::mem::offset_of!(SutScreenshotSection, tier),
            3,
        );
        *base.add(SECTION4_OFFSET + std::mem::offset_of!(SutScreenshotSection, data)) = 0xA5;
    }

    let owned = snapshot(&shm);
    // Mutating mmap after capture cannot affect the immutable event snapshot.
    unsafe {
        *shm.base_ptr()
            .add(SECTION4_OFFSET + std::mem::offset_of!(SutScreenshotSection, data)) = 0x5A;
    }
    let shots = owned.read_screenshots();
    assert_eq!(shots.len(), 1);
    assert_eq!(shots[0].timestamp_ns, 777_000);
    assert_eq!(shots[0].tier, 3);
    assert_eq!(shots[0].width, SCREENSHOT_WIDTH);
    assert_eq!(shots[0].height, SCREENSHOT_HEIGHT);
    assert_eq!(shots[0].rgba.len(), SCREENSHOT_BYTES_PER_SLOT);
    assert_eq!(shots[0].rgba[0], 0xA5);
}

#[test]
fn test_screenshot_selection_prioritizes_lower_tier_then_newest_timestamp() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    unsafe {
        let base = shm.base_ptr();
        let valid = SECTION4_OFFSET + std::mem::offset_of!(SutScreenshotSection, valid);
        let timestamp = SECTION4_OFFSET + std::mem::offset_of!(SutScreenshotSection, timestamp);
        let tier = SECTION4_OFFSET + std::mem::offset_of!(SutScreenshotSection, tier);
        for (index, timestamp_ns, capture_tier) in [(0, 300_u64, 2_u32), (1, 100, 1), (2, 200, 1)] {
            write_val::<u32>(base, valid + index * size_of::<u32>(), 2);
            write_val::<u64>(base, timestamp + index * size_of::<u64>(), timestamp_ns);
            write_val::<u32>(base, tier + index * size_of::<u32>(), capture_tier);
        }
    }

    let shots = snapshot(&shm).read_screenshots();
    let selected = shots
        .iter()
        .map(|shot| (shot.tier, shot.timestamp_ns))
        .collect::<Vec<_>>();
    assert_eq!(selected, vec![(1, 200), (1, 100), (2, 300)]);
}

#[test]
fn test_mapping_shape_reports_size_before_alignment_and_distinguishes_alignment() {
    assert_eq!(
        validate_mapping_shape(1, SHM_TOTAL_SIZE - 1),
        Err(ShmValidationError::MappingTooSmall {
            mapped: SHM_TOTAL_SIZE - 1,
            required: SHM_TOTAL_SIZE,
        })
    );
    assert_eq!(
        validate_mapping_shape(1, SHM_TOTAL_SIZE),
        Err(ShmValidationError::MisalignedMapping {
            address: 1,
            required: align_of::<SutShmRegion>(),
        })
    );
}

#[test]
fn screenshot_budget_prefers_lower_tier_then_newer_timestamp() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    let valid_base = SECTION4_OFFSET + std::mem::offset_of!(SutScreenshotSection, valid);
    let timestamp_base = SECTION4_OFFSET + std::mem::offset_of!(SutScreenshotSection, timestamp);
    let tier_base = SECTION4_OFFSET + std::mem::offset_of!(SutScreenshotSection, tier);
    let data_base = SECTION4_OFFSET + std::mem::offset_of!(SutScreenshotSection, data);
    for (index, timestamp, tier, marker) in [
        (0usize, 300u64, 9u32, 0x90u8),
        (1, 100, 1, 0x11),
        (2, 200, 1, 0x12),
    ] {
        unsafe {
            write_val::<u32>(shm.base_ptr(), valid_base + index * size_of::<u32>(), 2);
            write_val::<u64>(
                shm.base_ptr(),
                timestamp_base + index * size_of::<u64>(),
                timestamp,
            );
            write_val::<u32>(shm.base_ptr(), tier_base + index * size_of::<u32>(), tier);
            *shm.base_ptr()
                .add(data_base + index * SCREENSHOT_BYTES_PER_SLOT) = marker;
        }
    }

    let outcome =
        snapshot(&shm).read_screenshots_bounded(2, SCREENSHOT_BYTES_PER_SLOT * 2, || true);
    assert_eq!(
        outcome.budget_exhaustion,
        Some(ScreenshotBudgetExhaustion::FrameLimit)
    );
    assert!(outcome.unreadable_slots.is_empty());
    assert_eq!(outcome.screenshots.len(), 2);
    assert_eq!(
        (
            outcome.screenshots[0].tier,
            outcome.screenshots[0].timestamp_ns,
            outcome.screenshots[0].rgba[0]
        ),
        (1, 200, 0x12)
    );
    assert_eq!(
        (
            outcome.screenshots[1].tier,
            outcome.screenshots[1].timestamp_ns,
            outcome.screenshots[1].rgba[0]
        ),
        (1, 100, 0x11)
    );
}

#[test]
fn variable_length_snapshot_reports_incomplete_slot_separately_from_budget() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    let valid_base = SECTION4_OFFSET + std::mem::offset_of!(SutScreenshotSection, valid);
    let timestamp_base = SECTION4_OFFSET + std::mem::offset_of!(SutScreenshotSection, timestamp);
    let tier_base = SECTION4_OFFSET + std::mem::offset_of!(SutScreenshotSection, tier);
    let data_base = SECTION4_OFFSET + std::mem::offset_of!(SutScreenshotSection, data);
    unsafe {
        for (index, timestamp, marker) in [(0usize, 100u64, 0x11u8), (1, 200, 0x22)] {
            write_val::<u32>(shm.base_ptr(), valid_base + index * size_of::<u32>(), 2);
            write_val::<u64>(
                shm.base_ptr(),
                timestamp_base + index * size_of::<u64>(),
                timestamp,
            );
            write_val::<u32>(shm.base_ptr(), tier_base + index * size_of::<u32>(), 1);
            *shm.base_ptr()
                .add(data_base + index * SCREENSHOT_BYTES_PER_SLOT) = marker;
        }
    }

    let full = snapshot(&shm);
    let payload_end = data_base + SCREENSHOT_BYTES_PER_SLOT + SCREENSHOT_BYTES_PER_SLOT / 2;
    let mut variable_bytes = full.bytes[..payload_end].to_vec();
    variable_bytes.extend_from_slice(&SHM_CANARY.to_ne_bytes());
    let variable = OwnedShmSnapshot::from_owned_bytes(variable_bytes, Vec::new()).unwrap();
    let outcome = variable.read_screenshots_bounded(8, usize::MAX, || true);

    assert_eq!(outcome.budget_exhaustion, None);
    assert_eq!(outcome.unreadable_slots, vec![1]);
    assert_eq!(outcome.screenshots.len(), 1);
    assert_eq!(outcome.screenshots[0].timestamp_ns, 100);
    assert_eq!(outcome.screenshots[0].rgba[0], 0x11);
}

#[test]
fn test_read_screenshots_last_slot_stays_within_owned_snapshot() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    let index = SCREENSHOT_SLOTS as usize - 1;
    let valid_offset = SECTION4_OFFSET
        + std::mem::offset_of!(SutScreenshotSection, valid)
        + index * size_of::<u32>();
    let timestamp_offset = SECTION4_OFFSET
        + std::mem::offset_of!(SutScreenshotSection, timestamp)
        + index * size_of::<u64>();
    let data_offset = SECTION4_OFFSET
        + std::mem::offset_of!(SutScreenshotSection, data)
        + index * SCREENSHOT_BYTES_PER_SLOT;
    assert_eq!(data_offset + SCREENSHOT_BYTES_PER_SLOT, FOOTER_OFFSET);
    unsafe {
        write_val::<u32>(shm.base_ptr(), valid_offset, 2);
        write_val::<u64>(shm.base_ptr(), timestamp_offset, 999_000);
        *shm.base_ptr()
            .add(data_offset + SCREENSHOT_BYTES_PER_SLOT - 1) = 0xEF;
    }

    let shots = snapshot(&shm).read_screenshots();
    assert_eq!(shots.len(), 1);
    assert_eq!(shots[0].timestamp_ns, 999_000);
    assert_eq!(shots[0].rgba[SCREENSHOT_BYTES_PER_SLOT - 1], 0xEF);
}

#[test]
fn test_read_breadcrumbs_with_entry() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    let ring0 = SECTION2_OFFSET + std::mem::offset_of!(SutCrumbState, rings);
    let buf0 = ring0 + std::mem::offset_of!(SutCrumbRing, buf);
    unsafe {
        let base = shm.base_ptr();
        // One crumb in ring 0, slot 0.
        write_val::<u64>(
            base,
            buf0 + std::mem::offset_of!(SutBreadcrumb, timestamp_ns),
            12_345,
        );
        write_val::<u32>(
            base,
            buf0 + std::mem::offset_of!(SutBreadcrumb, thread_id),
            9,
        );
        write_val::<u16>(
            base,
            buf0 + std::mem::offset_of!(SutBreadcrumb, category),
            5,
        );
        write_val::<u16>(
            base,
            buf0 + std::mem::offset_of!(SutBreadcrumb, severity),
            2,
        );
        write_val::<u16>(base, buf0 + std::mem::offset_of!(SutBreadcrumb, line), 77);
        write_cstr(
            base,
            buf0 + std::mem::offset_of!(SutBreadcrumb, file),
            "app.c",
        );
        write_cstr(
            base,
            buf0 + std::mem::offset_of!(SutBreadcrumb, message),
            "boom",
        );
        // Ring bookkeeping: one entry written, write_idx just past it.
        write_val::<u32>(base, ring0 + std::mem::offset_of!(SutCrumbRing, count), 1);
        write_val::<u32>(
            base,
            ring0 + std::mem::offset_of!(SutCrumbRing, write_idx),
            1,
        );
        // One active ring.
        write_val::<u32>(
            base,
            SECTION2_OFFSET + std::mem::offset_of!(SutCrumbState, ring_count),
            1,
        );
    }

    let owned = snapshot(&shm);
    unsafe {
        write_val::<u64>(
            shm.base_ptr(),
            buf0 + std::mem::offset_of!(SutBreadcrumb, timestamp_ns),
            98_765,
        );
    }
    let crumbs = owned.read_breadcrumbs();
    assert_eq!(crumbs.len(), 1);
    let c = &crumbs[0];
    assert_eq!(c.timestamp_ns, 12_345);
    assert_eq!(c.thread_id, 9);
    assert_eq!(c.category, 5);
    assert_eq!(c.severity, 2);
    assert_eq!(c.line, 77);
    assert_eq!(c.file, "app.c");
    assert_eq!(c.message, "boom");
}

#[test]
fn test_breadcrumb_ring_count_accepts_max_and_rejects_above_max() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    let last_ring = SECTION2_OFFSET
        + std::mem::offset_of!(SutCrumbState, rings)
        + (CRUMB_MAX_THREADS - 1) * size_of::<SutCrumbRing>();
    let entry = last_ring + std::mem::offset_of!(SutCrumbRing, buf);
    let ring_count = SECTION2_OFFSET + std::mem::offset_of!(SutCrumbState, ring_count);
    unsafe {
        let base = shm.base_ptr();
        write_breadcrumb_wire(
            base,
            entry,
            777,
            CRUMB_CATEGORY_MAX,
            CRUMB_SEVERITY_MAX,
            b"last.c\0",
            b"last ring\0",
        );
        write_val::<u32>(
            base,
            last_ring + std::mem::offset_of!(SutCrumbRing, count),
            1,
        );
        write_val::<u32>(
            base,
            last_ring + std::mem::offset_of!(SutCrumbRing, write_idx),
            1,
        );
        store_u32_release(base, ring_count, u32::try_from(CRUMB_MAX_THREADS).unwrap());
    }

    let accepted = snapshot(&shm).read_breadcrumbs();
    assert_eq!(accepted.len(), 1);
    assert_eq!(accepted[0].timestamp_ns, 777);

    unsafe {
        store_u32_release(
            shm.base_ptr(),
            ring_count,
            u32::try_from(CRUMB_MAX_THREADS).unwrap() + 1,
        );
    }
    assert!(snapshot(&shm).read_breadcrumbs().is_empty());
}

#[test]
fn test_invalid_breadcrumb_values_drop_only_the_malformed_entries() {
    assert_eq!(CRUMB_CATEGORY_MAX, 10);
    assert_eq!(CRUMB_SEVERITY_INFO, 0);
    assert_eq!(CRUMB_SEVERITY_WARN, 1);
    assert_eq!(CRUMB_SEVERITY_ERROR, 2);
    assert_eq!(CRUMB_SEVERITY_MAX, CRUMB_SEVERITY_ERROR);

    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    let ring0 = SECTION2_OFFSET + std::mem::offset_of!(SutCrumbState, rings);
    let buffer = ring0 + std::mem::offset_of!(SutCrumbRing, buf);
    let entries = [
        BreadcrumbWireFixture {
            timestamp_ns: 10,
            category: 0,
            severity: CRUMB_SEVERITY_INFO,
            file: b"first.c\0",
            message: b"first\0",
        },
        BreadcrumbWireFixture {
            timestamp_ns: 20,
            category: CRUMB_CATEGORY_MAX + 1,
            severity: CRUMB_SEVERITY_INFO,
            file: b"cat.c\0",
            message: b"bad category\0",
        },
        BreadcrumbWireFixture {
            timestamp_ns: 30,
            category: CRUMB_CATEGORY_MAX,
            severity: CRUMB_SEVERITY_MAX,
            file: b"stable.c\0",
            message: b"stable\0",
        },
        BreadcrumbWireFixture {
            timestamp_ns: 40,
            category: 0,
            severity: CRUMB_SEVERITY_MAX + 1,
            file: b"sev.c\0",
            message: b"bad severity\0",
        },
        BreadcrumbWireFixture {
            timestamp_ns: 50,
            category: 0,
            severity: CRUMB_SEVERITY_INFO,
            file: b"\xff\0",
            message: b"bad file\0",
        },
        BreadcrumbWireFixture {
            timestamp_ns: 60,
            category: 0,
            severity: CRUMB_SEVERITY_INFO,
            file: b"msg.c\0",
            message: &[b'M'; BREADCRUMB_MESSAGE_LEN],
        },
    ];
    unsafe {
        let base = shm.base_ptr();
        for (index, entry) in entries.iter().enumerate() {
            write_breadcrumb_wire(
                base,
                buffer + index * size_of::<SutBreadcrumb>(),
                entry.timestamp_ns,
                entry.category,
                entry.severity,
                entry.file,
                entry.message,
            );
        }
        let entry_count = u32::try_from(entries.len()).expect("fixture count fits u32");
        write_val::<u32>(
            base,
            ring0 + std::mem::offset_of!(SutCrumbRing, count),
            entry_count,
        );
        write_val::<u32>(
            base,
            ring0 + std::mem::offset_of!(SutCrumbRing, write_idx),
            entry_count,
        );
        write_val::<u32>(
            base,
            SECTION2_OFFSET + std::mem::offset_of!(SutCrumbState, ring_count),
            1,
        );
    }

    let owned = snapshot(&shm);
    let breadcrumbs = owned.read_breadcrumbs();
    assert_eq!(
        breadcrumbs
            .iter()
            .map(|breadcrumb| breadcrumb.timestamp_ns)
            .collect::<Vec<_>>(),
        vec![10, 30]
    );
    assert_eq!(breadcrumbs[1].file, "stable.c");
    assert_eq!(breadcrumbs[1].message, "stable");

    let malformed_file = buffer - SECTION2_OFFSET
        + 4 * size_of::<SutBreadcrumb>()
        + std::mem::offset_of!(SutBreadcrumb, file);
    assert_eq!(owned.raw_breadcrumb_bytes_owned()[malformed_file], 0xff);
}

#[test]
fn test_odd_context_is_dropped_without_retry_and_stable_settings_survive() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    let context_generation = SECTION1_OFFSET + std::mem::offset_of!(ShmHeader, context_generation);
    let heartbeat_offset =
        CONTEXT_OFFSET + std::mem::offset_of!(SutCrashContext, heartbeat_counter);
    unsafe {
        let base = shm.base_ptr();
        store_u32_release(base, context_generation, 1);
        store_u64_release(base, heartbeat_offset, 321);
        write_cstr(
            base,
            CONTEXT_OFFSET + std::mem::offset_of!(SutCrashContext, app_version),
            "torn",
        );
        write_val::<u32>(
            base,
            SETTINGS_OFFSET + std::mem::offset_of!(SutCrashSettingsSnapshot, entry_count),
            0,
        );
    }

    let owned = shm
        .snapshot_owned_until(Some(Instant::now() + Duration::from_secs(10)))
        .expect("odd generation must be rejected without spinning");

    assert_eq!(
        owned.consistency_issues(),
        &[ShmConsistencyIssue::Context {
            generation_before: 1,
            generation_after: 1,
        }]
    );
    assert!(owned.read_context().is_none());
    assert!(
        owned
            .read_settings()
            .expect("stable extension")
            .values
            .is_empty()
    );

    let raw = owned.raw_context_bytes_owned();
    assert_eq!(u64::from_ne_bytes(raw[..8].try_into().unwrap()), 321);
    assert!(
        raw[8..size_of::<SutCrashContext>()]
            .iter()
            .all(|byte| *byte == 0)
    );
}

#[test]
fn test_odd_breadcrumb_ring_is_zeroed_while_stable_units_survive() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    let ring0 = SECTION2_OFFSET + std::mem::offset_of!(SutCrumbState, rings);
    let ring1 = ring0 + size_of::<SutCrumbRing>();
    unsafe {
        let base = shm.base_ptr();
        store_u32_release(
            base,
            SECTION2_OFFSET + std::mem::offset_of!(SutCrumbState, ring_count),
            2,
        );
        store_u32_release(
            base,
            ring0 + std::mem::offset_of!(SutCrumbRing, generation),
            1,
        );
        write_val::<u64>(base, ring0 + std::mem::offset_of!(SutCrumbRing, buf), 44);
        write_val::<u32>(
            base,
            ring0 + std::mem::offset_of!(SutCrumbRing, write_idx),
            1,
        );
        write_val::<u32>(base, ring0 + std::mem::offset_of!(SutCrumbRing, count), 1);

        write_val::<u64>(base, ring1 + std::mem::offset_of!(SutCrumbRing, buf), 55);
        write_val::<u32>(
            base,
            ring1 + std::mem::offset_of!(SutCrumbRing, write_idx),
            1,
        );
        write_val::<u32>(base, ring1 + std::mem::offset_of!(SutCrumbRing, count), 1);
        store_u32_release(
            base,
            ring1 + std::mem::offset_of!(SutCrumbRing, generation),
            2,
        );
        write_cstr(
            base,
            CONTEXT_OFFSET + std::mem::offset_of!(SutCrashContext, app_version),
            "stable",
        );
    }

    let owned = snapshot(&shm);
    assert_eq!(
        owned.consistency_issues(),
        &[ShmConsistencyIssue::BreadcrumbRing {
            index: 0,
            generation_before: 1,
            generation_after: 1,
        }]
    );
    let breadcrumbs = owned.read_breadcrumbs();
    assert_eq!(breadcrumbs.len(), 1);
    assert_eq!(breadcrumbs[0].timestamp_ns, 55);
    assert_eq!(
        owned.read_context().expect("stable context").app_version,
        "stable"
    );
    assert!(
        owned.raw_breadcrumb_bytes_owned()[..size_of::<SutCrumbRing>()]
            .iter()
            .all(|byte| *byte == 0)
    );
}

#[test]
fn test_odd_inactive_breadcrumb_ring_is_sanitized_for_raw_persistence() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    let ring0 = SECTION2_OFFSET + std::mem::offset_of!(SutCrumbState, rings);
    unsafe {
        let base = shm.base_ptr();
        store_u32_release(
            base,
            ring0 + std::mem::offset_of!(SutCrumbRing, generation),
            1,
        );
        *base.add(ring0) = 0xA5;
    }

    let owned = snapshot(&shm);
    assert_eq!(
        owned.consistency_issues(),
        &[ShmConsistencyIssue::BreadcrumbRing {
            index: 0,
            generation_before: 1,
            generation_after: 1,
        }]
    );
    assert!(owned.read_breadcrumbs().is_empty());
    assert!(
        owned.raw_breadcrumb_bytes_owned()[..size_of::<SutCrumbRing>()]
            .iter()
            .all(|byte| *byte == 0)
    );
}

#[test]
fn test_odd_registry_zeros_complete_breadcrumb_section() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    unsafe {
        let base = shm.base_ptr();
        store_u32_release(
            base,
            SECTION1_OFFSET + std::mem::offset_of!(ShmHeader, breadcrumb_registry_generation),
            3,
        );
        store_u32_release(
            base,
            SECTION2_OFFSET + std::mem::offset_of!(SutCrumbState, ring_count),
            1,
        );
        *base.add(SECTION2_OFFSET) = 0xA5;
    }

    let owned = snapshot(&shm);
    assert_eq!(
        owned.consistency_issues(),
        &[ShmConsistencyIssue::BreadcrumbRegistry {
            generation_before: 3,
            generation_after: 3,
            ring_count_before: 1,
            ring_count_after: 1,
        }]
    );
    assert!(
        owned
            .raw_breadcrumb_bytes_owned()
            .iter()
            .all(|byte| *byte == 0)
    );
}

#[test]
fn test_odd_settings_and_attachments_are_dropped_independently() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    unsafe {
        let base = shm.base_ptr();
        store_u32_release(
            base,
            SECTION1_OFFSET + std::mem::offset_of!(ShmHeader, settings_generation),
            5,
        );
        store_u32_release(
            base,
            SECTION1_OFFSET + std::mem::offset_of!(ShmHeader, attachments_generation),
            7,
        );
        write_val::<u32>(
            base,
            SETTINGS_OFFSET + std::mem::offset_of!(SutCrashSettingsSnapshot, entry_count),
            0,
        );
        write_val::<u32>(
            base,
            ATTACHMENT_OFFSET + std::mem::offset_of!(ShmAttachmentSection, count),
            1,
        );
        write_cstr(
            base,
            CONTEXT_OFFSET + std::mem::offset_of!(SutCrashContext, app_version),
            "still-valid",
        );
    }

    let owned = snapshot(&shm);
    assert_eq!(
        owned.consistency_issues(),
        &[
            ShmConsistencyIssue::Settings {
                generation_before: 5,
                generation_after: 5,
            },
            ShmConsistencyIssue::Attachments {
                generation_before: 7,
                generation_after: 7,
            },
        ]
    );
    assert!(owned.read_settings().is_none());
    assert!(owned.read_attachments().is_empty());
    assert_eq!(
        owned.read_context().expect("stable context").app_version,
        "still-valid"
    );
    let raw = owned.raw_context_bytes_owned();
    let settings_start = size_of::<SutCrashContext>();
    assert!(raw[settings_start..].iter().all(|byte| *byte == 0));
}

#[test]
fn test_odd_screenshot_is_zeroed_and_last_stable_slot_survives() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    let last = SCREENSHOT_SLOTS as usize - 1;
    let valid = SECTION4_OFFSET + std::mem::offset_of!(SutScreenshotSection, valid);
    let timestamp = SECTION4_OFFSET + std::mem::offset_of!(SutScreenshotSection, timestamp);
    let tier = SECTION4_OFFSET + std::mem::offset_of!(SutScreenshotSection, tier);
    let data = SECTION4_OFFSET + std::mem::offset_of!(SutScreenshotSection, data);
    unsafe {
        let base = shm.base_ptr();
        store_u32_release(base, valid, 1);
        write_val::<u64>(base, timestamp, 111);
        write_val::<u32>(base, tier, 9);
        *base.add(data) = 0xA5;

        write_val::<u64>(base, timestamp + last * size_of::<u64>(), 999);
        write_val::<u32>(base, tier + last * size_of::<u32>(), 4);
        *base.add(data + last * SCREENSHOT_BYTES_PER_SLOT) = 0xEF;
        store_u32_release(base, valid + last * size_of::<u32>(), 2);
    }

    let owned = snapshot(&shm);
    assert_eq!(
        owned.consistency_issues(),
        &[ShmConsistencyIssue::ScreenshotSlot {
            index: 0,
            generation_before: 1,
            generation_after: 1,
        }]
    );
    let screenshots = owned.read_screenshots();
    assert_eq!(screenshots.len(), 1);
    assert_eq!(screenshots[0].timestamp_ns, 999);
    assert_eq!(screenshots[0].rgba[0], 0xEF);

    assert_eq!(owned.bytes[valid..valid + size_of::<u32>()], [0; 4]);
    assert_eq!(owned.bytes[timestamp..timestamp + size_of::<u64>()], [0; 8]);
    assert_eq!(owned.bytes[tier..tier + size_of::<u32>()], [0; 4]);
    assert!(
        owned.bytes[data..data + SCREENSHOT_BYTES_PER_SLOT]
            .iter()
            .all(|byte| *byte == 0)
    );
}

#[test]
fn test_screenshot_sanitizer_checks_deadline_before_large_slot_zero() {
    let mut bytes = vec![0xA5; SCREENSHOT_DATA_OFFSET + SCREENSHOT_BYTES_PER_SLOT];
    let mut before = LivePublicationState::default();
    let mut after = LivePublicationState::default();
    before.screenshot_generations.fill(1);
    after.screenshot_generations.fill(1);
    let mut issues = Vec::new();

    assert_eq!(
        sanitize_screenshots(
            &mut bytes,
            &before,
            &after,
            &mut issues,
            Some(Instant::now()),
        ),
        Err(ShmSnapshotError::DeadlineExceeded)
    );
    assert!(issues.is_empty());
    assert_eq!(bytes[SCREENSHOT_DATA_OFFSET], 0xA5);
}

#[test]
fn test_raw_section_byte_lengths() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    let owned = snapshot(&shm);
    assert_eq!(owned.raw_breadcrumb_bytes_owned().len(), SECTION2_SIZE);
    assert_eq!(
        owned.raw_context_bytes_owned().len(),
        size_of::<SutCrashContext>() + size_of::<SutCrashSettingsSnapshot>()
    );
}

#[test]
fn test_raw_section_bytes_are_detached_from_live_mapping() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    unsafe {
        *shm.base_ptr().add(SECTION2_OFFSET) = 0x11;
        *shm.base_ptr().add(CONTEXT_OFFSET) = 0x22;
    }
    let owned = snapshot(&shm);
    unsafe {
        *shm.base_ptr().add(SECTION2_OFFSET) = 0xAA;
        *shm.base_ptr().add(CONTEXT_OFFSET) = 0xBB;
    }

    assert_eq!(owned.raw_breadcrumb_bytes_owned()[0], 0x11);
    assert_eq!(owned.raw_context_bytes_owned()[0], 0x22);
}

#[test]
fn test_owned_snapshot_survives_mapping_drop_and_arc_clone() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    unsafe {
        write_val::<u64>(
            shm.base_ptr(),
            CONTEXT_OFFSET + std::mem::offset_of!(SutCrashContext, heartbeat_counter),
            77,
        );
    }
    let first = Arc::new(snapshot(&shm));
    let cloned = first.clone();
    unsafe {
        write_val::<u64>(
            shm.base_ptr(),
            CONTEXT_OFFSET + std::mem::offset_of!(SutCrashContext, heartbeat_counter),
            88,
        );
    }

    drop(shm);

    assert_eq!(
        first
            .read_context()
            .expect("first context")
            .heartbeat_counter,
        77
    );
    assert_eq!(
        cloned
            .read_context()
            .expect("cloned context")
            .heartbeat_counter,
        77
    );
}
