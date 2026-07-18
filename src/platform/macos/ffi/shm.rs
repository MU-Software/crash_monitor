//! POSIX shared memory creation and cleanup (`shm_open`, `mmap`, `munmap`, `shm_unlink`).
//!
//! Platform-specific: works on macOS and Linux. Windows would need a different
//! implementation using `CreateFileMapping` / `MapViewOfFile`.

use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::ptr;

use nix::libc;
use uuid::Uuid;

use crate::shm::types::{
    CRUMB_MAX_THREADS, CRUMB_RING_CAPACITY, FOOTER_OFFSET, SCREENSHOT_HEIGHT, SCREENSHOT_SLOTS,
    SCREENSHOT_WIDTH, SHM_CANARY, SHM_MAGIC, SHM_PRODUCER_NOT_READY, SHM_TOTAL_SIZE, SHM_VERSION,
    ShmHeader,
};

/// Low-level owner of one mapped POSIX shared-memory object.
///
/// The mapping is unmapped before its name is unlinked. High-level readers
/// borrow only its pointer/length accessors and never participate in syscall
/// cleanup.
pub struct ShmMapping {
    name: String,
    base: *mut u8,
    size: usize,
}

// SAFETY: ownership may move between threads. High-level access is restricted
// to the atomic publication API or an owned snapshot while the producer task
// is suspended.
unsafe impl Send for ShmMapping {}
unsafe impl Sync for ShmMapping {}

impl ShmMapping {
    #[must_use]
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub(crate) fn base_ptr(&self) -> *mut u8 {
        self.base
    }

    #[must_use]
    pub(crate) fn len(&self) -> usize {
        self.size
    }

    #[cfg(test)]
    pub(crate) fn set_len_for_test(&mut self, size: usize) {
        self.size = size;
    }
}

const SHM_CREATE_ATTEMPTS: usize = 8;
/// Darwin's POSIX shared-memory namespace accepts at most 30 visible bytes
/// (`PSEMNAMLEN` includes the trailing NUL). Keep one byte of headroom.
const MAX_DARWIN_SHM_NAME_BYTES: usize = 29;

struct PendingShm {
    name: String,
    c_name: CString,
    fd: Option<OwnedFd>,
    owns_name: bool,
}

impl PendingShm {
    fn publish_mapping(mut self, base: *mut u8) -> ShmMapping {
        drop(self.fd.take());
        self.owns_name = false;
        ShmMapping {
            name: self.name.clone(),
            base,
            size: SHM_TOTAL_SIZE,
        }
    }
}

impl Drop for PendingShm {
    fn drop(&mut self) {
        // `OwnedFd` closes after this body. Unlinking first prevents new opens
        // while the failed creator still owns the last descriptor.
        if self.owns_name {
            unsafe {
                libc::shm_unlink(self.c_name.as_ptr());
            }
        }
    }
}

fn random_shm_name(monitor_pid: u32) -> String {
    // 8 hexadecimal PID digits + 16 nonce digits give a 64-bit random
    // collision domain while remaining valid on Darwin. O_EXCL and bounded
    // retries remain the authority if a collision does occur.
    let nonce = (Uuid::new_v4().as_u128() >> 64) as u64;
    let name = format!("/cm_{monitor_pid:08x}_{nonce:016x}");
    debug_assert!(name.len() <= MAX_DARWIN_SHM_NAME_BYTES);
    name
}

fn open_exclusive_shm(monitor_pid: u32) -> Result<PendingShm, String> {
    for _ in 0..SHM_CREATE_ATTEMPTS {
        let name = random_shm_name(monitor_pid);
        let c_name = CString::new(name.as_str())
            .map_err(|error| format!("shared-memory name contains NUL: {error}"))?;
        // SAFETY: name is NUL-terminated and O_EXCL ensures this creator never
        // attaches to or truncates an existing object.
        let raw_fd = unsafe {
            libc::shm_open(
                c_name.as_ptr(),
                libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
                0o600,
            )
        };
        if raw_fd >= 0 {
            // SAFETY: successful shm_open returned one newly-owned descriptor.
            let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
            return Ok(PendingShm {
                name,
                c_name,
                fd: Some(fd),
                owns_name: true,
            });
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EEXIST) {
            return Err(format!("shm_open({name}) failed: {error}"));
        }
    }
    Err(format!(
        "failed to allocate a unique shared-memory name after {SHM_CREATE_ATTEMPTS} attempts"
    ))
}

fn validate_created_object(fd: &OwnedFd) -> Result<(), String> {
    // SAFETY: `stat` is writable and fd is live for the call.
    let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe { libc::fstat(fd.as_raw_fd(), &raw mut stat) } != 0 {
        return Err(format!("fstat failed: {}", std::io::Error::last_os_error()));
    }
    if stat.st_uid != unsafe { libc::geteuid() } {
        return Err(format!(
            "shared-memory owner mismatch: expected uid {}, found {}",
            unsafe { libc::geteuid() },
            stat.st_uid
        ));
    }
    let file_type = stat.st_mode & libc::S_IFMT;
    // Darwin's POSIX shm descriptors report permission bits but no vnode type
    // bits (`S_IFMT == 0`). Other platforms expose them as regular files.
    #[cfg(target_os = "macos")]
    let valid_file_type = file_type == 0 || file_type == libc::S_IFREG;
    #[cfg(not(target_os = "macos"))]
    let valid_file_type = file_type == libc::S_IFREG;
    if !valid_file_type {
        return Err(format!(
            "shared-memory object is not regular: mode={:#o}",
            stat.st_mode
        ));
    }
    #[cfg(target_os = "macos")]
    let valid_size = {
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        let rounded_size = usize::try_from(page_size)
            .ok()
            .filter(|size| *size > 0)
            .and_then(|size| SHM_TOTAL_SIZE.checked_next_multiple_of(size));
        stat.st_size == SHM_TOTAL_SIZE as libc::off_t
            || rounded_size.is_some_and(|size| stat.st_size == size as libc::off_t)
    };
    #[cfg(not(target_os = "macos"))]
    let valid_size = stat.st_size == SHM_TOTAL_SIZE as libc::off_t;
    if !valid_size {
        return Err(format!(
            "shared-memory size mismatch: expected {SHM_TOTAL_SIZE}, found {}",
            stat.st_size
        ));
    }
    Ok(())
}

/// Create a new POSIX shared memory region for the given monitor PID.
///
/// # Errors
/// Returns an error if `shm_open`, `ftruncate`, or `mmap` fails.
pub fn create_shared_memory(monitor_pid: u32) -> Result<ShmMapping, String> {
    let pending = open_exclusive_shm(monitor_pid)?;
    let Some(fd) = pending.fd.as_ref() else {
        return Err("internal error: pending shm lost its descriptor".to_string());
    };

    // Size the region
    #[allow(clippy::cast_possible_wrap)]
    let rc = unsafe { libc::ftruncate(fd.as_raw_fd(), SHM_TOTAL_SIZE as libc::off_t) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        return Err(format!("ftruncate failed: {err}"));
    }
    validate_created_object(fd)?;

    // Map the region
    let base = unsafe {
        libc::mmap(
            ptr::null_mut(),
            SHM_TOTAL_SIZE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd.as_raw_fd(),
            0,
        )
    };
    if base == libc::MAP_FAILED {
        return Err(format!("mmap failed: {}", std::io::Error::last_os_error()));
    }

    let base = base.cast::<u8>();

    // A newly-created, extended POSIX shm object reads as zero without eagerly
    // dirtying every payload page. Initialize only schema metadata and canary.
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
        // The region is not shared with a producer yet, so plain zero
        // initialization is valid here. All live access to these words uses
        // the atomic publication contract.
        (*header).breadcrumb_registry_generation = 0;
        (*header).screenshot_slots = SCREENSHOT_SLOTS;
        (*header).screenshot_width = SCREENSHOT_WIDTH;
        (*header).screenshot_height = SCREENSHOT_HEIGHT;
        (*header).context_generation = 0;
        (*header).settings_generation = 0;
        (*header).attachments_generation = 0;
        (*header).producer_ready = SHM_PRODUCER_NOT_READY;
    }

    // Write canary
    unsafe {
        #[allow(clippy::cast_ptr_alignment)] // mmap is page-aligned
        let canary_ptr = base.add(FOOTER_OFFSET).cast::<u32>();
        ptr::write_volatile(canary_ptr, SHM_CANARY);
    }

    #[allow(clippy::cast_precision_loss)] // display only
    let size_mb = SHM_TOTAL_SIZE as f64 / (1024.0 * 1024.0);
    eprintln!(
        "[monitor] Shared memory created: {} ({SHM_TOTAL_SIZE} bytes, {size_mb:.1} MB)",
        pending.name
    );

    Ok(pending.publish_mapping(base))
}

impl Drop for ShmMapping {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_names_keep_pid_prefix_and_do_not_repeat() {
        let first = random_shm_name(42);
        let second = random_shm_name(42);
        assert!(first.starts_with("/cm_0000002a_"));
        assert_ne!(first, second);
        assert_eq!(first.len(), MAX_DARWIN_SHM_NAME_BYTES);
    }

    #[test]
    fn actual_shared_memory_name_is_accepted_and_unlinked_on_drop() {
        let name = {
            let mapping = create_shared_memory(std::process::id()).expect("create shared memory");
            assert!(mapping.name().len() <= MAX_DARWIN_SHM_NAME_BYTES);
            mapping.name().to_string()
        };

        let c_name = CString::new(name).unwrap();
        let reopened = unsafe { libc::shm_open(c_name.as_ptr(), libc::O_RDWR, 0) };
        assert_eq!(reopened, -1, "drop must unlink the shared-memory name");
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ENOENT)
        );
    }
}
