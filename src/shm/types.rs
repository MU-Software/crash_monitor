//! Constants, section layout, and Rust-native types.
//!
//! The C<->Rust struct mirrors (`SutBreadcrumb`, `SutCrumbState`,
//! `SutCrashContext`, ...) are generated from `crash_shm.h` by `build.rs`
//! (bindgen) — see the `ffi` module. The compile-time size/offset assertions
//! below validate that generated layout against known values
//! (belt-and-suspenders with bindgen's own layout checks).

use std::mem::{align_of, offset_of, size_of};
use std::sync::atomic::{AtomicU32, AtomicU64};

/// Bindgen-generated C shm types. Non-idiomatic names are contained here and
/// re-exported with Rust-style aliases below.
#[allow(
    non_camel_case_types,
    non_snake_case,
    non_upper_case_globals,
    dead_code,
    clippy::all,
    clippy::pedantic
)]
mod ffi {
    include!(concat!(env!("OUT_DIR"), "/crash_shm_bindings.rs"));
}

// Rust-style aliases for the generated shm structs (layout SSOT = crash_shm.h).
pub use ffi::{
    sut_breadcrumb_t as SutBreadcrumb, sut_crash_annotation_t as SutCrashAnnotation,
    sut_crash_context_t as SutCrashContext,
    sut_crash_settings_snapshot_t as SutCrashSettingsSnapshot, sut_crumb_ring_t as SutCrumbRing,
    sut_crumb_state_t as SutCrumbState, sut_screenshot_section_t as SutScreenshotSection,
    sut_shm_attachment_section_t as ShmAttachmentSection,
    sut_shm_attachment_slot_t as ShmAttachmentSlot, sut_shm_header_t as ShmHeader,
};

// ═══════════════════════════════════════════════════
//  Constants
// ═══════════════════════════════════════════════════

pub const SHM_MAGIC: u32 = ffi::SUT_SHM_MAGIC;
pub const SHM_VERSION: u32 = ffi::SUT_SHM_VERSION;
pub const SHM_CANARY: u32 = ffi::SUT_SHM_CANARY;

// Schema-derived from crash_shm.h #defines (via bindgen).
pub const CRUMB_RING_CAPACITY: usize = ffi::SUT_CRUMB_RING_CAPACITY as usize;
pub const CRUMB_MAX_THREADS: usize = ffi::SUT_CRUMB_MAX_THREADS as usize;
pub const CRUMB_CATEGORY_MAX: u16 = checked_wire_u16(ffi::SUT_CRUMB_CATEGORY_MAX);
pub const CRUMB_SEVERITY_INFO: u16 = checked_wire_u16(ffi::SUT_CRUMB_SEV_INFO);
pub const CRUMB_SEVERITY_WARN: u16 = checked_wire_u16(ffi::SUT_CRUMB_SEV_WARN);
pub const CRUMB_SEVERITY_ERROR: u16 = checked_wire_u16(ffi::SUT_CRUMB_SEV_ERROR);
pub const CRUMB_SEVERITY_MAX: u16 = checked_wire_u16(ffi::SUT_CRUMB_SEVERITY_MAX);
pub const MAX_ANNOTATIONS: usize = ffi::SUT_CRASH_MAX_ANNOTATIONS as usize;
pub const MAX_ATTACHMENTS: usize = ffi::SUT_SHM_MAX_ATTACHMENTS as usize;
pub const SCREENSHOT_SLOTS: u32 = ffi::SUT_SCREENSHOT_SLOTS;
pub const SCREENSHOT_WIDTH: u32 = ffi::SUT_SCREENSHOT_WIDTH;
pub const SCREENSHOT_HEIGHT: u32 = ffi::SUT_SCREENSHOT_HEIGHT;
pub const SCREENSHOT_BYTES_PER_SLOT: usize =
    (SCREENSHOT_WIDTH as usize) * (SCREENSHOT_HEIGHT as usize) * 4;

const fn checked_wire_u16(value: u32) -> u16 {
    assert!(value <= 65_535);
    #[allow(clippy::cast_possible_truncation)] // guarded by the const assertion above
    {
        value as u16
    }
}

const _: () = assert!(CRUMB_SEVERITY_INFO == 0);
const _: () = assert!(CRUMB_SEVERITY_WARN == 1);
const _: () = assert!(CRUMB_SEVERITY_ERROR == CRUMB_SEVERITY_MAX);

// ═══════════════════════════════════════════════════
//  Compile-time layout assertions (validate the bindgen output)
// ═══════════════════════════════════════════════════

const _: () = assert!(size_of::<AtomicU32>() == size_of::<u32>());
const _: () = assert!(align_of::<AtomicU32>() == align_of::<u32>());
const _: () = assert!(size_of::<AtomicU64>() == size_of::<u64>());
const _: () = assert!(align_of::<AtomicU64>() == align_of::<u64>());
const _: () = assert!(size_of::<ShmHeader>() == 64);
const _: () = assert!(offset_of!(ShmHeader, breadcrumb_registry_generation) == 16);
const _: () = assert!(offset_of!(ShmHeader, context_generation) == 32);
const _: () = assert!(offset_of!(ShmHeader, settings_generation) == 36);
const _: () = assert!(offset_of!(ShmHeader, attachments_generation) == 40);
const _: () = assert!(size_of::<SutBreadcrumb>() == 64);
const _: () = assert!(size_of::<SutCrumbRing>() == 32784);
const _: () = assert!(offset_of!(SutCrumbRing, generation) == 32780);
const _: () = assert!(size_of::<SutCrumbState>() == 262_280);
const _: () = assert!(offset_of!(SutCrumbState, ring_count) == 262_272);
const _: () = assert!(size_of::<SutCrashAnnotation>() == 96);
const _: () = assert!(size_of::<SutCrashContext>() == 1760);
const _: fn(SutCrashContext) -> u8 = |context| context.git_dirty;
const _: () = assert!(size_of::<SutCrashSettingsSnapshot>() == 160);
const _: () = assert!(size_of::<ShmAttachmentSlot>() == 288);
const _: () = assert!(size_of::<ShmAttachmentSection>() == 1160);
const _: () = assert!(offset_of!(ShmAttachmentSection, slots) == 8);
const _: () = assert!(size_of::<SutScreenshotSection>() == 49_767_936);
const _: () = assert!(offset_of!(SutScreenshotSection, valid) == 0);
const _: () = assert!(offset_of!(SutScreenshotSection, timestamp) == 384);
const _: () = assert!(offset_of!(SutScreenshotSection, tier) == 1152);
const _: () = assert!(offset_of!(SutScreenshotSection, data) == 1536);

// Offset assertions for SutCrashContext (app-agnostic layout)
const _: () = assert!(offset_of!(SutCrashContext, heartbeat_counter) == 0);
const _: () = assert!(offset_of!(SutCrashContext, session_start_ns) == 8);
const _: () = assert!(offset_of!(SutCrashContext, session_id) == 16);
const _: () = assert!(offset_of!(SutCrashContext, app_version) == 53);
const _: () = assert!(offset_of!(SutCrashContext, git_dirty) == 92);
const _: () = assert!(offset_of!(SutCrashContext, os_version) == 181);
const _: () = assert!(offset_of!(SutCrashContext, annotation_count) == 216);
const _: () = assert!(offset_of!(SutCrashContext, annotations) == 220);

// ═══════════════════════════════════════════════════
//  Section layout (computed from struct sizes)
// ═══════════════════════════════════════════════════

/// Section 1: Header
pub const SECTION1_OFFSET: usize = 0;
pub const SECTION1_SIZE: usize = size_of::<ShmHeader>();

/// Section 2: Breadcrumb rings
pub const SECTION2_OFFSET: usize = SECTION1_OFFSET + SECTION1_SIZE; // 64
pub const SECTION2_SIZE: usize = size_of::<SutCrumbState>();

/// Section 3: Crash context + settings + heartbeat + attachments
pub const SECTION3_OFFSET: usize = SECTION2_OFFSET + SECTION2_SIZE;
pub const CONTEXT_OFFSET: usize = SECTION3_OFFSET;
pub const SETTINGS_OFFSET: usize = CONTEXT_OFFSET + size_of::<SutCrashContext>();
pub const ATTACHMENT_OFFSET: usize = SETTINGS_OFFSET + size_of::<SutCrashSettingsSnapshot>();
pub const SECTION3_SIZE: usize = size_of::<SutCrashContext>()
    + size_of::<SutCrashSettingsSnapshot>()
    + size_of::<ShmAttachmentSection>();

/// Section 4: Screenshot ring buffer
pub const SECTION4_OFFSET: usize = SECTION3_OFFSET + SECTION3_SIZE;
pub const SCREENSHOT_META_SIZE: usize = 96 * 4 + 96 * 8 + 96 * 4; // valid + timestamp + tier
pub const SCREENSHOT_DATA_SIZE: usize = 96 * SCREENSHOT_BYTES_PER_SLOT;
pub const SECTION4_SIZE: usize = SCREENSHOT_META_SIZE + SCREENSHOT_DATA_SIZE;

/// Section 5: Footer (canary)
pub const FOOTER_OFFSET: usize = SECTION4_OFFSET + SECTION4_SIZE;

/// Total shared memory size, fixed by the C ABI schema.
pub const SHM_TOTAL_SIZE: usize = ffi::SUT_SHM_TOTAL_SIZE as usize;

const _: () = assert!(SECTION2_OFFSET == 64);
const _: () = assert!(CONTEXT_OFFSET == 262_344);
const _: () = assert!(SETTINGS_OFFSET == 264_104);
const _: () = assert!(ATTACHMENT_OFFSET == 264_264);
const _: () = assert!(SECTION4_OFFSET == 265_424);
const _: () = assert!(FOOTER_OFFSET == 50_033_360);
const _: () = assert!(SHM_TOTAL_SIZE == FOOTER_OFFSET + size_of::<u32>());
const _: () = assert!(SHM_TOTAL_SIZE == 50_033_364);

// ═══════════════════════════════════════════════════
//  Rust-native types (read from shm)
// ═══════════════════════════════════════════════════

/// A breadcrumb entry converted to Rust-native types.
#[derive(Debug, Clone)]
pub struct RawBreadcrumb {
    pub timestamp_ns: u64,
    pub thread_id: u32,
    pub category: u16,
    pub severity: u16,
    pub file: String,
    pub line: u16,
    pub message: String,
}

/// Crash context converted to Rust-native types.
///
/// App/domain state lives in `annotations` (generic key-value); the struct
/// carries no app-specific fields, keeping the monitor app-agnostic.
#[derive(Debug, Clone)]
pub struct RawCrashContext {
    pub heartbeat_counter: u64,
    pub session_start_ns: u64,
    pub session_id: String,
    pub annotations: Vec<(String, String)>,
    pub app_version: String,
    pub build_number: u32,
    pub git_hash: String,
    pub git_dirty: bool,
    pub build_type: String,
    pub build_preset: String,
    pub build_timestamp: String,
    pub compiler: String,
    pub os_version: String,
}

/// Settings snapshot converted to Rust-native types.
#[derive(Debug, Clone)]
pub struct RawSettingsSnapshot {
    pub world_bound_min: [i32; 3],
    pub world_bound_max: [i32; 3],
    pub palette_count: i32,
    pub history_max: i32,
    pub extra: String,
}

/// An attachment file registered by the C app.
#[derive(Debug, Clone)]
pub struct RawAttachment {
    pub label: String,
    pub path: String,
}

/// A screenshot frame read from shared memory.
pub struct RawScreenshot {
    pub timestamp_ns: u64,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}
