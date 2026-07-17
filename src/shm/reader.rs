//! `SharedMemory` handle: creation, validation, reading, and cleanup.

use std::mem::size_of;
use std::ptr;

#[allow(clippy::wildcard_imports)] // re-export convenience — all types are public
use super::types::*;

// ═══════════════════════════════════════════════════
//  Pure C→Rust conversion functions
// ═══════════════════════════════════════════════════

/// Convert a C `SutCrashContext` to the Rust `RawCrashContext`.
pub(crate) fn convert_c_context(c: &SutCrashContext) -> RawCrashContext {
    RawCrashContext {
        active_tool: c_array_to_string(&c.active_tool),
        region_count: c.region_count,
        voxel_count: c.voxel_count,
        undo_depth: c.undo_depth,
        redo_depth: c.redo_depth,
        last_action_id: c.last_action_id,
        frame_number: c.frame_number,
        alloc_count: c.alloc_count,
        free_count: c.free_count,
        alloc_bytes_total: c.alloc_bytes_total,
        thread_pool_size: c.thread_pool_size,
        active_batch: c.active_batch,
        heartbeat_counter: c.heartbeat_counter,
        session_start_ns: c.session_start_ns,
        session_id: c_array_to_string(&c.session_id),
        tags: read_tags(&c.tags, c.tag_count),
        app_version: c_array_to_string(&c.app_version),
        build_number: c.build_number,
        git_hash: c_array_to_string(&c.git_hash),
        git_dirty: c.git_dirty,
        build_type: c_array_to_string(&c.build_type),
        build_preset: c_array_to_string(&c.build_preset),
        build_timestamp: c_array_to_string(&c.build_timestamp),
        compiler: c_array_to_string(&c.compiler),
        os_version: c_array_to_string(&c.os_version),
    }
}

/// Convert a C `SutCrashSettingsSnapshot` to the Rust `RawSettingsSnapshot`.
pub(crate) fn convert_c_settings(s: &SutCrashSettingsSnapshot) -> RawSettingsSnapshot {
    RawSettingsSnapshot {
        world_bound_min: s.world_bound_min,
        world_bound_max: s.world_bound_max,
        palette_count: s.palette_count,
        history_max: s.history_max,
        extra: c_array_to_string(&s.extra),
    }
}

// ═══════════════════════════════════════════════════
//  SharedMemory handle
// ═══════════════════════════════════════════════════

/// Owns a POSIX shared memory region. Platform creates via `platform::macos::ffi::shm`,
/// unmaps+unlinks on drop (also in platform layer).
pub struct SharedMemory {
    pub(crate) name: String,
    pub(crate) base: *mut u8,
    pub(crate) size: usize,
}

// SAFETY: SharedMemory is essentially a file descriptor + mmap pointer.
// The mapped region is only accessed while the child is suspended (single-threaded access)
// or via atomic reads for heartbeat (watchdog thread).
unsafe impl Send for SharedMemory {}
unsafe impl Sync for SharedMemory {}

impl SharedMemory {
    /// Create a new shared memory region. Delegates to platform-specific implementation.
    ///
    /// # Errors
    /// Returns an error if the platform shm creation fails.
    pub fn create(monitor_pid: u32) -> Result<Self, String> {
        crate::platform::macos::ffi::shm::create_shared_memory(monitor_pid)
    }

    /// The shm name (e.g., `/mbb_crash_12345`) for passing via environment variable.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Validate magic + version + canary.
    #[must_use]
    pub fn validate(&self) -> bool {
        let header = self.header();
        let magic = unsafe { ptr::read_volatile(&raw const (*header).magic) };
        let version = unsafe { ptr::read_volatile(&raw const (*header).version) };
        let canary = unsafe {
            #[allow(clippy::cast_ptr_alignment)] // footer offset is 4-byte aligned
            let ptr = self.base.add(FOOTER_OFFSET).cast::<u32>();
            ptr::read_volatile(ptr)
        };
        magic == SHM_MAGIC && version == SHM_VERSION && canary == SHM_CANARY
    }

    /// Read the heartbeat counter (atomic, safe to call while child is running).
    #[must_use]
    pub fn read_heartbeat(&self) -> u64 {
        let ctx = self.context_ptr();
        unsafe { ptr::read_volatile(&raw const (*ctx).heartbeat_counter) }
    }

    /// Read all breadcrumbs from all thread rings, sorted by timestamp.
    #[must_use]
    pub fn read_breadcrumbs(&self) -> Vec<RawBreadcrumb> {
        if !self.validate() {
            return Vec::new();
        }

        let crumb_state = self.crumb_state_ptr();
        let ring_count = unsafe { ptr::read_volatile(&raw const (*crumb_state).ring_count) };
        #[allow(clippy::cast_possible_truncation)] // CRUMB_MAX_THREADS=8, fits in u32
        let max = CRUMB_MAX_THREADS as u32;
        let ring_count = ring_count.min(max) as usize;

        let mut all_crumbs = Vec::new();

        for i in 0..ring_count {
            let ring = unsafe { &(*crumb_state).rings[i] };
            let count = unsafe { ptr::read_volatile(&raw const ring.count) } as usize;
            let write_idx = unsafe { ptr::read_volatile(&raw const ring.write_idx) } as usize;

            if count == 0 {
                continue;
            }

            // Number of valid entries: min(count, capacity)
            let valid_count = count.min(CRUMB_RING_CAPACITY);

            for j in 0..valid_count {
                // Read entries backwards from write_idx
                let slot = if write_idx > j {
                    write_idx - j - 1
                } else {
                    CRUMB_RING_CAPACITY + write_idx - j - 1
                } % CRUMB_RING_CAPACITY;

                let entry = unsafe { ptr::read_volatile(&raw const ring.buf[slot]) };

                // Skip incomplete entries (partially written at crash time)
                if entry.timestamp_ns == 0 {
                    continue;
                }

                all_crumbs.push(RawBreadcrumb {
                    timestamp_ns: entry.timestamp_ns,
                    thread_id: entry.thread_id,
                    category: entry.category,
                    severity: entry.severity,
                    file: c_array_to_string(&entry.file),
                    line: entry.line,
                    message: c_array_to_string(&entry.message),
                });
            }
        }

        // Sort by timestamp (merge from multiple threads)
        all_crumbs.sort_by_key(|c| c.timestamp_ns);
        all_crumbs
    }

    /// Read crash context from shared memory.
    #[must_use]
    pub fn read_context(&self) -> Option<RawCrashContext> {
        if !self.validate() {
            return None;
        }

        // Copy the entire struct at once via volatile read to prevent the compiler
        // from caching partial fields. The child is suspended, so the data is stable,
        // but volatile ensures we read from the actual mmap'd memory, not a stale cache.
        let ctx = self.context_ptr();
        let c: SutCrashContext = unsafe { ptr::read_volatile(ctx) };

        Some(convert_c_context(&c))
    }

    /// Read settings snapshot from shared memory.
    #[must_use]
    pub fn read_settings(&self) -> Option<RawSettingsSnapshot> {
        if !self.validate() {
            return None;
        }

        #[allow(clippy::cast_ptr_alignment)] // offset within mmap, aligned
        let settings_ptr = unsafe {
            self.base
                .add(SETTINGS_OFFSET)
                .cast::<SutCrashSettingsSnapshot>()
        };
        let s: SutCrashSettingsSnapshot = unsafe { ptr::read_volatile(settings_ptr) };
        Some(convert_c_settings(&s))
    }

    /// Read attachment paths from shared memory.
    #[must_use]
    pub fn read_attachments(&self) -> Vec<RawAttachment> {
        if !self.validate() {
            return Vec::new();
        }

        #[allow(clippy::cast_ptr_alignment)] // section offset is 4-byte aligned within mmap
        let section = unsafe {
            &*self
                .base
                .add(ATTACHMENT_OFFSET)
                .cast::<ShmAttachmentSection>()
        };
        let count = unsafe { ptr::read_volatile(&raw const section.count) } as usize;
        let count = count.min(4);

        let mut result = Vec::with_capacity(count);
        for i in 0..count {
            let slot = unsafe { ptr::read_volatile(&raw const section.slots[i]) };
            let label = c_array_to_string(&slot.label);
            let path = c_array_to_string(&slot.path);
            if !path.is_empty() {
                result.push(RawAttachment { label, path });
            }
        }
        result
    }

    /// Read valid screenshot slots from shared memory.
    /// Returns RGBA pixel data for each valid slot, sorted by timestamp (newest first).
    #[must_use]
    pub fn read_screenshots(&self) -> Vec<RawScreenshot> {
        if !self.validate() {
            return Vec::new();
        }

        // Section 4 is a single `sut_screenshot_section_t` (generated from
        // crash_shm.h); read fields by name instead of raw offset arithmetic.
        #[allow(clippy::cast_ptr_alignment)] // section offset is aligned within mmap
        let section = unsafe {
            &*self
                .base
                .add(SECTION4_OFFSET)
                .cast::<SutScreenshotSection>()
        };

        let mut screenshots = Vec::new();
        for i in 0..SCREENSHOT_SLOTS as usize {
            // valid[] is published by the producer with __atomic_store_n (release).
            let valid = unsafe { ptr::read_volatile(&raw const section.valid[i]) };
            if valid == 0 {
                continue;
            }
            let timestamp_ns = unsafe { ptr::read_volatile(&raw const section.timestamp[i]) };
            screenshots.push(RawScreenshot {
                timestamp_ns,
                width: SCREENSHOT_WIDTH,
                height: SCREENSHOT_HEIGHT,
                rgba: section.data[i].to_vec(),
            });
        }

        // Sort newest first
        screenshots.sort_by(|a, b| b.timestamp_ns.cmp(&a.timestamp_ns));
        screenshots
    }

    /// Raw pointer to breadcrumb data for Stage 1 raw dump.
    #[must_use]
    pub fn raw_breadcrumb_bytes(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.base.add(SECTION2_OFFSET), SECTION2_SIZE) }
    }

    /// Raw pointer to context data for Stage 1 raw dump.
    #[must_use]
    pub fn raw_context_bytes(&self) -> &[u8] {
        unsafe {
            std::slice::from_raw_parts(
                self.base.add(CONTEXT_OFFSET),
                size_of::<SutCrashContext>() + size_of::<SutCrashSettingsSnapshot>(),
            )
        }
    }

    /// Total shared memory size.
    #[must_use]
    pub fn total_size() -> usize {
        SHM_TOTAL_SIZE
    }

    /// Base pointer for test access (write breadcrumbs, corrupt magic/canary, etc.).
    ///
    /// # Safety
    /// Caller must not write out of bounds or create data races.
    #[must_use]
    pub fn base_ptr(&self) -> *mut u8 {
        self.base
    }

    // ── Private helpers ──

    fn header(&self) -> *const ShmHeader {
        unsafe { self.base.add(SECTION1_OFFSET).cast() }
    }

    fn crumb_state_ptr(&self) -> *const SutCrumbState {
        unsafe { self.base.add(SECTION2_OFFSET).cast() }
    }

    fn context_ptr(&self) -> *const SutCrashContext {
        unsafe { self.base.add(CONTEXT_OFFSET).cast() }
    }
}

#[cfg(test)]
#[path = "../../tests/unit/shm/reader_tests.rs"]
mod tests;
