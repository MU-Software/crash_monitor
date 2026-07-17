//! Constants, section layout, and Rust-native types.
//!
//! The C<->Rust struct mirrors (`SutBreadcrumb`, `SutCrumbState`,
//! `SutCrashContext`, ...) are generated from `crash_shm.h` by `build.rs`
//! (bindgen) — see the `ffi` module. The compile-time size/offset assertions
//! below validate that generated layout against known values
//! (belt-and-suspenders with bindgen's own layout checks).

use std::mem::{offset_of, size_of};
use std::os::raw::c_char;
use std::sync::atomic::AtomicU32;

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
    sut_breadcrumb_t as SutBreadcrumb, sut_crash_context_t as SutCrashContext,
    sut_crash_settings_snapshot_t as SutCrashSettingsSnapshot, sut_crumb_ring_t as SutCrumbRing,
    sut_crumb_state_t as SutCrumbState, sut_screenshot_section_t as SutScreenshotSection,
};

// ═══════════════════════════════════════════════════
//  Constants
// ═══════════════════════════════════════════════════

pub const SHM_MAGIC: u32 = 0x434D_4F4E; // "CMON" (Crash MONitor)
pub const SHM_VERSION: u32 = 1;
pub const SHM_CANARY: u32 = 0xDEAD_BEEF;

// Schema-derived from crash_shm.h #defines (via bindgen).
pub const CRUMB_RING_CAPACITY: usize = ffi::SUT_CRUMB_RING_CAPACITY as usize;
pub const CRUMB_MAX_THREADS: usize = ffi::SUT_CRUMB_MAX_THREADS as usize;
pub const SCREENSHOT_SLOTS: u32 = ffi::SUT_SCREENSHOT_SLOTS;
pub const SCREENSHOT_WIDTH: u32 = ffi::SUT_SCREENSHOT_WIDTH;
pub const SCREENSHOT_HEIGHT: u32 = ffi::SUT_SCREENSHOT_HEIGHT;
pub const SCREENSHOT_BYTES_PER_SLOT: usize =
    (SCREENSHOT_WIDTH as usize) * (SCREENSHOT_HEIGHT as usize) * 4;

// ═══════════════════════════════════════════════════
//  Rust-only / monitor-owned mirrors (not part of crash_shm.h)
// ═══════════════════════════════════════════════════

/// Shared memory header — Section 1 (64 bytes). Written by the monitor
/// (magic/canary); the producer only skips past it. Monitor-owned, so it is
/// not part of the C schema.
#[repr(C)]
pub struct ShmHeader {
    pub magic: u32,
    pub version: u32,
    pub ring_capacity_per_thread: u32,
    pub max_threads: u32,
    pub ring_count: AtomicU32,
    pub screenshot_slots: u32,
    pub screenshot_width: u32,
    pub screenshot_height: u32,
    reserved: [u8; 32],
}

/// Attachment slot in shared memory (mirrors the C producer's struct in
/// `sut_crash_reporter.c`; not yet migrated into `crash_shm.h`).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct ShmAttachmentSlot {
    pub label: [c_char; 32],
    pub path: [c_char; 256],
}

/// Attachment section in shared memory.
#[repr(C)]
pub struct ShmAttachmentSection {
    pub count: u32,
    pad: [u8; 4], // align to 8
    pub slots: [ShmAttachmentSlot; 4],
}

// ═══════════════════════════════════════════════════
//  Compile-time layout assertions (validate the bindgen output)
// ═══════════════════════════════════════════════════

const _: () = assert!(size_of::<ShmHeader>() == 64);
const _: () = assert!(size_of::<SutBreadcrumb>() == 64);
const _: () = assert!(size_of::<SutCrumbRing>() == 32784);
const _: () = assert!(size_of::<SutCrumbState>() == 262_280);
const _: () = assert!(size_of::<SutCrashContext>() == 824);
const _: () = assert!(size_of::<SutCrashSettingsSnapshot>() == 160);
const _: () = assert!(size_of::<ShmAttachmentSlot>() == 288);

// Offset assertions for SutCrashContext
const _: () = assert!(offset_of!(SutCrashContext, heartbeat_counter) == 88);
const _: () = assert!(offset_of!(SutCrashContext, session_start_ns) == 96);
const _: () = assert!(offset_of!(SutCrashContext, session_id) == 104);
const _: () = assert!(offset_of!(SutCrashContext, tags) == 141);
const _: () = assert!(offset_of!(SutCrashContext, tag_count) == 656);
const _: () = assert!(offset_of!(SutCrashContext, app_version) == 660);
const _: () = assert!(offset_of!(SutCrashContext, git_dirty) == 696);
const _: () = assert!(offset_of!(SutCrashContext, os_version) == 785);

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

/// Total shared memory size
pub const SHM_TOTAL_SIZE: usize = FOOTER_OFFSET + 4; // canary u32

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
#[derive(Debug, Clone)]
pub struct RawCrashContext {
    pub active_tool: String,
    pub region_count: i32,
    pub voxel_count: i32,
    pub undo_depth: i32,
    pub redo_depth: i32,
    pub last_action_id: u32,
    pub frame_number: u32,
    pub alloc_count: u64,
    pub free_count: u64,
    pub alloc_bytes_total: u64,
    pub thread_pool_size: i32,
    pub active_batch: i32,
    pub heartbeat_counter: u64,
    pub session_start_ns: u64,
    pub session_id: String,
    pub tags: Vec<(String, String)>,
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

// ═══════════════════════════════════════════════════
//  Helpers
// ═══════════════════════════════════════════════════

/// Convert a fixed-size C char array to a Rust String (NUL-terminated).
///
/// bindgen renders C `char[]` as `c_char` (i8); reinterpret as bytes since the
/// buffer holds NUL-terminated (mostly ASCII) text.
pub(crate) fn c_array_to_string(bytes: &[c_char]) -> String {
    // SAFETY: c_char and u8 have identical size/alignment; we only read.
    let bytes: &[u8] =
        unsafe { std::slice::from_raw_parts(bytes.as_ptr().cast::<u8>(), bytes.len()) };
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// Read tags from the fixed [4][2][64] C array.
pub(crate) fn read_tags(tags: &[[[c_char; 64]; 2]; 4], count: i32) -> Vec<(String, String)> {
    #[allow(clippy::cast_sign_loss)] // clamped to 0..=4
    let count = count.clamp(0, 4) as usize;
    (0..count)
        .map(|i| {
            (
                c_array_to_string(&tags[i][0]),
                c_array_to_string(&tags[i][1]),
            )
        })
        .collect()
}

#[cfg(test)]
#[path = "../../tests/unit/shm/types_tests.rs"]
mod tests;
