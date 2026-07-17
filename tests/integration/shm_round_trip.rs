//! Integration tests: write data to `SharedMemory` via raw pointers, read back through public API.

use std::mem::{offset_of, size_of};
use std::sync::atomic::{AtomicU32, Ordering};

use crash_monitor::shm::*;

/// Unique PID per test to avoid shm name collisions.
fn unique_pid() -> u32 {
    static COUNTER: AtomicU32 = AtomicU32::new(700_000);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Helper: write a C-style NUL-terminated string into a fixed-size byte array via raw pointer.
unsafe fn write_c_str(dst: *mut u8, s: &str, max_len: usize) {
    let bytes = s.as_bytes();
    let len = bytes.len().min(max_len - 1);
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, len);
        *dst.add(len) = 0;
    }
}

/// Helper: write a value of type T at the given byte pointer offset.
/// Uses `copy_nonoverlapping` to avoid `cast_ptr_alignment` lint on `*mut u8` → `*mut T`.
unsafe fn write_val<T: Copy>(base: *mut u8, offset: usize, val: T) {
    unsafe {
        let src = (&raw const val).cast::<u8>();
        std::ptr::copy_nonoverlapping(src, base.add(offset), size_of::<T>());
    }
}

#[test]
fn test_breadcrumbs_write_read() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    let base = shm.base_ptr();

    unsafe {
        // SutCrumbState is at SECTION2_OFFSET
        let crumb_state = base.add(SECTION2_OFFSET);

        // Set ring_count = 1 (at end of SutCrumbState: offset = rings_size)
        let ring_count_offset = size_of::<SutCrumbRing>() * CRUMB_MAX_THREADS;
        write_val::<u32>(crumb_state, ring_count_offset, 1);

        // Ring 0 is at crumb_state + 0
        let ring0 = crumb_state;

        // Write a SutBreadcrumb at ring0.buf[0]
        // SutBreadcrumb layout (64 bytes):
        //   timestamp_ns: u64 (0)
        //   thread_id: u32 (8)
        //   category: u16 (12)
        //   severity: u16 (14)
        //   file: [u8; 16] (16)
        //   line: u16 (32)
        //   pad: u16 (34)
        //   message: [u8; 28] (36)
        let entry = ring0; // buf starts at offset 0 of ring
        write_val::<u64>(entry, 0, 123_456_789); // timestamp_ns
        write_val::<u32>(entry, 8, 42); // thread_id
        write_val::<u16>(entry, 12, 3); // category
        write_val::<u16>(entry, 14, 2); // severity
        write_c_str(entry.add(16), "test.c", 16); // file
        write_val::<u16>(entry, 32, 99); // line
        write_c_str(entry.add(36), "hello crash", 28); // message

        // Set ring0.write_idx = 1, ring0.count = 1
        // write_idx is at offset: buf_size = 512 * 64 = 32768
        let buf_size = CRUMB_RING_CAPACITY * size_of::<SutBreadcrumb>();
        write_val::<u32>(ring0, buf_size, 1); // write_idx
        write_val::<u32>(ring0, buf_size + 4, 1); // count
    }

    let crumbs = shm.read_breadcrumbs();
    assert_eq!(crumbs.len(), 1);
    assert_eq!(crumbs[0].timestamp_ns, 123_456_789);
    assert_eq!(crumbs[0].thread_id, 42);
    assert_eq!(crumbs[0].category, 3);
    assert_eq!(crumbs[0].severity, 2);
    assert_eq!(crumbs[0].file, "test.c");
    assert_eq!(crumbs[0].line, 99);
    assert_eq!(crumbs[0].message, "hello crash");
}

#[test]
fn test_breadcrumbs_multi_ring_merge_sort() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    let base = shm.base_ptr();

    let ring_size = size_of::<SutCrumbRing>();
    let buf_size = CRUMB_RING_CAPACITY * size_of::<SutBreadcrumb>();

    unsafe {
        let crumb_state = base.add(SECTION2_OFFSET);

        // Set ring_count = 2
        let ring_count_offset = ring_size * CRUMB_MAX_THREADS;
        write_val::<u32>(crumb_state, ring_count_offset, 2);

        // Ring 0: one entry with timestamp 200
        let ring0 = crumb_state;
        let entry0 = ring0;
        write_val::<u64>(entry0, 0, 200); // timestamp_ns
        write_val::<u32>(entry0, 8, 1); // thread_id
        write_c_str(entry0.add(16), "a.c", 16);
        write_c_str(entry0.add(36), "second", 28);
        write_val::<u32>(ring0, buf_size, 1); // write_idx
        write_val::<u32>(ring0, buf_size + 4, 1); // count

        // Ring 1: one entry with timestamp 100
        let ring1 = crumb_state.add(ring_size);
        let entry1 = ring1;
        write_val::<u64>(entry1, 0, 100); // timestamp_ns
        write_val::<u32>(entry1, 8, 2); // thread_id
        write_c_str(entry1.add(16), "b.c", 16);
        write_c_str(entry1.add(36), "first", 28);
        write_val::<u32>(ring1, buf_size, 1); // write_idx
        write_val::<u32>(ring1, buf_size + 4, 1); // count
    }

    let crumbs = shm.read_breadcrumbs();
    assert_eq!(crumbs.len(), 2);
    // Sorted by timestamp: 100 first, 200 second
    assert_eq!(crumbs[0].timestamp_ns, 100);
    assert_eq!(crumbs[0].message, "first");
    assert_eq!(crumbs[0].thread_id, 2);
    assert_eq!(crumbs[1].timestamp_ns, 200);
    assert_eq!(crumbs[1].message, "second");
    assert_eq!(crumbs[1].thread_id, 1);
}

#[test]
fn test_context_write_read() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    let base = shm.base_ptr();

    unsafe {
        let ctx = base.add(CONTEXT_OFFSET);

        // active_tool: [u8; 32] at offset 0
        write_c_str(ctx, "brush", 32);

        // region_count: i32
        write_val::<i32>(ctx, offset_of!(SutCrashContext, region_count), 7);

        // voxel_count: i32
        write_val::<i32>(ctx, offset_of!(SutCrashContext, voxel_count), 1024);
    }

    let ctx = shm.read_context().expect("read_context should succeed");
    assert_eq!(ctx.active_tool, "brush");
    assert_eq!(ctx.region_count, 7);
    assert_eq!(ctx.voxel_count, 1024);
}

#[test]
fn test_heartbeat_round_trip() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    assert_eq!(shm.read_heartbeat(), 0);

    unsafe {
        write_val::<u64>(
            shm.base_ptr(),
            CONTEXT_OFFSET + offset_of!(SutCrashContext, heartbeat_counter),
            999,
        );
    }

    assert_eq!(shm.read_heartbeat(), 999);
}

#[test]
fn test_settings_round_trip() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    let base = shm.base_ptr();

    unsafe {
        let settings = base.add(SETTINGS_OFFSET);

        // world_bound_min: [i32; 3] at offset 0
        write_val::<i32>(settings, 0, -150);
        write_val::<i32>(settings, 4, 0);
        write_val::<i32>(settings, 8, -150);

        // world_bound_max: [i32; 3] at offset 12
        write_val::<i32>(settings, 12, 150);
        write_val::<i32>(settings, 16, 300);
        write_val::<i32>(settings, 20, 150);
    }

    let s = shm.read_settings().expect("read_settings should succeed");
    assert_eq!(s.world_bound_min, [-150, 0, -150]);
    assert_eq!(s.world_bound_max, [150, 300, 150]);
}
