//! Collector plugins — gather data while the child is suspended.

pub mod attachment;
pub mod breadcrumb;
pub mod context;
pub mod dylib;
pub mod environment;
pub mod memory;
pub mod screenshot;
pub mod thread;

pub use attachment::{AttachmentCollector, AttachmentCopier};
pub use breadcrumb::BreadcrumbCollector;
pub use context::ContextCollector;
pub use dylib::DylibCollector;
pub use environment::{ChildEnvironmentSnapshot, EnvironmentCollector};
pub use memory::MemoryCollector;
pub use screenshot::ScreenshotCollector;
pub use thread::ThreadCollector;

/// Raw collected data from all collectors, prior to formatting.
#[derive(Default)]
pub struct RawData {
    pub threads: Vec<thread::RawThreadData>,
    pub images: Vec<dylib::RawImageData>,
    pub memory_map: Vec<crate::platform::VmRegionInfo>,
    pub heap: memory::RawHeapData,
    // Phase 4: shared memory data
    pub breadcrumbs: Vec<crate::shm::RawBreadcrumb>,
    pub crash_context: Option<crate::shm::RawCrashContext>,
    pub settings_snapshot: Option<crate::shm::RawSettingsSnapshot>,
    pub attachments: Vec<attachment::RawCopiedAttachment>,
    /// Attachment path registrations copied from SHM during capture. File I/O
    /// happens later in `AttachmentCopier` on the finalization worker.
    pub attachment_registrations: Vec<crate::shm::RawAttachment>,
    pub screenshots: Vec<crate::shm::RawScreenshot>,
    /// Address → symbol name mapping, populated by `SymbolResolver` pre-processor.
    pub symbols: std::collections::BTreeMap<u64, String>,
    /// Environment and system info, populated by `EnvironmentCollector`.
    pub environment: Option<environment::RawEnvironment>,
}
