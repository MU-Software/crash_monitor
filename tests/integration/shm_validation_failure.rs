//! Integration tests: corrupted SHM data is rejected at its owning-unit boundary.

use std::mem::{offset_of, size_of};
use std::sync::atomic::{AtomicU32, Ordering};

use crash_monitor::shm::*;

const BREADCRUMB_FILE_LEN: usize = 16;
const BREADCRUMB_MESSAGE_LEN: usize = 28;
const CONTEXT_APP_VERSION_LEN: usize = 16;
const ATTACHMENT_LABEL_LEN: usize = 32;
const ATTACHMENT_PATH_LEN: usize = 256;

fn unique_pid() -> u32 {
    static COUNTER: AtomicU32 = AtomicU32::new(800_000);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Write a value of type `T` at the given byte-pointer offset.
unsafe fn write_val<T: Copy>(base: *mut u8, offset: usize, val: T) {
    unsafe {
        let src = (&raw const val).cast::<u8>();
        std::ptr::copy_nonoverlapping(src, base.add(offset), size_of::<T>());
    }
}

unsafe fn write_bytes(base: *mut u8, offset: usize, bytes: &[u8]) {
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), base.add(offset), bytes.len());
    }
}

unsafe fn write_c_string(base: *mut u8, offset: usize, capacity: usize, value: &str) {
    assert!(value.len() < capacity);
    unsafe {
        std::ptr::write_bytes(base.add(offset), 0, capacity);
        write_bytes(base, offset, value.as_bytes());
    }
}

unsafe fn write_context_app_version(base: *mut u8, value: &str) {
    unsafe {
        write_c_string(
            base,
            CONTEXT_OFFSET + offset_of!(SutCrashContext, app_version),
            CONTEXT_APP_VERSION_LEN,
            value,
        );
    }
}

unsafe fn write_breadcrumb(
    ring: *mut u8,
    index: usize,
    timestamp_ns: u64,
    category: u16,
    severity: u16,
    file: &[u8],
    message: &[u8],
) {
    assert!(file.len() <= BREADCRUMB_FILE_LEN);
    assert!(message.len() <= BREADCRUMB_MESSAGE_LEN);
    let entry_offset = index * size_of::<SutBreadcrumb>();
    unsafe {
        std::ptr::write_bytes(ring.add(entry_offset), 0, size_of::<SutBreadcrumb>());
        write_val::<u64>(
            ring,
            entry_offset + offset_of!(SutBreadcrumb, timestamp_ns),
            timestamp_ns,
        );
        write_val::<u32>(ring, entry_offset + offset_of!(SutBreadcrumb, thread_id), 7);
        write_val::<u16>(
            ring,
            entry_offset + offset_of!(SutBreadcrumb, category),
            category,
        );
        write_val::<u16>(
            ring,
            entry_offset + offset_of!(SutBreadcrumb, severity),
            severity,
        );
        write_bytes(ring, entry_offset + offset_of!(SutBreadcrumb, file), file);
        write_bytes(
            ring,
            entry_offset + offset_of!(SutBreadcrumb, message),
            message,
        );
    }
}

unsafe fn publish_ring(base: *mut u8, entry_count: u32, ring_count: u32) {
    let ring = unsafe { base.add(SECTION2_OFFSET + offset_of!(SutCrumbState, rings)) };
    unsafe {
        write_val::<u32>(ring, offset_of!(SutCrumbRing, write_idx), entry_count);
        write_val::<u32>(ring, offset_of!(SutCrumbRing, count), entry_count);
        write_val::<u32>(
            base,
            SECTION2_OFFSET + offset_of!(SutCrumbState, ring_count),
            ring_count,
        );
    }
}

#[test]
fn test_corrupted_magic_returns_error() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");

    unsafe {
        write_val::<u32>(shm.base_ptr(), offset_of!(ShmHeader, magic), 0xBAD_BEEF);
    }
    assert_eq!(
        shm.snapshot_owned_until(None).unwrap_err(),
        ShmSnapshotError::InvalidMagic { found: 0xBAD_BEEF }
    );
}

#[test]
fn test_corrupted_canary_returns_error() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");

    unsafe {
        write_val::<u32>(shm.base_ptr(), FOOTER_OFFSET, 0x0000_0000);
    }
    assert_eq!(
        shm.snapshot_owned_until(None).unwrap_err(),
        ShmSnapshotError::InvalidCanary { found: 0 }
    );
}

#[test]
fn test_exact_old_and_future_schema_versions_are_rejected() {
    for version in [
        1,
        2,
        SHM_VERSION.checked_add(1).expect("future schema version"),
    ] {
        let shm = SharedMemory::create(unique_pid()).expect("shm create");
        unsafe {
            write_val::<u32>(shm.base_ptr(), offset_of!(ShmHeader, version), version);
        }
        assert_eq!(
            shm.snapshot_owned_until(None).unwrap_err(),
            ShmSnapshotError::UnsupportedVersion { found: version }
        );
    }
}

#[test]
fn test_invalid_annotation_counts_reject_context_and_preserve_settings() {
    let too_large = i32::try_from(MAX_ANNOTATIONS).expect("annotation maximum fits i32") + 1;
    for annotation_count in [-1, too_large] {
        let shm = SharedMemory::create(unique_pid()).expect("shm create");
        unsafe {
            write_val::<i32>(
                shm.base_ptr(),
                CONTEXT_OFFSET + offset_of!(SutCrashContext, annotation_count),
                annotation_count,
            );
            write_val::<i32>(
                shm.base_ptr(),
                SETTINGS_OFFSET + offset_of!(SutCrashSettingsSnapshot, palette_count),
                23,
            );
        }

        let snapshot = shm.snapshot_owned_until(None).expect("snapshot");
        assert!(
            snapshot.read_context().is_none(),
            "annotation_count {annotation_count} must reject its context"
        );
        assert_eq!(
            snapshot
                .read_settings()
                .expect("independent settings survive")
                .palette_count,
            23
        );
    }
}

#[test]
fn test_invalid_attachment_count_rejects_section_and_preserves_context() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    let slots = ATTACHMENT_OFFSET + offset_of!(ShmAttachmentSection, slots);
    unsafe {
        write_val::<u32>(
            shm.base_ptr(),
            ATTACHMENT_OFFSET + offset_of!(ShmAttachmentSection, count),
            u32::try_from(MAX_ATTACHMENTS).expect("attachment maximum fits u32") + 1,
        );
        write_c_string(
            shm.base_ptr(),
            slots + offset_of!(ShmAttachmentSlot, label),
            ATTACHMENT_LABEL_LEN,
            "must-not-survive",
        );
        write_c_string(
            shm.base_ptr(),
            slots + offset_of!(ShmAttachmentSlot, path),
            ATTACHMENT_PATH_LEN,
            "/tmp/must-not-survive.log",
        );
        write_context_app_version(shm.base_ptr(), "stable-context");
    }

    let snapshot = shm.snapshot_owned_until(None).expect("snapshot");
    assert!(snapshot.read_attachments().is_empty());
    assert_eq!(
        snapshot
            .read_context()
            .expect("independent context survives")
            .app_version,
        "stable-context"
    );
}

#[test]
fn test_invalid_ring_count_rejects_registry_and_preserves_context() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    let ring = unsafe {
        shm.base_ptr()
            .add(SECTION2_OFFSET + offset_of!(SutCrumbState, rings))
    };
    unsafe {
        write_breadcrumb(ring, 0, 10, 0, 0, b"valid.c\0", b"must-not-survive\0");
        publish_ring(
            shm.base_ptr(),
            1,
            u32::try_from(CRUMB_MAX_THREADS).expect("ring maximum fits u32") + 1,
        );
        write_context_app_version(shm.base_ptr(), "stable-context");
    }

    let snapshot = shm.snapshot_owned_until(None).expect("snapshot");
    assert!(snapshot.read_breadcrumbs().is_empty());
    assert_eq!(
        snapshot
            .read_context()
            .expect("independent context survives")
            .app_version,
        "stable-context"
    );
}

#[test]
fn test_malformed_breadcrumb_values_drop_only_their_entries() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    let ring = unsafe {
        shm.base_ptr()
            .add(SECTION2_OFFSET + offset_of!(SutCrumbState, rings))
    };
    unsafe {
        write_breadcrumb(ring, 0, 10, 0, 0, b"bad-\xff\0", b"invalid-utf8\0");
        write_breadcrumb(ring, 1, 20, 0, 0, b"control.c\0", b"line\nfeed\0");
        write_breadcrumb(
            ring,
            2,
            30,
            0,
            0,
            &[b'X'; BREADCRUMB_FILE_LEN],
            b"missing-nul\0",
        );
        write_breadcrumb(ring, 3, 40, u16::MAX, 0, b"range.c\0", b"bad-category\0");
        write_breadcrumb(ring, 4, 50, 0, u16::MAX, b"range.c\0", b"bad-severity\0");
        write_breadcrumb(ring, 5, 60, 0, 0, b"stable.c\0", b"survivor\0");
        publish_ring(shm.base_ptr(), 6, 1);
    }

    let breadcrumbs = shm
        .snapshot_owned_until(None)
        .expect("snapshot")
        .read_breadcrumbs();
    assert_eq!(breadcrumbs.len(), 1);
    assert_eq!(breadcrumbs[0].timestamp_ns, 60);
    assert_eq!(breadcrumbs[0].file, "stable.c");
    assert_eq!(breadcrumbs[0].message, "survivor");
}
