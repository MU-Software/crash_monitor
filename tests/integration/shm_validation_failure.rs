//! Integration tests: corrupted shm data should return empty/None, never panic.

use std::mem::size_of;
use std::sync::atomic::{AtomicU32, Ordering};

use crash_monitor::shm::*;

fn unique_pid() -> u32 {
    static COUNTER: AtomicU32 = AtomicU32::new(800_000);
    COUNTER.fetch_add(1, Ordering::Relaxed)
}

/// Helper: write a value of type T at the given byte pointer offset.
unsafe fn write_val<T: Copy>(base: *mut u8, offset: usize, val: T) {
    unsafe {
        let src = (&raw const val).cast::<u8>();
        std::ptr::copy_nonoverlapping(src, base.add(offset), size_of::<T>());
    }
}

#[test]
fn test_corrupted_magic_returns_empty() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");

    // Corrupt magic
    unsafe {
        write_val::<u32>(shm.base_ptr(), 0, 0xBAD_BEEF);
    }
    assert_eq!(
        shm.snapshot_owned_until(None).unwrap_err(),
        ShmSnapshotError::InvalidMagic { found: 0xBAD_BEEF }
    );
}

#[test]
fn test_corrupted_canary_returns_empty() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");

    // Corrupt canary at FOOTER_OFFSET
    unsafe {
        write_val::<u32>(shm.base_ptr(), FOOTER_OFFSET, 0x0000_0000);
    }
    assert_eq!(
        shm.snapshot_owned_until(None).unwrap_err(),
        ShmSnapshotError::InvalidCanary { found: 0 }
    );
}

#[test]
fn test_ring_count_clamped() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");

    // Set ring_count to an absurd value (1000) via raw pointer
    unsafe {
        let crumb_state = shm.base_ptr().add(SECTION2_OFFSET);
        let ring_count_offset = std::mem::size_of::<SutCrumbRing>() * CRUMB_MAX_THREADS;
        write_val::<u32>(crumb_state, ring_count_offset, 1000);
    }

    // Should not crash -- ring_count is clamped to CRUMB_MAX_THREADS internally.
    // No valid entries were written, so the result should be empty.
    let crumbs = shm
        .snapshot_owned_until(None)
        .expect("snapshot")
        .read_breadcrumbs();
    assert!(
        crumbs.is_empty(),
        "no valid entries were written, got {}",
        crumbs.len()
    );
}
