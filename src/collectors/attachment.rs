//! Collector: Attachment files registered by the C app via `sut_crash_attach_file()`.
//!
//! Reads file paths from the shared memory attachment section,
//! copies each file to the report's pending directory.
//! Self-contained — shm reading is in `SharedMemory`, file copy is here.

use std::path::PathBuf;
use std::sync::Arc;

use mach2::port::mach_port_t;

use crate::pipeline::{CollectedData, Collector, CrashEvent, Plugin, Priority};
use crate::shm::SharedMemory;

// ═══════════════════════════════════════════════════
//  Raw data type
// ═══════════════════════════════════════════════════

/// An attachment that was registered and successfully copied.
pub struct RawCopiedAttachment {
    pub label: String,
    pub original_path: String,
    pub copied_path: PathBuf,
    pub size: u64,
}

// ═══════════════════════════════════════════════════
//  Plugin + Collector implementation
// ═══════════════════════════════════════════════════

/// Maximum file size to copy (50 MB).
const MAX_ATTACHMENT_SIZE: u64 = 50 * 1024 * 1024;

pub struct AttachmentCollector {
    shm: Arc<SharedMemory>,
}

impl AttachmentCollector {
    pub fn new(shm: Arc<SharedMemory>) -> Self {
        Self { shm }
    }
}

impl Plugin for AttachmentCollector {
    fn name(&self) -> &'static str {
        "AttachmentCollector"
    }
    fn priority(&self) -> Priority {
        Priority::Low
    }
}

impl Collector for AttachmentCollector {
    fn collect(
        &self,
        event: &CrashEvent,
        _task: mach_port_t,
        data: &mut CollectedData,
    ) -> Result<(), String> {
        let registered = self.shm.read_attachments();
        if registered.is_empty() {
            return Ok(());
        }

        let pending_dir = crate::utils::paths::pending_dir()?;
        let basename = crate::pipeline::report::report_filename(event.report_type, event.pid);

        for att in &registered {
            let src = std::path::Path::new(&att.path);
            if !src.exists() {
                eprintln!(
                    "[monitor] AttachmentCollector: file not found: {} (label={})",
                    att.path, att.label
                );
                continue;
            }

            let meta = match std::fs::metadata(src) {
                Ok(m) => m,
                Err(e) => {
                    eprintln!(
                        "[monitor] AttachmentCollector: metadata failed: {} (label={}): {e}",
                        att.path, att.label
                    );
                    continue;
                }
            };
            if meta.len() > MAX_ATTACHMENT_SIZE {
                eprintln!(
                    "[monitor] AttachmentCollector: file too large ({} bytes > {MAX_ATTACHMENT_SIZE}): {}",
                    meta.len(),
                    att.path
                );
                continue;
            }

            let ext = src.extension().and_then(|e| e.to_str()).unwrap_or("bin");
            let dest_name = format!("{basename}_{}.{ext}", att.label);
            let dest = pending_dir.join(&dest_name);

            if let Err(e) = std::fs::copy(src, &dest) {
                eprintln!(
                    "[monitor] AttachmentCollector: copy failed: {} → {}: {e}",
                    att.path,
                    dest.display()
                );
                continue;
            }

            data.raw.attachments.push(RawCopiedAttachment {
                label: att.label.clone(),
                original_path: att.path.clone(),
                copied_path: dest,
                size: meta.len(),
            });
        }

        if !data.raw.attachments.is_empty() {
            eprintln!(
                "[monitor] AttachmentCollector: {} files copied",
                data.raw.attachments.len()
            );
        }

        Ok(())
    }
}
