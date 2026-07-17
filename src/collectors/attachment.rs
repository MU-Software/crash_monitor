//! Collector: Attachment files registered by the C app via `sut_crash_attach_file()`.
//!
//! The collector reads registered paths from the event's owned shared-memory
//! snapshot. The live mapping was copied while the target was suspended;
//! `AttachmentCopier` performs filesystem I/O later on the finalization worker.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use mach2::port::mach_port_t;

use crate::pipeline::{
    CollectedData, Collector, CrashEvent, Plugin, PluginContext, PluginExecution, PreProcessor,
    Priority,
};
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

/// Bound each cooperative read/write step independently of the file size.
const ATTACHMENT_COPY_BUFFER_SIZE: usize = 64 * 1024;

/// A broken writer must not turn repeated partial/EINTR writes into an
/// unbounded in-process loop when no deadline was configured.
const MAX_WRITE_ATTEMPTS_PER_CHUNK: usize = 1024;

/// Apply the same finite retry policy to interrupted reads.
const MAX_READ_ATTEMPTS_PER_CHUNK: usize = 1024;

#[derive(Default)]
pub struct AttachmentCollector;

impl AttachmentCollector {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl Plugin for AttachmentCollector {
    fn name(&self) -> &'static str {
        "AttachmentCollector"
    }
    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
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
        context: &PluginContext,
    ) -> Result<(), String> {
        context.checkpoint()?;
        let snapshot = context
            .shm_snapshot()
            .ok_or_else(|| "owned shared-memory snapshot unavailable".to_string())?;
        data.raw.attachment_registrations = snapshot.read_attachments();
        context.checkpoint()?;
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

/// A same-directory temporary file that is removed unless it is atomically
/// published at its final destination.
struct PendingAttachmentFile {
    path: PathBuf,
    file: Option<File>,
    committed: bool,
}

impl PendingAttachmentFile {
    fn create(path: PathBuf) -> Result<Self, String> {
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
            .map_err(|error| format!("create temporary attachment failed: {error}"))?;
        Ok(Self {
            path,
            file: Some(file),
            committed: false,
        })
    }

    fn file_mut(&mut self) -> Result<&mut File, String> {
        self.file
            .as_mut()
            .ok_or_else(|| "temporary attachment is already closed".to_string())
    }

    fn commit(mut self, destination: &Path) -> Result<(), String> {
        drop(self.file.take());
        // Publish atomically without replacing an existing attachment if a
        // filename collision or another writer wins the race.
        std::fs::hard_link(&self.path, destination)
            .map_err(|error| format!("commit temporary attachment failed: {error}"))?;
        self.committed = true;
        if let Err(error) = std::fs::remove_file(&self.path) {
            eprintln!(
                "[monitor] AttachmentCopier: failed to remove committed temporary file {}: {error}",
                self.path.display()
            );
        }
        Ok(())
    }
}

impl Drop for PendingAttachmentFile {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn write_all_cooperative<W: Write>(
    destination: &mut W,
    mut bytes: &[u8],
    context: &PluginContext,
) -> Result<(), String> {
    let mut attempts = 0_usize;
    while !bytes.is_empty() {
        context.checkpoint()?;
        if attempts >= MAX_WRITE_ATTEMPTS_PER_CHUNK {
            return Err("too many attachment write attempts".to_string());
        }
        attempts += 1;

        match destination.write(bytes) {
            Ok(0) => return Err("write temporary attachment made no progress".to_string()),
            Ok(written) => {
                bytes = bytes
                    .get(written..)
                    .ok_or_else(|| "attachment writer returned invalid length".to_string())?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => {
                return Err(format!("write temporary attachment failed: {error}"));
            }
        }
    }
    Ok(())
}

fn copy_bounded<R: Read, W: Write>(
    source: &mut R,
    destination: &mut W,
    max_size: u64,
    context: &PluginContext,
) -> Result<u64, String> {
    let probe_limit = max_size
        .checked_add(1)
        .ok_or_else(|| "attachment size limit overflow".to_string())?;
    let mut copied = 0_u64;
    let mut buffer = vec![0_u8; ATTACHMENT_COPY_BUFFER_SIZE];

    loop {
        context.checkpoint()?;
        let remaining_probe = probe_limit
            .checked_sub(copied)
            .ok_or_else(|| "attachment exceeded size limit".to_string())?;
        let read_len = usize::try_from(remaining_probe)
            .unwrap_or(buffer.len())
            .min(buffer.len());
        let mut attempts = 0_usize;
        let bytes_read = loop {
            context.checkpoint()?;
            if attempts >= MAX_READ_ATTEMPTS_PER_CHUNK {
                return Err("too many attachment read attempts".to_string());
            }
            attempts += 1;

            match source.read(&mut buffer[..read_len]) {
                Ok(bytes_read) => break bytes_read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(format!("read attachment failed: {error}")),
            }
        };
        context.checkpoint()?;

        if bytes_read == 0 {
            return Ok(copied);
        }

        let next_size = copied
            .checked_add(
                u64::try_from(bytes_read)
                    .map_err(|_| "attachment byte count overflow".to_string())?,
            )
            .ok_or_else(|| "attachment byte count overflow".to_string())?;
        if next_size > max_size {
            return Err(format!("attachment exceeds {max_size}-byte size limit"));
        }

        write_all_cooperative(destination, &buffer[..bytes_read], context)?;
        copied = next_size;
        context.checkpoint()?;
    }
}

fn copy_registered_attachment(
    source_path: &Path,
    destination: &Path,
    temporary_path: PathBuf,
    context: &PluginContext,
) -> Result<u64, String> {
    context.checkpoint()?;
    let mut source = OpenOptions::new()
        .read(true)
        // O_NOFOLLOW rejects a symlink final component. O_NONBLOCK prevents
        // opening an attacker-controlled FIFO from stalling before fstat.
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(source_path)
        .map_err(|error| format!("open attachment failed: {error}"))?;
    context.checkpoint()?;

    let metadata = source
        .metadata()
        .map_err(|error| format!("fstat attachment failed: {error}"))?;
    if !metadata.file_type().is_file() {
        return Err("attachment is not a regular file".to_string());
    }
    if metadata.len() > MAX_ATTACHMENT_SIZE {
        return Err(format!(
            "attachment is too large ({} bytes > {MAX_ATTACHMENT_SIZE})",
            metadata.len()
        ));
    }
    context.checkpoint()?;

    let mut temporary = PendingAttachmentFile::create(temporary_path)?;
    let copied = copy_bounded(
        &mut source,
        temporary.file_mut()?,
        MAX_ATTACHMENT_SIZE,
        context,
    )?;
    context.checkpoint()?;
    temporary.commit(destination)?;
    Ok(copied)
}

fn attachment_filename_component(value: &str, fallback: &str) -> String {
    let sanitized: String = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        fallback.to_string()
    } else {
        sanitized
    }
}

impl Plugin for AttachmentCopier {
    fn name(&self) -> &'static str {
        "AttachmentCopier"
    }

    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }

    fn priority(&self) -> Priority {
        Priority::High
    }
}

impl PreProcessor for AttachmentCopier {
    fn process(
        &self,
        event: &CrashEvent,
        data: &mut CollectedData,
        context: &PluginContext,
    ) -> Result<(), String> {
        context.checkpoint()?;
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
            context.checkpoint()?;
            let src = std::path::Path::new(&att.path);
            let label = attachment_filename_component(&att.label, "attachment");
            let ext = attachment_filename_component(
                src.extension()
                    .and_then(|extension| extension.to_str())
                    .unwrap_or("bin"),
                "bin",
            );
            // Keep duplicate labels and labels that sanitize to the same
            // component distinct. The no-clobber publish above remains the
            // final guard against the vanishingly unlikely UUID collision.
            let dest_name = format!("{basename}_{label}_{}.{ext}", uuid::Uuid::new_v4().simple());
            let dest = pending_dir.join(&dest_name);
            let temporary_path = pending_dir.join(format!(
                ".{dest_name}.{}.attachment.tmp",
                uuid::Uuid::new_v4()
            ));

            let size = match copy_registered_attachment(src, &dest, temporary_path, context) {
                Ok(size) => size,
                Err(error) if context.is_timed_out() => return Err(error),
                Err(error) => {
                    eprintln!(
                        "[monitor] AttachmentCopier: copy rejected: {} → {} (label={}): {error}",
                        att.path,
                        dest.display(),
                        att.label
                    );
                    continue;
                }
            };

            data.raw.attachments.push(RawCopiedAttachment {
                label: att.label.clone(),
                original_path: att.path.clone(),
                copied_path: dest,
                size,
            });
        }

        context.checkpoint()?;

        if !data.raw.attachments.is_empty() {
            eprintln!(
                "[monitor] AttachmentCopier: {} files copied",
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
    use nix::sys::stat::Mode;
    use nix::unistd::mkfifo;
    use std::io::{Cursor, Read};
    use std::os::unix::fs::symlink;

    struct InterruptingWriter;

    struct InterruptOnceReader {
        interrupted: bool,
        inner: Cursor<Vec<u8>>,
    }

    impl Read for InterruptOnceReader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            if !self.interrupted {
                self.interrupted = true;
                return Err(std::io::ErrorKind::Interrupted.into());
            }
            self.inner.read(buffer)
        }
    }

    impl Write for InterruptingWriter {
        fn write(&mut self, _buffer: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::from(std::io::ErrorKind::Interrupted))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn event() -> CrashEvent {
        CrashEvent {
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
        }
    }

    fn registration(label: &str, path: &Path) -> crate::shm::RawAttachment {
        crate::shm::RawAttachment {
            label: label.into(),
            path: path.to_string_lossy().into_owned(),
        }
    }

    #[test]
    fn attachment_copy_runs_in_preprocessor_stage() {
        let tempdir = tempfile::tempdir().unwrap();
        let source = tempdir.path().join("source.log");
        std::fs::write(&source, b"diagnostic").unwrap();
        let mut data = CollectedData::default();
        data.raw
            .attachment_registrations
            .push(registration("log", &source));

        AttachmentCopier::with_dir(tempdir.path().to_path_buf())
            .process(&event(), &mut data, &PluginContext::without_deadline())
            .unwrap();

        assert_eq!(data.raw.attachments.len(), 1);
        assert_eq!(data.raw.attachments[0].size, 10);
        assert_eq!(
            std::fs::read(&data.raw.attachments[0].copied_path).unwrap(),
            b"diagnostic"
        );
    }

    #[test]
    fn bounded_copy_rejects_limit_plus_one() {
        let mut source = Cursor::new(vec![0_u8; 9]);
        let mut destination = Vec::new();

        let error = copy_bounded(
            &mut source,
            &mut destination,
            8,
            &PluginContext::without_deadline(),
        )
        .unwrap_err();

        assert!(error.contains("exceeds 8-byte size limit"));
        assert!(destination.is_empty());
    }

    #[test]
    fn cooperative_writer_bounds_repeated_eintr_without_deadline() {
        let error = write_all_cooperative(
            &mut InterruptingWriter,
            b"diagnostic",
            &PluginContext::without_deadline(),
        )
        .unwrap_err();

        assert_eq!(error, "too many attachment write attempts");
    }

    #[test]
    fn bounded_copy_retries_eintr() {
        let mut source = InterruptOnceReader {
            interrupted: false,
            inner: Cursor::new(b"diagnostic".to_vec()),
        };
        let mut destination = Vec::new();

        let copied = copy_bounded(
            &mut source,
            &mut destination,
            32,
            &PluginContext::without_deadline(),
        )
        .unwrap();

        assert_eq!(copied, 10);
        assert_eq!(destination, b"diagnostic");
    }

    #[test]
    fn duplicate_and_sanitized_labels_get_distinct_files() {
        let tempdir = tempfile::tempdir().unwrap();
        let first = tempdir.path().join("first.log");
        let second = tempdir.path().join("second.log");
        std::fs::write(&first, b"first").unwrap();
        std::fs::write(&second, b"second").unwrap();
        let mut data = CollectedData::default();
        data.raw
            .attachment_registrations
            .push(registration("same/label", &first));
        data.raw
            .attachment_registrations
            .push(registration("same_label", &second));

        AttachmentCopier::with_dir(tempdir.path().to_path_buf())
            .process(&event(), &mut data, &PluginContext::without_deadline())
            .unwrap();

        assert_eq!(data.raw.attachments.len(), 2);
        let first_copy = &data.raw.attachments[0].copied_path;
        let second_copy = &data.raw.attachments[1].copied_path;
        assert_ne!(first_copy, second_copy);
        assert_eq!(std::fs::read(first_copy).unwrap(), b"first");
        assert_eq!(std::fs::read(second_copy).unwrap(), b"second");
    }

    #[test]
    fn temporary_commit_does_not_overwrite_existing_destination() {
        let tempdir = tempfile::tempdir().unwrap();
        let temporary = tempdir.path().join("temporary.tmp");
        let destination = tempdir.path().join("attachment.log");
        std::fs::write(&destination, b"existing").unwrap();
        let mut pending = PendingAttachmentFile::create(temporary.clone()).unwrap();
        pending.file_mut().unwrap().write_all(b"new").unwrap();

        let error = pending.commit(&destination).unwrap_err();

        assert!(error.contains("commit temporary attachment failed"));
        assert_eq!(std::fs::read(&destination).unwrap(), b"existing");
        assert!(!temporary.exists());
    }

    #[test]
    fn attachment_copy_rejects_symlink_source() {
        let tempdir = tempfile::tempdir().unwrap();
        let target = tempdir.path().join("target.log");
        let source = tempdir.path().join("source.log");
        std::fs::write(&target, b"diagnostic").unwrap();
        symlink(&target, &source).unwrap();
        let mut data = CollectedData::default();
        data.raw
            .attachment_registrations
            .push(registration("log", &source));

        AttachmentCopier::with_dir(tempdir.path().to_path_buf())
            .process(&event(), &mut data, &PluginContext::without_deadline())
            .unwrap();

        assert!(data.raw.attachments.is_empty());
    }

    #[test]
    fn attachment_copy_rejects_fifo_without_blocking() {
        let tempdir = tempfile::tempdir().unwrap();
        let source = tempdir.path().join("source.fifo");
        mkfifo(&source, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();
        let mut data = CollectedData::default();
        data.raw
            .attachment_registrations
            .push(registration("pipe", &source));

        AttachmentCopier::with_dir(tempdir.path().to_path_buf())
            .process(&event(), &mut data, &PluginContext::without_deadline())
            .unwrap();

        assert!(data.raw.attachments.is_empty());
    }

    #[test]
    fn uncommitted_temporary_attachment_is_removed_on_drop() {
        let tempdir = tempfile::tempdir().unwrap();
        let temporary = tempdir.path().join("temporary.tmp");
        let pending = PendingAttachmentFile::create(temporary.clone()).unwrap();
        assert!(temporary.exists());

        drop(pending);

        assert!(!temporary.exists());
    }
}
