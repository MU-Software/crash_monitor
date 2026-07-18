//! Post-processor: bundle report files into a single ZIP archive.
//!
//! Archives only the exact files registered by the event-scoped transaction.

use crate::pipeline::{
    ArtifactKind, CrashEvent, Plugin, PluginContext, PluginExecution, PostProcessor, Priority,
    ReportResult,
};
use crate::utils::paths::{
    create_private_file, open_private_directory, open_private_file, publish_private_path,
};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use zip::write::SimpleFileOptions;

const MAX_ARCHIVE_ENTRIES: usize = 256;
const MAX_ARCHIVE_FILE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_ARCHIVE_TOTAL_BYTES: u64 = 512 * 1024 * 1024;
const STREAM_BUFFER_BYTES: usize = 64 * 1024;

struct ArchiveEntry {
    path: PathBuf,
    size: u64,
}

struct PendingArchive {
    path: PathBuf,
    published: bool,
}

impl PendingArchive {
    fn create(path: PathBuf) -> Result<(Self, fs::File), String> {
        let file =
            create_private_file(&path).map_err(|e| format!("cannot create ZIP file: {e}"))?;
        Ok((
            Self {
                path,
                published: false,
            },
            file,
        ))
    }

    fn mark_published(&mut self) {
        self.published = true;
    }
}

impl Drop for PendingArchive {
    fn drop(&mut self) {
        if !self.published {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub struct ZIPArchiver;

impl ZIPArchiver {
    fn process_impl(
        result: &mut ReportResult,
        context: &PluginContext,
        mut after_archive_publish: impl FnMut(),
        mut after_archive_chunk: impl FnMut(),
        mut after_archive_finalize: impl FnMut(),
        sync_archive_directory: impl FnOnce(&Path) -> Result<(), String>,
    ) -> Result<(), String> {
        context.checkpoint()?;
        let Some(json_path) = result.json_path.clone() else {
            return Ok(()); // No report to archive
        };

        let dir = json_path
            .parent()
            .ok_or_else(|| "no parent directory".to_string())?;
        // Archiving operates only on managed report artifacts. Correct owned
        // mode drift before any input is opened or a temporary ZIP is created.
        open_private_directory(dir)?;

        let files = collect_report_files(result, context)?;
        if files.is_empty() {
            return Ok(());
        }

        let archive_stem = if context.artifact_transaction().is_some() {
            "report".to_string()
        } else {
            json_path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .ok_or_else(|| "no report file stem".to_string())?
                .to_string()
        };
        let zip_path = dir.join(format!("{archive_stem}.zip"));
        let tmp_path = dir.join(format!(".{archive_stem}.zip-{}.tmp", uuid::Uuid::new_v4()));
        let raw_was_archived = result
            .raw_path
            .as_ref()
            .is_some_and(|raw_path| files.iter().any(|file| file.path == *raw_path));

        // Write ZIP to temporary file
        let (mut pending_archive, tmp_file) =
            PendingArchive::create(tmp_path).map_err(|e| format!("ZIP creation failed: {e}"))?;
        if let Err(e) = write_zip(tmp_file, &files, context, &mut after_archive_chunk) {
            return Err(format!("ZIP creation failed: {e}"));
        }
        after_archive_finalize();
        context.checkpoint()?;

        // Atomic exclusive publication: a raced destination is never replaced.
        if let Err(e) = publish_private_path(&pending_archive.path, &zip_path) {
            return Err(format!("ZIP rename failed: {e}"));
        }
        pending_archive.mark_published();
        if let Err(error) = sync_archive_directory(dir) {
            let warning = format!(
                "ZIP '{}' was published but its directory could not be synced: {error}",
                zip_path.display()
            );
            if let Some(transaction) = context.artifact_transaction() {
                transaction.record_durability_warning(warning);
            } else {
                eprintln!("[monitor] ZIPArchiver: {warning}");
            }
        }

        if let Some(transaction) = context.artifact_transaction() {
            transaction.register_file(&zip_path, ArtifactKind::Archive)?;
        }

        after_archive_publish();

        // The published ZIP is canonical immediately. Commit every result
        // field represented by the archive before any cleanup checkpoint can
        // return on cancellation.
        result.json_path = Some(zip_path.clone());
        if !result.artifact_paths.contains(&zip_path) {
            result.artifact_paths.push(zip_path);
        }
        if raw_was_archived {
            result.raw_path = None;
        }

        // Remove original files (best-effort). On cancellation, leaving an
        // original behind is safe because ReportResult already names the ZIP.
        for file in &files {
            context.checkpoint()?;
            match fs::remove_file(&file.path) {
                Ok(()) => {
                    if let Some(transaction) = context.artifact_transaction() {
                        transaction.unregister_file(&file.path)?;
                    }
                    result
                        .artifact_paths
                        .retain(|artifact| artifact != &file.path);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    if let Some(transaction) = context.artifact_transaction() {
                        transaction.unregister_file(&file.path)?;
                    }
                    result
                        .artifact_paths
                        .retain(|artifact| artifact != &file.path);
                }
                Err(error) => eprintln!(
                    "[monitor] ZIPArchiver: cannot remove archived input '{}': {error}",
                    file.path.display()
                ),
            }
        }

        context.checkpoint()
    }

    #[cfg(test)]
    fn process_with_after_archive_publish(
        result: &mut ReportResult,
        context: &PluginContext,
        after_archive_publish: impl FnMut(),
    ) -> Result<(), String> {
        Self::process_impl(
            result,
            context,
            after_archive_publish,
            || {},
            || {},
            sync_private_directory,
        )
    }

    #[cfg(test)]
    fn process_with_after_archive_chunk(
        result: &mut ReportResult,
        context: &PluginContext,
        after_archive_chunk: impl FnMut(),
    ) -> Result<(), String> {
        Self::process_impl(
            result,
            context,
            || {},
            after_archive_chunk,
            || {},
            sync_private_directory,
        )
    }

    #[cfg(test)]
    fn process_with_after_archive_finalize(
        result: &mut ReportResult,
        context: &PluginContext,
        after_archive_finalize: impl FnMut(),
    ) -> Result<(), String> {
        Self::process_impl(
            result,
            context,
            || {},
            || {},
            after_archive_finalize,
            sync_private_directory,
        )
    }

    #[cfg(test)]
    fn process_with_directory_sync(
        result: &mut ReportResult,
        context: &PluginContext,
        sync_archive_directory: impl FnOnce(&Path) -> Result<(), String>,
    ) -> Result<(), String> {
        Self::process_impl(result, context, || {}, || {}, || {}, sync_archive_directory)
    }
}

impl Plugin for ZIPArchiver {
    fn name(&self) -> &'static str {
        "ZIPArchiver"
    }
    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }
    fn priority(&self) -> Priority {
        Priority::Low
    }
    fn order_after(&self) -> &'static [&'static str] {
        // RawCleanup decides whether the fail-safe raw file is retained;
        // PNGConverter and FeedbackDialog mutate report artifacts before they
        // are archived. All are optional ordering constraints.
        &["RawCleanup", "PNGConverter", "FeedbackDialog"]
    }
}

impl PostProcessor for ZIPArchiver {
    fn process(
        &self,
        _event: &CrashEvent,
        result: &mut ReportResult,
        context: &PluginContext,
    ) -> Result<(), String> {
        Self::process_impl(result, context, || {}, || {}, || {}, sync_private_directory)
    }
}

fn collect_report_files(
    result: &ReportResult,
    context: &PluginContext,
) -> Result<Vec<ArchiveEntry>, String> {
    context.checkpoint()?;
    let paths = if let Some(transaction) = context.artifact_transaction() {
        transaction.artifact_paths()
    } else if !result.artifact_paths.is_empty() {
        result.artifact_paths.clone()
    } else {
        [result.json_path.clone(), result.raw_path.clone()]
            .into_iter()
            .flatten()
            .collect()
    };

    let mut files = Vec::new();
    let mut total_bytes = 0_u64;
    for path in paths {
        context.checkpoint()?;
        if files.len() >= MAX_ARCHIVE_ENTRIES {
            return Err(format!(
                "report manifest exceeds archive entry limit ({MAX_ARCHIVE_ENTRIES})"
            ));
        }
        let metadata = fs::symlink_metadata(&path)
            .map_err(|e| format!("cannot inspect '{}': {e}", path.display()))?;
        if !metadata.file_type().is_file() {
            return Err(format!(
                "manifest artifact is not a regular file: '{}'",
                path.display()
            ));
        }
        let size = metadata.len();
        if size > MAX_ARCHIVE_FILE_BYTES {
            return Err(format!(
                "archive input '{}' exceeds per-file limit ({size} > {MAX_ARCHIVE_FILE_BYTES})",
                path.display()
            ));
        }
        total_bytes = total_bytes
            .checked_add(size)
            .ok_or_else(|| "archive input size overflow".to_string())?;
        if total_bytes > MAX_ARCHIVE_TOTAL_BYTES {
            return Err(format!(
                "report family exceeds total archive limit ({total_bytes} > {MAX_ARCHIVE_TOTAL_BYTES})"
            ));
        }
        files.push(ArchiveEntry { path, size });
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
}

fn write_zip(
    file: fs::File,
    files: &[ArchiveEntry],
    context: &PluginContext,
    mut after_chunk: impl FnMut(),
) -> Result<(), String> {
    context.checkpoint()?;
    let mut writer = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o600);
    let mut total_written = 0_u64;
    let mut buffer = vec![0_u8; STREAM_BUFFER_BYTES];

    for entry in files {
        context.checkpoint()?;
        let name = entry
            .path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| "invalid filename".to_string())?;
        let mut input = open_regular_file(&entry.path)?;
        let current_size = input
            .metadata()
            .map_err(|e| format!("cannot inspect '{}': {e}", entry.path.display()))?
            .len();
        if current_size != entry.size {
            return Err(format!(
                "archive input '{}' changed size ({} -> {current_size})",
                entry.path.display(),
                entry.size
            ));
        }

        writer
            .start_file(name, options)
            .map_err(|e| format!("ZIP start_file failed: {e}"))?;

        let mut file_written = 0_u64;
        loop {
            context.checkpoint()?;
            let count = input
                .read(&mut buffer)
                .map_err(|e| format!("cannot read '{}': {e}", entry.path.display()))?;
            if count == 0 {
                break;
            }
            let count_u64 = u64::try_from(count).map_err(|_| "read size overflow".to_string())?;
            file_written = file_written
                .checked_add(count_u64)
                .ok_or_else(|| "archive file size overflow".to_string())?;
            if file_written > MAX_ARCHIVE_FILE_BYTES {
                return Err(format!(
                    "archive input '{}' exceeded per-file limit while reading",
                    entry.path.display()
                ));
            }
            total_written = total_written
                .checked_add(count_u64)
                .ok_or_else(|| "archive total size overflow".to_string())?;
            if total_written > MAX_ARCHIVE_TOTAL_BYTES {
                return Err("archive input exceeded total limit while reading".to_string());
            }
            writer
                .write_all(&buffer[..count])
                .map_err(|e| format!("ZIP write failed: {e}"))?;
            after_chunk();
            context.checkpoint()?;
        }
        if file_written != entry.size {
            return Err(format!(
                "archive input '{}' changed size while reading ({} != {})",
                entry.path.display(),
                file_written,
                entry.size
            ));
        }
    }

    let output = writer
        .finish()
        .map_err(|e| format!("ZIP finalize failed: {e}"))?;
    output
        .sync_all()
        .map_err(|e| format!("ZIP sync failed: {e}"))?;
    Ok(())
}

fn open_regular_file(path: &Path) -> Result<fs::File, String> {
    let file = open_private_file(path)
        .map_err(|e| format!("cannot safely open archive input '{}': {e}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|e| format!("cannot inspect '{}': {e}", path.display()))?;
    if !metadata.file_type().is_file() {
        return Err(format!(
            "archive input is not a regular file: '{}'",
            path.display()
        ));
    }
    if metadata.len() > MAX_ARCHIVE_FILE_BYTES {
        return Err(format!(
            "archive input '{}' exceeds per-file limit ({} > {MAX_ARCHIVE_FILE_BYTES})",
            path.display(),
            metadata.len()
        ));
    }
    Ok(file)
}

fn sync_private_directory(path: &Path) -> Result<(), String> {
    open_private_directory(path)?
        .sync_all()
        .map_err(|error| format!("ZIP directory sync failed: {error}"))
}

#[cfg(test)]
#[path = "../../tests/unit/postprocessors/zip_archiver_tests.rs"]
mod tests;
