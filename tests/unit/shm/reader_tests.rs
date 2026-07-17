use super::*;

use std::mem::{MaybeUninit, size_of};
use std::os::raw::c_char;
use std::sync::atomic::{AtomicU32, Ordering};

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

#[test]
fn test_shm_create_validate_drop() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    assert!(shm.validate(), "freshly created shm should validate");
    assert!(shm.name().starts_with("/crash_monitor_"));
    assert_eq!(shm.read_heartbeat(), 0);
    assert!(shm.read_breadcrumbs().is_empty());
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
    assert!(shm.validate());
    // Corrupt magic
    unsafe {
        write_val::<u32>(shm.base_ptr(), 0, 0xDEAD_DEAD);
    }
    assert!(!shm.validate());
}

#[test]
fn test_validate_corrupted_canary() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    assert!(shm.validate());
    // Corrupt canary
    unsafe {
        write_val::<u32>(shm.base_ptr(), FOOTER_OFFSET, 0x0000_0000);
    }
    assert!(!shm.validate());
}

#[test]
fn test_read_breadcrumbs_empty_rings() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    let crumbs = shm.read_breadcrumbs();
    assert!(crumbs.is_empty());
}

#[test]
fn test_read_context_fresh_shm() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    let ctx = shm.read_context();
    assert!(ctx.is_some());
    let ctx = ctx.unwrap();
    assert_eq!(ctx.heartbeat_counter, 0);
    assert!(ctx.annotations.is_empty());
    assert!(ctx.session_id.is_empty());
}

#[test]
fn test_read_heartbeat_write_read() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    assert_eq!(shm.read_heartbeat(), 0);
    // Write a known heartbeat value at the correct offset
    unsafe {
        write_val::<u64>(
            shm.base_ptr(),
            CONTEXT_OFFSET + std::mem::offset_of!(SutCrashContext, heartbeat_counter),
            42,
        );
    }
    assert_eq!(shm.read_heartbeat(), 42);
}

// ── convert_c_context / convert_c_settings unit tests ──

/// Helper: create a zeroed `SutCrashContext`.
fn zeroed_context() -> SutCrashContext {
    // SAFETY: SutCrashContext is repr(C) with no padding invariants beyond zero-fill.
    unsafe { MaybeUninit::zeroed().assume_init() }
}

/// Helper: create a zeroed `SutCrashSettingsSnapshot`.
fn zeroed_settings() -> SutCrashSettingsSnapshot {
    // SAFETY: SutCrashSettingsSnapshot is repr(C), Copy, all-zero is valid.
    unsafe { MaybeUninit::zeroed().assume_init() }
}

/// Helper: write a NUL-terminated string into a fixed-size `c_char` array
/// (bindgen renders C `char[]` as `c_char`).
fn write_c_string(dst: &mut [c_char], s: &str) {
    // SAFETY: c_char and u8 share size/alignment; we only write bytes.
    let dst: &mut [u8] =
        unsafe { std::slice::from_raw_parts_mut(dst.as_mut_ptr().cast::<u8>(), dst.len()) };
    let bytes = s.as_bytes();
    let len = bytes.len().min(dst.len() - 1);
    dst[..len].copy_from_slice(&bytes[..len]);
    dst[len] = 0;
}

#[test]
fn test_convert_c_context_basic_fields() {
    let mut c = zeroed_context();
    c.heartbeat_counter = 999;
    c.session_start_ns = 123_456_789;
    c.build_number = 7;
    c.git_dirty = true;
    // App/domain state arrives as generic key-value annotations.
    c.annotation_count = 2;
    write_c_string(&mut c.annotations[0].key, "active_tool");
    write_c_string(&mut c.annotations[0].value, "brush");
    write_c_string(&mut c.annotations[1].key, "voxel_count");
    write_c_string(&mut c.annotations[1].value, "1024");

    let r = convert_c_context(&c);

    assert_eq!(r.heartbeat_counter, 999);
    assert_eq!(r.session_start_ns, 123_456_789);
    assert_eq!(r.build_number, 7);
    assert!(r.git_dirty);
    assert_eq!(r.annotations.len(), 2);
    assert_eq!(
        r.annotations[0],
        ("active_tool".to_string(), "brush".to_string())
    );
    assert_eq!(
        r.annotations[1],
        ("voxel_count".to_string(), "1024".to_string())
    );
    // Zeroed string fields should be empty
    assert!(r.session_id.is_empty());
    assert!(r.app_version.is_empty());
    assert!(r.git_hash.is_empty());
}

#[test]
fn test_convert_c_context_annotation_count_clamped() {
    let mut c = zeroed_context();
    // A corrupt/over-large count must be clamped, never read out of bounds.
    c.annotation_count = 9999;
    let r = convert_c_context(&c);
    assert_eq!(r.annotations.len(), crate::shm::types::MAX_ANNOTATIONS);
}

#[test]
fn test_convert_c_settings_basic() {
    let mut s = zeroed_settings();
    s.world_bound_min = [-150, 0, -150];
    s.world_bound_max = [150, 300, 150];
    s.palette_count = 64;
    s.history_max = 100;
    write_c_string(&mut s.extra, "ssao=on");

    let r = convert_c_settings(&s);

    assert_eq!(r.world_bound_min, [-150, 0, -150]);
    assert_eq!(r.world_bound_max, [150, 300, 150]);
    assert_eq!(r.palette_count, 64);
    assert_eq!(r.history_max, 100);
    assert_eq!(r.extra, "ssao=on");
}

#[test]
fn test_convert_c_context_string_fields() {
    let mut c = zeroed_context();
    write_c_string(&mut c.session_id, "sess-abc");
    write_c_string(&mut c.app_version, "1.2.3");
    write_c_string(&mut c.git_hash, "deadbeef");
    write_c_string(&mut c.build_type, "release");
    write_c_string(&mut c.build_preset, "default");
    write_c_string(&mut c.build_timestamp, "2026-07-17");
    write_c_string(&mut c.compiler, "clang-17");
    write_c_string(&mut c.os_version, "macOS 15.3");

    let r = convert_c_context(&c);

    assert_eq!(r.session_id, "sess-abc");
    assert_eq!(r.app_version, "1.2.3");
    assert_eq!(r.git_hash, "deadbeef");
    assert_eq!(r.build_type, "release");
    assert_eq!(r.build_preset, "default");
    assert_eq!(r.build_timestamp, "2026-07-17");
    assert_eq!(r.compiler, "clang-17");
    assert_eq!(r.os_version, "macOS 15.3");
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

    let s = shm.read_settings().expect("settings should read");
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

    let att = shm.read_attachments();
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
    assert!(shm.read_attachments().is_empty());
}

#[test]
fn test_read_screenshots_one_valid_slot() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    unsafe {
        let base = shm.base_ptr();
        // valid[0] = 1 (u32 at SECTION4_OFFSET + 0)
        write_val::<u32>(base, SECTION4_OFFSET, 1);
        // timestamp[0] (u64 at SECTION4_OFFSET + 96*4)
        write_val::<u64>(base, SECTION4_OFFSET + 96 * 4, 777_000);
    }

    let shots = shm.read_screenshots();
    assert_eq!(shots.len(), 1);
    assert_eq!(shots[0].timestamp_ns, 777_000);
    assert_eq!(shots[0].width, SCREENSHOT_WIDTH);
    assert_eq!(shots[0].height, SCREENSHOT_HEIGHT);
    assert_eq!(shots[0].rgba.len(), SCREENSHOT_BYTES_PER_SLOT);
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

    let crumbs = shm.read_breadcrumbs();
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
    assert_eq!(shm.raw_breadcrumb_bytes().len(), SECTION2_SIZE);
    assert_eq!(
        shm.raw_context_bytes().len(),
        size_of::<SutCrashContext>() + size_of::<SutCrashSettingsSnapshot>()
    );
}
