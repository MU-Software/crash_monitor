//! POSIX shared memory creation and cleanup (`shm_open`, `mmap`, `munmap`, `shm_unlink`).
//!
//! Platform-specific: works on macOS and Linux. Windows would need a different
//! implementation using `CreateFileMapping` / `MapViewOfFile`.

use std::ffi::CString;
use std::ptr;
use std::sync::atomic::Ordering;

use nix::libc;

use crate::shm::SharedMemory;
use crate::shm::types::{
    CRUMB_MAX_THREADS, CRUMB_RING_CAPACITY, FOOTER_OFFSET, SCREENSHOT_HEIGHT, SCREENSHOT_SLOTS,
    SCREENSHOT_WIDTH, SHM_CANARY, SHM_MAGIC, SHM_TOTAL_SIZE, SHM_VERSION, ShmHeader,
};

/// Create a new POSIX shared memory region for the given monitor PID.
///
/// # Errors
/// Returns an error if `shm_open`, `ftruncate`, or `mmap` fails.
pub fn create_shared_memory(monitor_pid: u32) -> Result<SharedMemory, String> {
    let name = format!("/mbb_crash_{monitor_pid}");
    let c_name = CString::new(name.as_str()).map_err(|e| format!("CString::new failed: {e}"))?;

    // Try to unlink any stale segment with the same name (e.g., from a crashed test).
    // Errors are ignored — the segment may not exist.
    unsafe {
        libc::shm_unlink(c_name.as_ptr());
    }

    // SAFETY: shm_open creates/opens a POSIX shared memory object.
    let fd = unsafe {
        libc::shm_open(
            c_name.as_ptr(),
            libc::O_CREAT | libc::O_RDWR | libc::O_TRUNC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(format!(
            "shm_open({name}) failed: {}",
            std::io::Error::last_os_error()
        ));
    }

    // Size the region
    #[allow(clippy::cast_possible_wrap)]
    let rc = unsafe { libc::ftruncate(fd, SHM_TOTAL_SIZE as libc::off_t) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        unsafe {
            libc::close(fd);
        }
        return Err(format!("ftruncate failed: {err}"));
    }

    // Map the region
    let base = unsafe {
        libc::mmap(
            ptr::null_mut(),
            SHM_TOTAL_SIZE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        )
    };
    unsafe {
        libc::close(fd);
    } // fd no longer needed after mmap

    if base == libc::MAP_FAILED {
        return Err(format!("mmap failed: {}", std::io::Error::last_os_error()));
    }

    let base = base.cast::<u8>();

    // Zero the entire region
    unsafe {
        ptr::write_bytes(base, 0, SHM_TOTAL_SIZE);
    }

    // Write header
    // SAFETY: mmap returns page-aligned memory (4KB+), so casting to ShmHeader is safe.
    #[allow(clippy::cast_ptr_alignment)]
    let header = base.cast::<ShmHeader>();
    unsafe {
        (*header).magic = SHM_MAGIC;
        (*header).version = SHM_VERSION;
        #[allow(clippy::cast_possible_truncation)] // compile-time constants < u32::MAX
        {
            (*header).ring_capacity_per_thread = CRUMB_RING_CAPACITY as u32;
            (*header).max_threads = CRUMB_MAX_THREADS as u32;
        }
        (*header).ring_count.store(0, Ordering::Release);
        (*header).screenshot_slots = SCREENSHOT_SLOTS;
        (*header).screenshot_width = SCREENSHOT_WIDTH;
        (*header).screenshot_height = SCREENSHOT_HEIGHT;
    }

    // Write canary
    unsafe {
        #[allow(clippy::cast_ptr_alignment)] // mmap is page-aligned
        let canary_ptr = base.add(FOOTER_OFFSET).cast::<u32>();
        ptr::write_volatile(canary_ptr, SHM_CANARY);
    }

    #[allow(clippy::cast_precision_loss)] // display only
    let size_mb = SHM_TOTAL_SIZE as f64 / (1024.0 * 1024.0);
    eprintln!("[monitor] Shared memory created: {name} ({SHM_TOTAL_SIZE} bytes, {size_mb:.1} MB)");

    Ok(SharedMemory {
        name,
        base,
        size: SHM_TOTAL_SIZE,
    })
}

impl Drop for SharedMemory {
    fn drop(&mut self) {
        // SAFETY: munmap + shm_unlink are safe cleanup operations.
        unsafe {
            libc::munmap(self.base.cast(), self.size);
        }
        if let Ok(c_name) = CString::new(self.name.as_str()) {
            unsafe {
                libc::shm_unlink(c_name.as_ptr());
            }
        }
        eprintln!("[monitor] Shared memory unlinked: {}", self.name);
    }
}
