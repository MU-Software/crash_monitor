//! POSIX shared memory bridge between C app and Rust crash monitor.
//!
//! The monitor creates a shared memory region before spawning the child.
//! The child maps it and writes breadcrumbs, crash context, and screenshots.
//! On crash/snapshot, the monitor copies only privacy-authorized sections into
//! an immutable owned snapshot while the child is suspended.
//! Payload parsers and explicitly enabled Stage 1 persistence consume only
//! those owned bytes after that boundary;
//! only publication words plus the watchdog readiness/heartbeat handshake are
//! read from the live mapping, always through aligned acquire atomics.
//!
//! All C ABI structs are generated from `schema/crash_shm.h`. Layout
//! correctness is enforced by compile-time size/offset assertions and tests.

mod reader;
pub(crate) mod types;

pub use reader::{
    OwnedShmSnapshot, ScreenshotBudgetExhaustion, ScreenshotReadOutcome, SharedMemory,
    ShmConsistencyIssue, ShmSnapshotError, ShmSnapshotPolicy, ShmValidationError,
};
pub use types::{
    ATTACHMENT_OFFSET, CONTEXT_OFFSET, CRUMB_CATEGORY_MAX, CRUMB_MAX_THREADS, CRUMB_RING_CAPACITY,
    CRUMB_SEVERITY_ERROR, CRUMB_SEVERITY_INFO, CRUMB_SEVERITY_MAX, CRUMB_SEVERITY_WARN,
    FOOTER_OFFSET, MAX_ANNOTATIONS, MAX_ATTACHMENTS, RawAttachment, RawBreadcrumb, RawCrashContext,
    RawScreenshot, RawSettingsSnapshot, SCREENSHOT_BYTES_PER_SLOT, SCREENSHOT_DATA_SIZE,
    SCREENSHOT_HEIGHT, SCREENSHOT_META_SIZE, SCREENSHOT_SLOTS, SCREENSHOT_WIDTH, SECTION1_OFFSET,
    SECTION1_SIZE, SECTION2_OFFSET, SECTION2_SIZE, SECTION3_OFFSET, SECTION3_SIZE, SECTION4_OFFSET,
    SECTION4_SIZE, SETTINGS_OFFSET, SHM_CANARY, SHM_MAGIC, SHM_PRODUCER_NOT_READY,
    SHM_PRODUCER_READY, SHM_TOTAL_SIZE, SHM_VERSION, SUT_PRODUCER_EXTENSION_MAX_ENTRIES,
    SUT_PRODUCER_EXTENSION_VERSION, ShmAttachmentSection, ShmAttachmentSlot, ShmHeader,
    SutBreadcrumb, SutCrashAnnotation, SutCrashContext, SutCrashSettingsSnapshot, SutCrumbRing,
    SutCrumbState, SutProducerExtensionEntry, SutScreenshotSection, SutShmRegion,
};
