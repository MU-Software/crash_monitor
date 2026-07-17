//! Collector: Attachment files registered by the C app via `sut_crash_attach_file()`.
//!
//! The collector only snapshots registered paths from shared memory while the
//! target is suspended. `AttachmentCopier` performs filesystem I/O later on
//! the finalization worker.

use std::path::PathBuf;
use std::sync::Arc;

use mach2::port::mach_port_t;

use crate::pipeline::{CollectedData, Collector, CrashEvent, Plugin, PreProcessor, Priority};
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
        _event: &CrashEvent,
        _task: mach_port_t,
        data: &mut CollectedData,
    ) -> Result<(), String> {
        data.raw.attachment_registrations = self.shm.read_attachments();
        Ok(())
    }
}

/// Finalization-only pre-processor that copies registered attachment files.
pub struct AttachmentCopier {
    output_dir: Option<PathBuf>,
}

impl AttachmentCopier {
    #[must_use]
    pub fn new() -> Self {
        Self { output_dir: None }
    }

    #[cfg(test)]
    #[must_use]
    fn with_dir(output_dir: PathBuf) -> Self {
        Self {
            output_dir: Some(output_dir),
        }
    }
}

impl Default for AttachmentCopier {
    fn default() -> Self {
        Self::new()
    }
}

impl Plugin for AttachmentCopier {
    fn name(&self) -> &'static str {
        "AttachmentCopier"
    }

    fn priority(&self) -> Priority {
        Priority::High
    }
}

impl PreProcessor for AttachmentCopier {
    fn process(&self, event: &CrashEvent, data: &mut CollectedData) -> Result<(), String> {
        let registered = &data.raw.attachment_registrations;
        if registered.is_empty() {
            return Ok(());
        }

        let pending_dir = match &self.output_dir {
            Some(dir) => dir.clone(),
            None => crate::utils::paths::pending_dir()?,
        };
        let basename = crate::pipeline::report::report_filename(event.report_type, event.pid);

        for att in registered {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::ReportType;

    #[test]
    fn attachment_copy_runs_in_preprocessor_stage() {
        let tempdir = tempfile::tempdir().unwrap();
        let source = tempdir.path().join("source.log");
        std::fs::write(&source, b"diagnostic").unwrap();
        let mut data = CollectedData::default();
        data.raw
            .attachment_registrations
            .push(crate::shm::RawAttachment {
                label: "log".into(),
                path: source.to_string_lossy().into_owned(),
            });
        let event = CrashEvent {
            report_type: ReportType::Crash,
            termination: None,
            exception_type: Some(1),
            exception_code: None,
            exception_subcode: None,
            crashed_thread: None,
            bail_on_suspend_failure: false,
            pid: 123,
            process_name: "app".into(),
            hang_duration_ms: None,
        };

        AttachmentCopier::with_dir(tempdir.path().to_path_buf())
            .process(&event, &mut data)
            .unwrap();

        assert_eq!(data.raw.attachments.len(), 1);
        assert_eq!(
            std::fs::read(&data.raw.attachments[0].copied_path).unwrap(),
            b"diagnostic"
        );
    }
}
