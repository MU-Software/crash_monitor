//! `SharedMemory` handle and immutable, owned snapshots of its payload.

use std::fmt;
use std::mem::{offset_of, size_of};
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

#[allow(clippy::wildcard_imports)] // re-export convenience — all types are public
use super::types::*;

const SNAPSHOT_COPY_CHUNK_SIZE: usize = 1024 * 1024;

const BREADCRUMB_FILE_LEN: usize = 16;
const BREADCRUMB_MESSAGE_LEN: usize = 28;
const CONTEXT_SESSION_ID_LEN: usize = 37;
const CONTEXT_APP_VERSION_LEN: usize = 16;
const CONTEXT_GIT_HASH_LEN: usize = 16;
const CONTEXT_BUILD_TYPE_LEN: usize = 16;
const CONTEXT_BUILD_PRESET_LEN: usize = 16;
const CONTEXT_BUILD_TIMESTAMP_LEN: usize = 24;
const CONTEXT_COMPILER_LEN: usize = 32;
const CONTEXT_OS_VERSION_LEN: usize = 32;
const ANNOTATION_KEY_LEN: usize = 32;
const ANNOTATION_VALUE_LEN: usize = 64;
const SETTINGS_EXTRA_LEN: usize = 128;
const ATTACHMENT_LABEL_LEN: usize = 32;
const ATTACHMENT_PATH_LEN: usize = 256;
const ATTACHMENT_SLOT_COUNT: usize = 4;

/// Failure to copy or validate an immutable shared-memory snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShmSnapshotError {
    /// The mapped object is shorter than the schema requires.
    MappingTooSmall { mapped: usize, required: usize },
    /// The caller's absolute snapshot deadline elapsed.
    DeadlineExceeded,
    /// Allocating the bounded owned destination failed.
    AllocationFailed { requested: usize },
    /// The copied header did not contain the expected magic value.
    InvalidMagic { found: u32 },
    /// The copied header used an unsupported schema version.
    UnsupportedVersion { found: u32 },
    /// The copied footer did not contain the expected canary.
    InvalidCanary { found: u32 },
}

impl fmt::Display for ShmSnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MappingTooSmall { mapped, required } => {
                write!(
                    f,
                    "shared-memory mapping is too small: {mapped} bytes, need {required}"
                )
            }
            Self::DeadlineExceeded => f.write_str("shared-memory snapshot deadline exceeded"),
            Self::AllocationFailed { requested } => {
                write!(
                    f,
                    "failed to allocate {requested} bytes for shared-memory snapshot"
                )
            }
            Self::InvalidMagic { found } => {
                write!(f, "invalid shared-memory magic: {found:#010x}")
            }
            Self::UnsupportedVersion { found } => {
                write!(f, "unsupported shared-memory version: {found}")
            }
            Self::InvalidCanary { found } => {
                write!(f, "invalid shared-memory canary: {found:#010x}")
            }
        }
    }
}

impl std::error::Error for ShmSnapshotError {}

/// Immutable bytes copied from the complete shared-memory mapping.
///
/// No method on this type reads the live mapping. Parsing uses checked byte
/// ranges and primitive integer decoding, so untrusted wire bytes are never
/// materialized as a bindgen-generated Rust struct.
pub struct OwnedShmSnapshot {
    bytes: Vec<u8>,
}

impl fmt::Debug for OwnedShmSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OwnedShmSnapshot")
            .field("len", &self.bytes.len())
            .finish_non_exhaustive()
    }
}

impl OwnedShmSnapshot {
    fn from_owned_bytes(bytes: Vec<u8>) -> Result<Self, ShmSnapshotError> {
        let snapshot = Self { bytes };
        snapshot.validate()?;
        Ok(snapshot)
    }

    fn validate(&self) -> Result<(), ShmSnapshotError> {
        let magic = self.read_u32(SECTION1_OFFSET).unwrap_or_default();
        if magic != SHM_MAGIC {
            return Err(ShmSnapshotError::InvalidMagic { found: magic });
        }

        let version = self
            .read_u32(SECTION1_OFFSET + size_of::<u32>())
            .unwrap_or_default();
        if version != SHM_VERSION {
            return Err(ShmSnapshotError::UnsupportedVersion { found: version });
        }

        let canary = self.read_u32(FOOTER_OFFSET).unwrap_or_default();
        if canary != SHM_CANARY {
            return Err(ShmSnapshotError::InvalidCanary { found: canary });
        }

        Ok(())
    }

    /// Read all breadcrumbs from all thread rings, sorted by timestamp.
    #[must_use]
    pub fn read_breadcrumbs(&self) -> Vec<RawBreadcrumb> {
        let Some(ring_count_offset) =
            SECTION2_OFFSET.checked_add(offset_of!(SutCrumbState, ring_count))
        else {
            return Vec::new();
        };
        let Some(ring_count) = self.read_u32(ring_count_offset) else {
            return Vec::new();
        };
        #[allow(clippy::cast_possible_truncation)] // CRUMB_MAX_THREADS is schema-bounded
        let ring_count = ring_count.min(CRUMB_MAX_THREADS as u32) as usize;

        let mut breadcrumbs = Vec::new();
        for ring_index in 0..ring_count {
            let Some(rings_offset) = SECTION2_OFFSET.checked_add(offset_of!(SutCrumbState, rings))
            else {
                break;
            };
            let Some(ring_offset) =
                indexed_offset(rings_offset, ring_index, size_of::<SutCrumbRing>())
            else {
                break;
            };
            let Some(count_offset) = ring_offset.checked_add(offset_of!(SutCrumbRing, count))
            else {
                break;
            };
            let Some(write_idx_offset) =
                ring_offset.checked_add(offset_of!(SutCrumbRing, write_idx))
            else {
                break;
            };
            let (Some(count), Some(write_idx)) =
                (self.read_u32(count_offset), self.read_u32(write_idx_offset))
            else {
                break;
            };
            let valid_count = usize::try_from(count)
                .unwrap_or(usize::MAX)
                .min(CRUMB_RING_CAPACITY);
            let write_idx = usize::try_from(write_idx).unwrap_or(usize::MAX);

            for reverse_index in 0..valid_count {
                let slot = if write_idx > reverse_index {
                    write_idx - reverse_index - 1
                } else {
                    CRUMB_RING_CAPACITY + write_idx - reverse_index - 1
                } % CRUMB_RING_CAPACITY;
                let Some(buffer_offset) = ring_offset.checked_add(offset_of!(SutCrumbRing, buf))
                else {
                    continue;
                };
                let Some(entry_offset) =
                    indexed_offset(buffer_offset, slot, size_of::<SutBreadcrumb>())
                else {
                    continue;
                };
                let Some(entry) = self.read_breadcrumb(entry_offset) else {
                    continue;
                };
                if entry.timestamp_ns != 0 {
                    breadcrumbs.push(entry);
                }
            }
        }

        breadcrumbs.sort_by_key(|breadcrumb| breadcrumb.timestamp_ns);
        breadcrumbs
    }

    /// Read crash context from this immutable snapshot.
    #[must_use]
    pub fn read_context(&self) -> Option<RawCrashContext> {
        let field = |offset| CONTEXT_OFFSET.checked_add(offset);
        let heartbeat_counter =
            self.read_u64(field(offset_of!(SutCrashContext, heartbeat_counter))?)?;
        let session_start_ns =
            self.read_u64(field(offset_of!(SutCrashContext, session_start_ns))?)?;
        let session_id = self.read_string(
            field(offset_of!(SutCrashContext, session_id))?,
            CONTEXT_SESSION_ID_LEN,
        )?;
        let app_version = self.read_string(
            field(offset_of!(SutCrashContext, app_version))?,
            CONTEXT_APP_VERSION_LEN,
        )?;
        let build_number = self.read_u32(field(offset_of!(SutCrashContext, build_number))?)?;
        let git_hash = self.read_string(
            field(offset_of!(SutCrashContext, git_hash))?,
            CONTEXT_GIT_HASH_LEN,
        )?;
        // Never construct the bindgen `bool`: every wire byte is decoded first.
        let git_dirty = self.read_u8(field(offset_of!(SutCrashContext, git_dirty))?)? != 0;
        let build_type = self.read_string(
            field(offset_of!(SutCrashContext, build_type))?,
            CONTEXT_BUILD_TYPE_LEN,
        )?;
        let build_preset = self.read_string(
            field(offset_of!(SutCrashContext, build_preset))?,
            CONTEXT_BUILD_PRESET_LEN,
        )?;
        let build_timestamp = self.read_string(
            field(offset_of!(SutCrashContext, build_timestamp))?,
            CONTEXT_BUILD_TIMESTAMP_LEN,
        )?;
        let compiler = self.read_string(
            field(offset_of!(SutCrashContext, compiler))?,
            CONTEXT_COMPILER_LEN,
        )?;
        let os_version = self.read_string(
            field(offset_of!(SutCrashContext, os_version))?,
            CONTEXT_OS_VERSION_LEN,
        )?;

        let annotation_count =
            self.read_i32(field(offset_of!(SutCrashContext, annotation_count))?)?;
        let annotation_count = usize::try_from(annotation_count)
            .unwrap_or(0)
            .min(MAX_ANNOTATIONS);
        let annotations_offset = field(offset_of!(SutCrashContext, annotations))?;
        let mut annotations = Vec::with_capacity(annotation_count);
        for index in 0..annotation_count {
            let annotation_offset =
                indexed_offset(annotations_offset, index, size_of::<SutCrashAnnotation>())?;
            let key = self.read_string(
                annotation_offset.checked_add(offset_of!(SutCrashAnnotation, key))?,
                ANNOTATION_KEY_LEN,
            )?;
            let value = self.read_string(
                annotation_offset.checked_add(offset_of!(SutCrashAnnotation, value))?,
                ANNOTATION_VALUE_LEN,
            )?;
            annotations.push((key, value));
        }

        Some(RawCrashContext {
            heartbeat_counter,
            session_start_ns,
            session_id,
            annotations,
            app_version,
            build_number,
            git_hash,
            git_dirty,
            build_type,
            build_preset,
            build_timestamp,
            compiler,
            os_version,
        })
    }

    /// Read settings from this immutable snapshot.
    #[must_use]
    pub fn read_settings(&self) -> Option<RawSettingsSnapshot> {
        let field = |offset| SETTINGS_OFFSET.checked_add(offset);
        let world_min = field(offset_of!(SutCrashSettingsSnapshot, world_bound_min))?;
        let world_max = field(offset_of!(SutCrashSettingsSnapshot, world_bound_max))?;
        Some(RawSettingsSnapshot {
            world_bound_min: self.read_i32_triplet(world_min)?,
            world_bound_max: self.read_i32_triplet(world_max)?,
            palette_count: self
                .read_i32(field(offset_of!(SutCrashSettingsSnapshot, palette_count))?)?,
            history_max: self
                .read_i32(field(offset_of!(SutCrashSettingsSnapshot, history_max))?)?,
            extra: self.read_string(
                field(offset_of!(SutCrashSettingsSnapshot, extra))?,
                SETTINGS_EXTRA_LEN,
            )?,
        })
    }

    /// Read registered attachment paths from this immutable snapshot.
    #[must_use]
    pub fn read_attachments(&self) -> Vec<RawAttachment> {
        let Some(count_offset) =
            ATTACHMENT_OFFSET.checked_add(offset_of!(ShmAttachmentSection, count))
        else {
            return Vec::new();
        };
        let Some(count) = self.read_u32(count_offset) else {
            return Vec::new();
        };
        let count = usize::try_from(count)
            .unwrap_or(usize::MAX)
            .min(ATTACHMENT_SLOT_COUNT);
        let Some(slots_offset) =
            ATTACHMENT_OFFSET.checked_add(offset_of!(ShmAttachmentSection, slots))
        else {
            return Vec::new();
        };
        let mut attachments = Vec::with_capacity(count);
        for index in 0..count {
            let Some(slot_offset) =
                indexed_offset(slots_offset, index, size_of::<ShmAttachmentSlot>())
            else {
                break;
            };
            let Some(label_offset) = slot_offset.checked_add(offset_of!(ShmAttachmentSlot, label))
            else {
                break;
            };
            let Some(path_offset) = slot_offset.checked_add(offset_of!(ShmAttachmentSlot, path))
            else {
                break;
            };
            let (Some(label), Some(path)) = (
                self.read_string(label_offset, ATTACHMENT_LABEL_LEN),
                self.read_string(path_offset, ATTACHMENT_PATH_LEN),
            ) else {
                break;
            };
            if !path.is_empty() {
                attachments.push(RawAttachment { label, path });
            }
        }
        attachments
    }

    /// Read valid screenshot slots, sorted by timestamp (newest first).
    #[must_use]
    pub fn read_screenshots(&self) -> Vec<RawScreenshot> {
        let (Some(valid_offset), Some(timestamp_offset), Some(data_offset)) = (
            SECTION4_OFFSET.checked_add(offset_of!(SutScreenshotSection, valid)),
            SECTION4_OFFSET.checked_add(offset_of!(SutScreenshotSection, timestamp)),
            SECTION4_OFFSET.checked_add(offset_of!(SutScreenshotSection, data)),
        ) else {
            return Vec::new();
        };
        let mut screenshots = Vec::new();

        for index in 0..SCREENSHOT_SLOTS as usize {
            let Some(valid) = indexed_offset(valid_offset, index, size_of::<u32>())
                .and_then(|offset| self.read_u32(offset))
            else {
                break;
            };
            if valid == 0 {
                continue;
            }
            let Some(timestamp_ns) = indexed_offset(timestamp_offset, index, size_of::<u64>())
                .and_then(|offset| self.read_u64(offset))
            else {
                continue;
            };
            let Some(rgba) = indexed_offset(data_offset, index, SCREENSHOT_BYTES_PER_SLOT)
                .and_then(|offset| self.range(offset, SCREENSHOT_BYTES_PER_SLOT))
                .map(<[u8]>::to_vec)
            else {
                continue;
            };
            screenshots.push(RawScreenshot {
                timestamp_ns,
                width: SCREENSHOT_WIDTH,
                height: SCREENSHOT_HEIGHT,
                rgba,
            });
        }

        screenshots.sort_by(|a, b| b.timestamp_ns.cmp(&a.timestamp_ns));
        screenshots
    }

    /// Return an owned copy of the breadcrumb section for Stage 1 persistence.
    #[must_use]
    pub fn raw_breadcrumb_bytes_owned(&self) -> Vec<u8> {
        self.range(SECTION2_OFFSET, SECTION2_SIZE)
            .map_or_else(Vec::new, <[u8]>::to_vec)
    }

    /// Return an owned copy of context + settings for Stage 1 persistence.
    #[must_use]
    pub fn raw_context_bytes_owned(&self) -> Vec<u8> {
        let len = size_of::<SutCrashContext>() + size_of::<SutCrashSettingsSnapshot>();
        self.range(CONTEXT_OFFSET, len)
            .map_or_else(Vec::new, <[u8]>::to_vec)
    }

    fn read_breadcrumb(&self, base: usize) -> Option<RawBreadcrumb> {
        Some(RawBreadcrumb {
            timestamp_ns: self
                .read_u64(base.checked_add(offset_of!(SutBreadcrumb, timestamp_ns))?)?,
            thread_id: self.read_u32(base.checked_add(offset_of!(SutBreadcrumb, thread_id))?)?,
            category: self.read_u16(base.checked_add(offset_of!(SutBreadcrumb, category))?)?,
            severity: self.read_u16(base.checked_add(offset_of!(SutBreadcrumb, severity))?)?,
            file: self.read_string(
                base.checked_add(offset_of!(SutBreadcrumb, file))?,
                BREADCRUMB_FILE_LEN,
            )?,
            line: self.read_u16(base.checked_add(offset_of!(SutBreadcrumb, line))?)?,
            message: self.read_string(
                base.checked_add(offset_of!(SutBreadcrumb, message))?,
                BREADCRUMB_MESSAGE_LEN,
            )?,
        })
    }

    fn range(&self, offset: usize, len: usize) -> Option<&[u8]> {
        let end = offset.checked_add(len)?;
        self.bytes.get(offset..end)
    }

    fn read_array<const N: usize>(&self, offset: usize) -> Option<[u8; N]> {
        self.range(offset, N)?.try_into().ok()
    }

    fn read_u8(&self, offset: usize) -> Option<u8> {
        self.bytes.get(offset).copied()
    }

    fn read_u16(&self, offset: usize) -> Option<u16> {
        Some(u16::from_ne_bytes(self.read_array(offset)?))
    }

    fn read_u32(&self, offset: usize) -> Option<u32> {
        Some(u32::from_ne_bytes(self.read_array(offset)?))
    }

    fn read_u64(&self, offset: usize) -> Option<u64> {
        Some(u64::from_ne_bytes(self.read_array(offset)?))
    }

    fn read_i32(&self, offset: usize) -> Option<i32> {
        Some(i32::from_ne_bytes(self.read_array(offset)?))
    }

    fn read_i32_triplet(&self, offset: usize) -> Option<[i32; 3]> {
        Some([
            self.read_i32(offset)?,
            self.read_i32(offset.checked_add(size_of::<i32>())?)?,
            self.read_i32(offset.checked_add(size_of::<i32>() * 2)?)?,
        ])
    }

    fn read_string(&self, offset: usize, len: usize) -> Option<String> {
        let bytes = self.range(offset, len)?;
        let end = bytes.iter().position(|byte| *byte == 0).unwrap_or(len);
        Some(String::from_utf8_lossy(&bytes[..end]).into_owned())
    }
}

fn indexed_offset(base: usize, index: usize, stride: usize) -> Option<usize> {
    base.checked_add(index.checked_mul(stride)?)
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

// SAFETY: payload access never creates borrowed references into the mapping.
// Complete payloads are copied through bounded raw pointers while the caller
// owns task suspension. The one running-child observation is an aligned atomic
// heartbeat load.
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

    /// The shm name (e.g., `/crash_monitor_12345`) for passing via environment variable.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Copy the complete mapping into immutable owned bytes before `deadline`.
    ///
    /// The caller must invoke this while it owns target-process suspension. A
    /// deadline is checked at every bounded copy chunk; no borrowed reference
    /// or slice is ever formed over the live mapping.
    ///
    /// # Errors
    /// Returns an error for a short mapping, elapsed deadline, or invalid copied
    /// header/footer metadata.
    pub fn snapshot_owned_until(
        &self,
        deadline: Option<Instant>,
    ) -> Result<OwnedShmSnapshot, ShmSnapshotError> {
        if self.size < SHM_TOTAL_SIZE {
            return Err(ShmSnapshotError::MappingTooSmall {
                mapped: self.size,
                required: SHM_TOTAL_SIZE,
            });
        }
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(ShmSnapshotError::DeadlineExceeded);
        }
        let mut bytes = Vec::<u8>::new();
        bytes.try_reserve_exact(SHM_TOTAL_SIZE).map_err(|_| {
            ShmSnapshotError::AllocationFailed {
                requested: SHM_TOTAL_SIZE,
            }
        })?;
        while bytes.len() < SHM_TOTAL_SIZE {
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Err(ShmSnapshotError::DeadlineExceeded);
            }
            let copied = bytes.len();
            let chunk_len = (SHM_TOTAL_SIZE - copied).min(SNAPSHOT_COPY_CHUNK_SIZE);
            // SAFETY: `self.size >= SHM_TOTAL_SIZE` was checked before pointer
            // arithmetic. `try_reserve_exact` established destination capacity
            // for the full schema, and `copied + chunk_len <= SHM_TOTAL_SIZE`.
            // The source and destination cannot overlap. The new length is
            // published only after every byte in this chunk has been initialized.
            unsafe {
                ptr::copy_nonoverlapping(
                    self.base.cast_const().add(copied),
                    bytes.as_mut_ptr().add(copied),
                    chunk_len,
                );
                bytes.set_len(copied + chunk_len);
            }
        }

        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(ShmSnapshotError::DeadlineExceeded);
        }
        let snapshot = OwnedShmSnapshot::from_owned_bytes(bytes)?;
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(ShmSnapshotError::DeadlineExceeded);
        }
        Ok(snapshot)
    }

    /// Read the heartbeat counter using an aligned acquire atomic load.
    ///
    /// This is the only API that observes the mapping while the child runs.
    #[must_use]
    pub fn read_live_heartbeat(&self) -> u64 {
        const HEARTBEAT_OFFSET: usize =
            CONTEXT_OFFSET + offset_of!(SutCrashContext, heartbeat_counter);
        const _: () = assert!(HEARTBEAT_OFFSET.is_multiple_of(align_of::<AtomicU64>()));
        debug_assert!(self.size >= HEARTBEAT_OFFSET + size_of::<AtomicU64>());

        // SAFETY: the mapping is live for `self`; the compile-time assertion
        // establishes alignment and creation always maps the full schema. The C
        // producer accesses this exact ABI word atomically.
        #[allow(clippy::cast_ptr_alignment)] // proven by the const assertion above
        let heartbeat = unsafe { &*self.base.add(HEARTBEAT_OFFSET).cast::<AtomicU64>() };
        heartbeat.load(Ordering::Acquire)
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
}

#[cfg(test)]
#[path = "../../tests/unit/shm/reader_tests.rs"]
mod tests;
