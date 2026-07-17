use super::*;

use std::mem::size_of;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

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

/// Helper: write a NUL-terminated string into the shm buffer at `offset`.
unsafe fn write_cstr(base: *mut u8, offset: usize, s: &str) {
    let bytes = s.as_bytes();
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), base.add(offset), bytes.len());
        *base.add(offset + bytes.len()) = 0;
    }
}

fn snapshot(shm: &SharedMemory) -> OwnedShmSnapshot {
    shm.snapshot_owned_until(None).expect("shm snapshot")
}

#[test]
fn test_shm_create_snapshot_drop() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    let snapshot = snapshot(&shm);
    assert!(shm.name().starts_with("/crash_monitor_"));
    assert_eq!(shm.read_live_heartbeat(), 0);
    assert!(snapshot.read_breadcrumbs().is_empty());
    // Drop cleans up
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
        ShmSnapshotError::InvalidMagic { found: 0xDEAD_DEAD }
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
        ShmSnapshotError::InvalidCanary { found: 0 }
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
    let actual_size = shm.size;
    shm.size = FOOTER_OFFSET;
    assert_eq!(
        shm.snapshot_owned_until(None).unwrap_err(),
        ShmSnapshotError::MappingTooSmall {
            mapped: FOOTER_OFFSET,
            required: SHM_TOTAL_SIZE,
        }
    );
    // Restore the true mmap length before Drop calls munmap.
    shm.size = actual_size;
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

// ── owned byte parser tests ──

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
fn test_read_context_annotation_count_is_clamped() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    unsafe {
        write_val::<i32>(
            shm.base_ptr(),
            CONTEXT_OFFSET + std::mem::offset_of!(SutCrashContext, annotation_count),
            9999,
        );
    }
    let context = snapshot(&shm).read_context().expect("context");
    assert_eq!(context.annotations.len(), MAX_ANNOTATIONS);
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

// ── read_settings / read_attachments / read_screenshots / read_breadcrumbs with data ──

#[test]
fn test_read_settings_populated() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    unsafe {
        let base = shm.base_ptr();
        write_val::<[i32; 3]>(
            base,
            SETTINGS_OFFSET + std::mem::offset_of!(SutCrashSettingsSnapshot, world_bound_min),
            [-1, -2, -3],
        );
        write_val::<[i32; 3]>(
            base,
            SETTINGS_OFFSET + std::mem::offset_of!(SutCrashSettingsSnapshot, world_bound_max),
            [1, 2, 3],
        );
        write_val::<i32>(
            base,
            SETTINGS_OFFSET + std::mem::offset_of!(SutCrashSettingsSnapshot, palette_count),
            16,
        );
        write_val::<i32>(
            base,
            SETTINGS_OFFSET + std::mem::offset_of!(SutCrashSettingsSnapshot, history_max),
            50,
        );
    }

    let s = snapshot(&shm)
        .read_settings()
        .expect("settings should read");
    assert_eq!(s.world_bound_min, [-1, -2, -3]);
    assert_eq!(s.world_bound_max, [1, 2, 3]);
    assert_eq!(s.palette_count, 16);
    assert_eq!(s.history_max, 50);
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
fn test_read_screenshots_one_valid_slot() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    unsafe {
        let base = shm.base_ptr();
        write_val::<u32>(
            base,
            SECTION4_OFFSET + std::mem::offset_of!(SutScreenshotSection, valid),
            1,
        );
        write_val::<u64>(
            base,
            SECTION4_OFFSET + std::mem::offset_of!(SutScreenshotSection, timestamp),
            777_000,
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
    assert_eq!(shots[0].width, SCREENSHOT_WIDTH);
    assert_eq!(shots[0].height, SCREENSHOT_HEIGHT);
    assert_eq!(shots[0].rgba.len(), SCREENSHOT_BYTES_PER_SLOT);
    assert_eq!(shots[0].rgba[0], 0xA5);
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
        write_val::<u32>(shm.base_ptr(), valid_offset, 1);
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
