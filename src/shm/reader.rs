//! `SharedMemory` handle and immutable, owned snapshots of its payload.

use std::fmt;
use std::mem::{align_of, offset_of, size_of};
use std::ptr;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering, fence};
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
const ATTACHMENT_SLOT_COUNT: usize = MAX_ATTACHMENTS;

const REGISTRY_GENERATION_OFFSET: usize =
    SECTION1_OFFSET + offset_of!(ShmHeader, breadcrumb_registry_generation);
const CONTEXT_GENERATION_OFFSET: usize =
    SECTION1_OFFSET + offset_of!(ShmHeader, context_generation);
const SETTINGS_GENERATION_OFFSET: usize =
    SECTION1_OFFSET + offset_of!(ShmHeader, settings_generation);
const ATTACHMENTS_GENERATION_OFFSET: usize =
    SECTION1_OFFSET + offset_of!(ShmHeader, attachments_generation);
const PRODUCER_READY_OFFSET: usize = SECTION1_OFFSET + offset_of!(ShmHeader, producer_ready);
const RING_COUNT_OFFSET: usize = SECTION2_OFFSET + offset_of!(SutCrumbState, ring_count);
const RINGS_OFFSET: usize = SECTION2_OFFSET + offset_of!(SutCrumbState, rings);
const RING_GENERATION_OFFSET: usize = offset_of!(SutCrumbRing, generation);
const SCREENSHOT_GENERATIONS_OFFSET: usize =
    SECTION4_OFFSET + offset_of!(SutScreenshotSection, valid);
const SCREENSHOT_TIMESTAMPS_OFFSET: usize =
    SECTION4_OFFSET + offset_of!(SutScreenshotSection, timestamp);
const SCREENSHOT_TIERS_OFFSET: usize = SECTION4_OFFSET + offset_of!(SutScreenshotSection, tier);
const SCREENSHOT_DATA_OFFSET: usize = SECTION4_OFFSET + offset_of!(SutScreenshotSection, data);
const HEARTBEAT_OFFSET: usize = CONTEXT_OFFSET + offset_of!(SutCrashContext, heartbeat_counter);

#[derive(Clone, Copy)]
enum LiveAtomicWord {
    U32(usize),
    U64(usize),
}

impl LiveAtomicWord {
    const fn offset(self) -> usize {
        match self {
            Self::U32(offset) | Self::U64(offset) => offset,
        }
    }

    const fn width(self) -> usize {
        match self {
            Self::U32(_) => size_of::<u32>(),
            Self::U64(_) => size_of::<u64>(),
        }
    }
}

const LIVE_ATOMIC_WORD_COUNT: usize = 5 + CRUMB_MAX_THREADS + 1 + 1 + SCREENSHOT_SLOTS as usize;

const fn live_atomic_words() -> [LiveAtomicWord; LIVE_ATOMIC_WORD_COUNT] {
    let mut words = [LiveAtomicWord::U32(REGISTRY_GENERATION_OFFSET); LIVE_ATOMIC_WORD_COUNT];
    let mut index = 0;

    words[index] = LiveAtomicWord::U32(REGISTRY_GENERATION_OFFSET);
    index += 1;
    words[index] = LiveAtomicWord::U32(CONTEXT_GENERATION_OFFSET);
    index += 1;
    words[index] = LiveAtomicWord::U32(SETTINGS_GENERATION_OFFSET);
    index += 1;
    words[index] = LiveAtomicWord::U32(ATTACHMENTS_GENERATION_OFFSET);
    index += 1;
    words[index] = LiveAtomicWord::U32(PRODUCER_READY_OFFSET);
    index += 1;

    let mut ring_index = 0;
    while ring_index < CRUMB_MAX_THREADS {
        words[index] = LiveAtomicWord::U32(
            RINGS_OFFSET + ring_index * size_of::<SutCrumbRing>() + RING_GENERATION_OFFSET,
        );
        index += 1;
        ring_index += 1;
    }

    words[index] = LiveAtomicWord::U32(RING_COUNT_OFFSET);
    index += 1;
    words[index] = LiveAtomicWord::U64(HEARTBEAT_OFFSET);
    index += 1;

    let mut screenshot_index = 0;
    while screenshot_index < SCREENSHOT_SLOTS as usize {
        words[index] = LiveAtomicWord::U32(
            SCREENSHOT_GENERATIONS_OFFSET + screenshot_index * size_of::<u32>(),
        );
        index += 1;
        screenshot_index += 1;
    }

    words
}

const LIVE_ATOMIC_WORDS: [LiveAtomicWord; LIVE_ATOMIC_WORD_COUNT] = live_atomic_words();

const _: () = assert!(REGISTRY_GENERATION_OFFSET.is_multiple_of(align_of::<AtomicU32>()));
const _: () = assert!(CONTEXT_GENERATION_OFFSET.is_multiple_of(align_of::<AtomicU32>()));
const _: () = assert!(SETTINGS_GENERATION_OFFSET.is_multiple_of(align_of::<AtomicU32>()));
const _: () = assert!(ATTACHMENTS_GENERATION_OFFSET.is_multiple_of(align_of::<AtomicU32>()));
const _: () = assert!(PRODUCER_READY_OFFSET.is_multiple_of(align_of::<AtomicU32>()));
const _: () = assert!(RING_COUNT_OFFSET.is_multiple_of(align_of::<AtomicU32>()));
const _: () =
    assert!((RINGS_OFFSET + RING_GENERATION_OFFSET).is_multiple_of(align_of::<AtomicU32>()));
const _: () = assert!(size_of::<SutCrumbRing>().is_multiple_of(align_of::<AtomicU32>()));
const _: () = assert!(SCREENSHOT_GENERATIONS_OFFSET.is_multiple_of(align_of::<AtomicU32>()));
const _: () = assert!(HEARTBEAT_OFFSET.is_multiple_of(align_of::<AtomicU64>()));
const _: () = {
    let mut index = 0;
    let mut previous_end = 0;
    while index < LIVE_ATOMIC_WORDS.len() {
        let word = LIVE_ATOMIC_WORDS[index];
        assert!(word.offset() >= previous_end);
        assert!(word.offset() + word.width() <= SHM_TOTAL_SIZE);
        previous_end = word.offset() + word.width();
        index += 1;
    }
};

/// Immutable selection of SHM payload sections that capture may copy.
///
/// Header/footer schema metadata is always copied for validation. A disabled
/// payload section remains zero-filled in the owned snapshot and is never read
/// from the live mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // one bit per independently published section
pub struct ShmSnapshotPolicy {
    pub breadcrumbs: bool,
    pub context: bool,
    pub attachments: bool,
    pub screenshots: bool,
}

impl ShmSnapshotPolicy {
    pub const ALL: Self = Self {
        breadcrumbs: true,
        context: true,
        attachments: true,
        screenshots: true,
    };

    fn allows_atomic(self, offset: usize) -> bool {
        if offset == REGISTRY_GENERATION_OFFSET || offset == RING_COUNT_OFFSET {
            return self.breadcrumbs;
        }
        if offset == CONTEXT_GENERATION_OFFSET
            || offset == SETTINGS_GENERATION_OFFSET
            || offset == HEARTBEAT_OFFSET
        {
            return self.context;
        }
        if offset == ATTACHMENTS_GENERATION_OFFSET {
            return self.attachments;
        }
        if offset == PRODUCER_READY_OFFSET {
            return true;
        }
        if (RINGS_OFFSET..SECTION2_OFFSET + SECTION2_SIZE).contains(&offset) {
            return self.breadcrumbs;
        }
        if (SCREENSHOT_GENERATIONS_OFFSET..SCREENSHOT_TIMESTAMPS_OFFSET).contains(&offset) {
            return self.screenshots;
        }
        false
    }
}

impl Default for ShmSnapshotPolicy {
    fn default() -> Self {
        Self::ALL
    }
}

/// Failure to validate the live mapping or copied SHM schema metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShmValidationError {
    /// The mapped object is shorter than the schema requires.
    MappingTooSmall { mapped: usize, required: usize },
    /// The live mapping base does not satisfy the generated region alignment.
    MisalignedMapping { address: usize, required: usize },
    /// The copied header did not contain the expected magic value.
    InvalidMagic { found: u32 },
    /// The copied header used an unsupported schema version.
    UnsupportedVersion { found: u32 },
    /// The copied footer did not contain the expected canary.
    InvalidCanary { found: u32 },
}

impl fmt::Display for ShmValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MappingTooSmall { mapped, required } => {
                write!(
                    f,
                    "shared-memory mapping is too small: {mapped} bytes, need {required}"
                )
            }
            Self::MisalignedMapping { address, required } => {
                write!(
                    f,
                    "misaligned shared-memory mapping at {address:#x}: need {required}-byte alignment"
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

impl std::error::Error for ShmValidationError {}

/// Failure to copy or validate an immutable shared-memory snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShmSnapshotError {
    /// The live mapping or copied schema metadata failed validation.
    Validation(ShmValidationError),
    /// The caller's absolute snapshot deadline elapsed.
    DeadlineExceeded,
    /// Allocating the bounded owned destination failed.
    AllocationFailed { requested: usize },
}

impl From<ShmValidationError> for ShmSnapshotError {
    fn from(error: ShmValidationError) -> Self {
        Self::Validation(error)
    }
}

impl fmt::Display for ShmSnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Validation(error) => error.fmt(f),
            Self::DeadlineExceeded => f.write_str("shared-memory snapshot deadline exceeded"),
            Self::AllocationFailed { requested } => write!(
                f,
                "failed to allocate {requested} bytes for shared-memory snapshot"
            ),
        }
    }
}

impl std::error::Error for ShmSnapshotError {}

fn validate_mapping_shape(address: usize, mapped: usize) -> Result<(), ShmValidationError> {
    if mapped < SHM_TOTAL_SIZE {
        return Err(ShmValidationError::MappingTooSmall {
            mapped,
            required: SHM_TOTAL_SIZE,
        });
    }
    let required = align_of::<SutShmRegion>();
    if !address.is_multiple_of(required) {
        return Err(ShmValidationError::MisalignedMapping { address, required });
    }
    Ok(())
}

/// A publication unit that could not be proven stable during a snapshot.
///
/// The affected bytes are sanitized in the owned snapshot while unrelated
/// publication units remain available to collectors and Stage 1 persistence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ShmConsistencyIssue {
    BreadcrumbRegistry {
        generation_before: u32,
        generation_after: u32,
        ring_count_before: u32,
        ring_count_after: u32,
    },
    BreadcrumbRing {
        index: usize,
        generation_before: u32,
        generation_after: u32,
    },
    Context {
        generation_before: u32,
        generation_after: u32,
    },
    Settings {
        generation_before: u32,
        generation_after: u32,
    },
    Attachments {
        generation_before: u32,
        generation_after: u32,
    },
    ScreenshotSlot {
        index: usize,
        generation_before: u32,
        generation_after: u32,
    },
}

/// Immutable, fixed-layout bytes copied from policy-authorized SHM sections.
///
/// No method on this type reads the live mapping. Parsing uses checked byte
/// ranges and primitive integer decoding, so untrusted wire bytes are never
/// materialized as a bindgen-generated Rust struct.
pub struct OwnedShmSnapshot {
    bytes: Vec<u8>,
    consistency_issues: Vec<ShmConsistencyIssue>,
}

impl fmt::Debug for OwnedShmSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OwnedShmSnapshot")
            .field("len", &self.bytes.len())
            .field("consistency_issues", &self.consistency_issues)
            .finish_non_exhaustive()
    }
}

impl OwnedShmSnapshot {
    fn from_owned_bytes(
        bytes: Vec<u8>,
        consistency_issues: Vec<ShmConsistencyIssue>,
    ) -> Result<Self, ShmSnapshotError> {
        let snapshot = Self {
            bytes,
            consistency_issues,
        };
        snapshot.validate()?;
        Ok(snapshot)
    }

    /// Publication units rejected because their generation changed or was odd.
    #[must_use]
    pub fn consistency_issues(&self) -> &[ShmConsistencyIssue] {
        &self.consistency_issues
    }

    fn validate(&self) -> Result<(), ShmValidationError> {
        let magic = self.read_u32(SECTION1_OFFSET).unwrap_or_default();
        if magic != SHM_MAGIC {
            return Err(ShmValidationError::InvalidMagic { found: magic });
        }

        let version = self
            .read_u32(SECTION1_OFFSET + size_of::<u32>())
            .unwrap_or_default();
        if version != SHM_VERSION {
            return Err(ShmValidationError::UnsupportedVersion { found: version });
        }

        let canary = self.read_u32(FOOTER_OFFSET).unwrap_or_default();
        if canary != SHM_CANARY {
            return Err(ShmValidationError::InvalidCanary { found: canary });
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
        let Ok(ring_count) = usize::try_from(ring_count) else {
            return Vec::new();
        };
        if ring_count > CRUMB_MAX_THREADS {
            return Vec::new();
        }

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
        if self
            .consistency_issues
            .iter()
            .any(|issue| matches!(issue, ShmConsistencyIssue::Context { .. }))
        {
            return None;
        }
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
        // Keep the untrusted wire value integer until this explicit conversion.
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
        let annotation_count = usize::try_from(annotation_count).ok()?;
        if annotation_count > MAX_ANNOTATIONS {
            return None;
        }
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
        if self
            .consistency_issues
            .iter()
            .any(|issue| matches!(issue, ShmConsistencyIssue::Settings { .. }))
        {
            return None;
        }
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
        if self
            .consistency_issues
            .iter()
            .any(|issue| matches!(issue, ShmConsistencyIssue::Attachments { .. }))
        {
            return Vec::new();
        }
        let Some(count_offset) =
            ATTACHMENT_OFFSET.checked_add(offset_of!(ShmAttachmentSection, count))
        else {
            return Vec::new();
        };
        let Some(count) = self.read_u32(count_offset) else {
            return Vec::new();
        };
        let Ok(count) = usize::try_from(count) else {
            return Vec::new();
        };
        if count > ATTACHMENT_SLOT_COUNT {
            return Vec::new();
        }
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
                continue;
            };
            if !path.is_empty() {
                attachments.push(RawAttachment { label, path });
            }
        }
        attachments
    }

    /// Read valid screenshot slots in capture priority order.
    ///
    /// Lower numeric tiers have higher priority. Frames in the same tier are
    /// ordered newest first, with slot order providing a deterministic tie.
    #[must_use]
    pub fn read_screenshots(&self) -> Vec<RawScreenshot> {
        let (Some(valid_offset), Some(timestamp_offset), Some(tier_offset), Some(data_offset)) = (
            SECTION4_OFFSET.checked_add(offset_of!(SutScreenshotSection, valid)),
            SECTION4_OFFSET.checked_add(offset_of!(SutScreenshotSection, timestamp)),
            SECTION4_OFFSET.checked_add(offset_of!(SutScreenshotSection, tier)),
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
            // `valid` is a per-slot generation: zero is empty, odd is being
            // written, and a non-zero even value is published.
            if valid == 0 || !valid.is_multiple_of(2) {
                continue;
            }
            let Some(timestamp_ns) = indexed_offset(timestamp_offset, index, size_of::<u64>())
                .and_then(|offset| self.read_u64(offset))
            else {
                continue;
            };
            let Some(tier) = indexed_offset(tier_offset, index, size_of::<u32>())
                .and_then(|offset| self.read_u32(offset))
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
                tier,
                width: SCREENSHOT_WIDTH,
                height: SCREENSHOT_HEIGHT,
                rgba,
            });
        }

        screenshots.sort_by(|a, b| {
            a.tier
                .cmp(&b.tier)
                .then_with(|| b.timestamp_ns.cmp(&a.timestamp_ns))
        });
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
        let category = self.read_u16(base.checked_add(offset_of!(SutBreadcrumb, category))?)?;
        let severity = self.read_u16(base.checked_add(offset_of!(SutBreadcrumb, severity))?)?;
        if category > CRUMB_CATEGORY_MAX || severity > CRUMB_SEVERITY_MAX {
            return None;
        }

        Some(RawBreadcrumb {
            timestamp_ns: self
                .read_u64(base.checked_add(offset_of!(SutBreadcrumb, timestamp_ns))?)?,
            thread_id: self.read_u32(base.checked_add(offset_of!(SutBreadcrumb, thread_id))?)?,
            category,
            severity,
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
        decode_bounded_c_string(self.range(offset, len)?)
    }
}

fn decode_bounded_c_string(bytes: &[u8]) -> Option<String> {
    let end = bytes.iter().position(|byte| *byte == 0)?;
    let value = std::str::from_utf8(&bytes[..end]).ok()?;
    if value.chars().any(char::is_control) {
        return None;
    }
    Some(value.to_owned())
}

fn indexed_offset(base: usize, index: usize, stride: usize) -> Option<usize> {
    base.checked_add(index.checked_mul(stride)?)
}

fn check_snapshot_deadline(deadline: Option<Instant>) -> Result<(), ShmSnapshotError> {
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        Err(ShmSnapshotError::DeadlineExceeded)
    } else {
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct LivePublicationState {
    registry_generation: u32,
    ring_count: u32,
    ring_generations: [u32; CRUMB_MAX_THREADS],
    context_generation: u32,
    settings_generation: u32,
    attachments_generation: u32,
    producer_ready: u32,
    screenshot_generations: [u32; SCREENSHOT_SLOTS as usize],
}

impl Default for LivePublicationState {
    fn default() -> Self {
        Self {
            registry_generation: 0,
            ring_count: 0,
            ring_generations: [0; CRUMB_MAX_THREADS],
            context_generation: 0,
            settings_generation: 0,
            attachments_generation: 0,
            producer_ready: SHM_PRODUCER_NOT_READY,
            screenshot_generations: [0; SCREENSHOT_SLOTS as usize],
        }
    }
}

const fn stable_generation(before: u32, after: u32) -> bool {
    before == after && before.is_multiple_of(2)
}

fn zero_owned_range(bytes: &mut [u8], offset: usize, len: usize) {
    let Some(end) = offset.checked_add(len) else {
        return;
    };
    if let Some(range) = bytes.get_mut(offset..end) {
        range.fill(0);
    }
}

fn write_owned_u32(bytes: &mut [u8], offset: usize, value: u32) {
    let Some(end) = offset.checked_add(size_of::<u32>()) else {
        return;
    };
    if let Some(range) = bytes.get_mut(offset..end) {
        range.copy_from_slice(&value.to_ne_bytes());
    }
}

fn write_owned_u64(bytes: &mut [u8], offset: usize, value: u64) {
    let Some(end) = offset.checked_add(size_of::<u64>()) else {
        return;
    };
    if let Some(range) = bytes.get_mut(offset..end) {
        range.copy_from_slice(&value.to_ne_bytes());
    }
}

fn sanitize_breadcrumbs(
    bytes: &mut [u8],
    before: &LivePublicationState,
    after: &LivePublicationState,
    issues: &mut Vec<ShmConsistencyIssue>,
) {
    let registry_is_stable =
        stable_generation(before.registry_generation, after.registry_generation)
            && before.ring_count == after.ring_count;
    if registry_is_stable {
        write_owned_u32(bytes, RING_COUNT_OFFSET, after.ring_count);
        // Stage 1 persists the complete fixed ring array, so inactive slots
        // must be checked too while registration is in progress.
        for index in 0..CRUMB_MAX_THREADS {
            let generation_before = before.ring_generations[index];
            let generation_after = after.ring_generations[index];
            let Some(ring_offset) = indexed_offset(RINGS_OFFSET, index, size_of::<SutCrumbRing>())
            else {
                continue;
            };
            if stable_generation(generation_before, generation_after) {
                if let Some(generation_offset) = ring_offset.checked_add(RING_GENERATION_OFFSET) {
                    write_owned_u32(bytes, generation_offset, generation_after);
                }
            } else {
                zero_owned_range(bytes, ring_offset, size_of::<SutCrumbRing>());
                issues.push(ShmConsistencyIssue::BreadcrumbRing {
                    index,
                    generation_before,
                    generation_after,
                });
            }
        }
    } else {
        zero_owned_range(bytes, SECTION2_OFFSET, SECTION2_SIZE);
        issues.push(ShmConsistencyIssue::BreadcrumbRegistry {
            generation_before: before.registry_generation,
            generation_after: after.registry_generation,
            ring_count_before: before.ring_count,
            ring_count_after: after.ring_count,
        });
    }
}

fn sanitize_context_settings_attachments(
    bytes: &mut [u8],
    before: &LivePublicationState,
    after: &LivePublicationState,
    policy: ShmSnapshotPolicy,
    issues: &mut Vec<ShmConsistencyIssue>,
) {
    if policy.context && !stable_generation(before.context_generation, after.context_generation) {
        zero_owned_range(bytes, CONTEXT_OFFSET, size_of::<SutCrashContext>());
        issues.push(ShmConsistencyIssue::Context {
            generation_before: before.context_generation,
            generation_after: after.context_generation,
        });
    }

    if policy.context && !stable_generation(before.settings_generation, after.settings_generation) {
        zero_owned_range(
            bytes,
            SETTINGS_OFFSET,
            size_of::<SutCrashSettingsSnapshot>(),
        );
        issues.push(ShmConsistencyIssue::Settings {
            generation_before: before.settings_generation,
            generation_after: after.settings_generation,
        });
    }

    if policy.attachments
        && !stable_generation(before.attachments_generation, after.attachments_generation)
    {
        zero_owned_range(bytes, ATTACHMENT_OFFSET, size_of::<ShmAttachmentSection>());
        issues.push(ShmConsistencyIssue::Attachments {
            generation_before: before.attachments_generation,
            generation_after: after.attachments_generation,
        });
    }
}

fn sanitize_screenshots(
    bytes: &mut [u8],
    before: &LivePublicationState,
    after: &LivePublicationState,
    issues: &mut Vec<ShmConsistencyIssue>,
    deadline: Option<Instant>,
) -> Result<(), ShmSnapshotError> {
    for index in 0..SCREENSHOT_SLOTS as usize {
        check_snapshot_deadline(deadline)?;
        let generation_before = before.screenshot_generations[index];
        let generation_after = after.screenshot_generations[index];
        let Some(generation_offset) =
            indexed_offset(SCREENSHOT_GENERATIONS_OFFSET, index, size_of::<u32>())
        else {
            continue;
        };
        if stable_generation(generation_before, generation_after) {
            write_owned_u32(bytes, generation_offset, generation_after);
        } else {
            write_owned_u32(bytes, generation_offset, 0);
            if let Some(offset) =
                indexed_offset(SCREENSHOT_TIMESTAMPS_OFFSET, index, size_of::<u64>())
            {
                zero_owned_range(bytes, offset, size_of::<u64>());
            }
            if let Some(offset) = indexed_offset(SCREENSHOT_TIERS_OFFSET, index, size_of::<u32>()) {
                zero_owned_range(bytes, offset, size_of::<u32>());
            }
            if let Some(offset) =
                indexed_offset(SCREENSHOT_DATA_OFFSET, index, SCREENSHOT_BYTES_PER_SLOT)
            {
                zero_owned_range(bytes, offset, SCREENSHOT_BYTES_PER_SLOT);
            }
            issues.push(ShmConsistencyIssue::ScreenshotSlot {
                index,
                generation_before,
                generation_after,
            });
        }
    }
    check_snapshot_deadline(deadline)
}

fn sanitize_publications(
    bytes: &mut [u8],
    before: &LivePublicationState,
    after: &LivePublicationState,
    heartbeat: u64,
    deadline: Option<Instant>,
    policy: ShmSnapshotPolicy,
) -> Result<Vec<ShmConsistencyIssue>, ShmSnapshotError> {
    let mut issues = Vec::new();

    // Keep the copied header's publication words aligned with the acquire
    // observations used to approve or reject each payload unit.
    if policy.breadcrumbs {
        write_owned_u32(bytes, REGISTRY_GENERATION_OFFSET, after.registry_generation);
    }
    if policy.context {
        write_owned_u32(bytes, CONTEXT_GENERATION_OFFSET, after.context_generation);
        write_owned_u32(bytes, SETTINGS_GENERATION_OFFSET, after.settings_generation);
    }
    if policy.attachments {
        write_owned_u32(
            bytes,
            ATTACHMENTS_GENERATION_OFFSET,
            after.attachments_generation,
        );
    }
    write_owned_u32(bytes, PRODUCER_READY_OFFSET, after.producer_ready);

    if policy.breadcrumbs {
        check_snapshot_deadline(deadline)?;
        sanitize_breadcrumbs(bytes, before, after, &mut issues);
    }
    check_snapshot_deadline(deadline)?;
    sanitize_context_settings_attachments(bytes, before, after, policy, &mut issues);
    if policy.screenshots {
        check_snapshot_deadline(deadline)?;
        sanitize_screenshots(bytes, before, after, &mut issues, deadline)?;
    }

    // Heartbeat publication is independent from the context seqlock. Always
    // replace the raw-copy bytes with one aligned acquire load taken after the
    // copy, including when the rest of context was rejected and zeroed.
    if policy.context {
        write_owned_u64(bytes, HEARTBEAT_OFFSET, heartbeat);
    }

    Ok(issues)
}

// ═══════════════════════════════════════════════════
//  SharedMemory handle
// ═══════════════════════════════════════════════════

/// High-level reader over a low-level mapping owner.
///
/// The platform FFI layer owns mmap/unmap/unlink. This type owns that handle
/// and contains only schema validation, atomic observation, and snapshot logic.
pub struct SharedMemory {
    mapping: crate::platform::macos::ffi::shm::ShmMapping,
}

impl SharedMemory {
    fn load_atomic_u32_at(&self, offset: usize) -> Option<u32> {
        let end = offset.checked_add(size_of::<AtomicU32>())?;
        if end > self.mapping.len() || !offset.is_multiple_of(align_of::<AtomicU32>()) {
            return None;
        }

        // SAFETY: the checked range lies inside the live mapping. mmap returns
        // page-aligned storage and the offset alignment is checked above. The
        // producer accesses every publication word through the matching C
        // atomic ABI; this atomic reference is the intentional exception to
        // the no-borrowed-references rule for live shared memory.
        #[allow(clippy::cast_ptr_alignment)] // checked immediately above
        let value = unsafe { &*self.mapping.base_ptr().add(offset).cast::<AtomicU32>() };
        Some(value.load(Ordering::Acquire))
    }

    fn load_atomic_u64_at(&self, offset: usize) -> Option<u64> {
        let end = offset.checked_add(size_of::<AtomicU64>())?;
        if end > self.mapping.len() || !offset.is_multiple_of(align_of::<AtomicU64>()) {
            return None;
        }

        // SAFETY: see `load_atomic_u32_at`; this is the aligned 64-bit atomic
        // form used by the independently published heartbeat counter.
        #[allow(clippy::cast_ptr_alignment)] // checked immediately above
        let value = unsafe { &*self.mapping.base_ptr().add(offset).cast::<AtomicU64>() };
        Some(value.load(Ordering::Acquire))
    }

    fn copy_non_atomic_range(
        &self,
        bytes: &mut [u8],
        start: usize,
        end: usize,
        deadline: Option<Instant>,
    ) -> Result<(), ShmSnapshotError> {
        debug_assert!(start <= end);
        debug_assert!(end <= bytes.len());
        let mut copied = start;
        while copied < end {
            check_snapshot_deadline(deadline)?;
            let chunk_len = (end - copied).min(SNAPSHOT_COPY_CHUNK_SIZE);
            // SAFETY: the caller checked `self.size >= SHM_TOTAL_SIZE` and
            // initialized a destination with that complete size. Atomic
            // publication words delimit every range passed to this helper, and
            // policy-disabled sections never call it.
            unsafe {
                ptr::copy_nonoverlapping(
                    self.mapping.base_ptr().cast_const().add(copied),
                    bytes.as_mut_ptr().add(copied),
                    chunk_len,
                );
            }
            copied += chunk_len;
        }
        Ok(())
    }

    fn copy_selected_non_atomic_range(
        &self,
        bytes: &mut [u8],
        start: usize,
        end: usize,
        deadline: Option<Instant>,
        policy: ShmSnapshotPolicy,
    ) -> Result<(), ShmSnapshotError> {
        let sections = [
            (SECTION1_OFFSET, SECTION1_OFFSET + SECTION1_SIZE, true),
            (
                SECTION2_OFFSET,
                SECTION2_OFFSET + SECTION2_SIZE,
                policy.breadcrumbs,
            ),
            (CONTEXT_OFFSET, ATTACHMENT_OFFSET, policy.context),
            (ATTACHMENT_OFFSET, SECTION4_OFFSET, policy.attachments),
            (SECTION4_OFFSET, FOOTER_OFFSET, policy.screenshots),
            (FOOTER_OFFSET, SHM_TOTAL_SIZE, true),
        ];
        for (section_start, section_end, enabled) in sections {
            if !enabled {
                continue;
            }
            let copy_start = start.max(section_start);
            let copy_end = end.min(section_end);
            if copy_start < copy_end {
                self.copy_non_atomic_range(bytes, copy_start, copy_end, deadline)?;
            }
        }
        Ok(())
    }

    fn copy_atomic_word(&self, bytes: &mut [u8], word: LiveAtomicWord) {
        let offset = word.offset();
        debug_assert!(offset + word.width() <= bytes.len());

        match word {
            LiveAtomicWord::U32(offset) => {
                let encoded = self
                    .load_atomic_u32_at(offset)
                    .unwrap_or_default()
                    .to_ne_bytes();
                bytes[offset..offset + encoded.len()].copy_from_slice(&encoded);
            }
            LiveAtomicWord::U64(offset) => {
                let encoded = self
                    .load_atomic_u64_at(offset)
                    .unwrap_or_default()
                    .to_ne_bytes();
                bytes[offset..offset + encoded.len()].copy_from_slice(&encoded);
            }
        }
    }

    fn load_publication_state(&self, policy: ShmSnapshotPolicy) -> LivePublicationState {
        let load_u32 = |enabled: bool, offset: usize| {
            if enabled {
                self.load_atomic_u32_at(offset).unwrap_or_default()
            } else {
                0
            }
        };
        let mut ring_generations = [0; CRUMB_MAX_THREADS];
        if policy.breadcrumbs {
            for (index, generation) in ring_generations.iter_mut().enumerate() {
                let offset = indexed_offset(RINGS_OFFSET, index, size_of::<SutCrumbRing>())
                    .and_then(|ring| ring.checked_add(RING_GENERATION_OFFSET));
                if let Some(offset) = offset {
                    *generation = self.load_atomic_u32_at(offset).unwrap_or_default();
                }
            }
        }

        let mut screenshot_generations = [0; SCREENSHOT_SLOTS as usize];
        if policy.screenshots {
            for (index, generation) in screenshot_generations.iter_mut().enumerate() {
                if let Some(offset) =
                    indexed_offset(SCREENSHOT_GENERATIONS_OFFSET, index, size_of::<u32>())
                {
                    *generation = self.load_atomic_u32_at(offset).unwrap_or_default();
                }
            }
        }

        LivePublicationState {
            registry_generation: load_u32(policy.breadcrumbs, REGISTRY_GENERATION_OFFSET),
            ring_count: load_u32(policy.breadcrumbs, RING_COUNT_OFFSET),
            ring_generations,
            context_generation: load_u32(policy.context, CONTEXT_GENERATION_OFFSET),
            settings_generation: load_u32(policy.context, SETTINGS_GENERATION_OFFSET),
            attachments_generation: load_u32(policy.attachments, ATTACHMENTS_GENERATION_OFFSET),
            producer_ready: self
                .load_atomic_u32_at(PRODUCER_READY_OFFSET)
                .unwrap_or(SHM_PRODUCER_NOT_READY),
            screenshot_generations,
        }
    }

    /// Create a new shared memory region. Delegates to platform-specific implementation.
    ///
    /// # Errors
    /// Returns an error if the platform shm creation fails.
    pub fn create(monitor_pid: u32) -> Result<Self, String> {
        crate::platform::macos::ffi::shm::create_shared_memory(monitor_pid)
            .map(|mapping| Self { mapping })
    }

    /// The shm name (e.g., `/crash_monitor_12345`) for passing via environment variable.
    #[must_use]
    pub fn name(&self) -> &str {
        self.mapping.name()
    }

    /// Copy every payload section into immutable owned bytes before `deadline`.
    ///
    /// The caller must invoke this while it owns target-process suspension. A
    /// deadline is checked at every bounded copy chunk and screenshot
    /// sanitization slot; no ordinary borrowed reference or slice is ever
    /// formed over the live mapping.
    ///
    /// # Errors
    /// Returns an error for a short mapping, elapsed deadline, or invalid copied
    /// header/footer metadata.
    pub fn snapshot_owned_until(
        &self,
        deadline: Option<Instant>,
    ) -> Result<OwnedShmSnapshot, ShmSnapshotError> {
        self.snapshot_owned_until_with_policy(deadline, ShmSnapshotPolicy::ALL)
    }

    /// Copy only policy-authorized payload sections before `deadline`.
    ///
    /// The returned value retains the fixed ABI length so existing checked
    /// parsers remain valid. Disabled sections are initialized to zero and are
    /// never copied or acquire-loaded from the live mapping.
    ///
    /// # Errors
    /// Returns an error for a short mapping, elapsed deadline, allocation
    /// failure, or invalid copied header/footer metadata.
    pub fn snapshot_owned_until_with_policy(
        &self,
        deadline: Option<Instant>,
        policy: ShmSnapshotPolicy,
    ) -> Result<OwnedShmSnapshot, ShmSnapshotError> {
        validate_mapping_shape(self.mapping.base_ptr().addr(), self.mapping.len())?;
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(ShmSnapshotError::DeadlineExceeded);
        }
        let mut bytes = Vec::<u8>::new();
        bytes.try_reserve_exact(SHM_TOTAL_SIZE).map_err(|_| {
            ShmSnapshotError::AllocationFailed {
                requested: SHM_TOTAL_SIZE,
            }
        })?;
        bytes.resize(SHM_TOTAL_SIZE, 0);

        let publication_before = self.load_publication_state(policy);
        let mut cursor = 0;
        for word in LIVE_ATOMIC_WORDS {
            self.copy_selected_non_atomic_range(
                &mut bytes,
                cursor,
                word.offset(),
                deadline,
                policy,
            )?;
            check_snapshot_deadline(deadline)?;
            if policy.allows_atomic(word.offset()) {
                self.copy_atomic_word(&mut bytes, word);
            }
            cursor = word.offset() + word.width();
        }
        self.copy_selected_non_atomic_range(&mut bytes, cursor, SHM_TOTAL_SIZE, deadline, policy)?;

        // Keep every payload read ordered before the closing generation loads.
        // Each acquire-loaded generation is compared with the corresponding
        // pre-copy value below; there is deliberately no retry or spin.
        fence(Ordering::SeqCst);
        let publication_after = self.load_publication_state(policy);
        let heartbeat = if policy.context {
            self.load_atomic_u64_at(HEARTBEAT_OFFSET)
                .unwrap_or_default()
        } else {
            0
        };

        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(ShmSnapshotError::DeadlineExceeded);
        }
        let consistency_issues = sanitize_publications(
            &mut bytes,
            &publication_before,
            &publication_after,
            heartbeat,
            deadline,
            policy,
        )?;
        let snapshot = OwnedShmSnapshot::from_owned_bytes(bytes, consistency_issues)?;
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(ShmSnapshotError::DeadlineExceeded);
        }
        Ok(snapshot)
    }

    /// Acquire the producer's ANR opt-in handshake and its heartbeat baseline.
    ///
    /// `producer_ready` is loaded first. Only the exact schema value `1` arms
    /// ANR monitoring; every other untrusted value returns `None` without
    /// treating zero-filled or malformed shared memory as a stalled producer.
    /// The ready acquire synchronizes with the producer's ready release, making
    /// the earlier initial-heartbeat store happen-before this observation. The
    /// heartbeat itself is then acquire-loaded independently.
    #[must_use]
    pub fn read_live_anr_heartbeat(&self) -> Option<u64> {
        let ready = self.load_atomic_u32_at(PRODUCER_READY_OFFSET)?;
        if ready != SHM_PRODUCER_READY {
            return None;
        }
        self.load_atomic_u64_at(HEARTBEAT_OFFSET)
    }

    /// Read the heartbeat counter using an aligned acquire atomic load.
    ///
    /// This raw heartbeat helper is retained for schema round-trip tests. ANR
    /// monitoring must use [`Self::read_live_anr_heartbeat`] so producer
    /// readiness is acquired before the heartbeat.
    #[must_use]
    pub fn read_live_heartbeat(&self) -> u64 {
        self.load_atomic_u64_at(HEARTBEAT_OFFSET)
            .unwrap_or_default()
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
        self.mapping.base_ptr()
    }

    #[cfg(test)]
    fn set_mapped_size_for_test(&mut self, size: usize) {
        self.mapping.set_len_for_test(size);
    }
}

#[cfg(test)]
#[path = "../../tests/unit/shm/reader_tests.rs"]
mod tests;
