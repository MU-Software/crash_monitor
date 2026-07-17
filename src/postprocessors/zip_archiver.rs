//! Post-processor: bundle report files into a single ZIP archive.
//!
//! Collects all files sharing the same basename prefix as the report JSON
//! (e.g., screenshots), compresses them into a `.zip`, then removes originals.

use crate::pipeline::{
    CrashEvent, Plugin, PluginContext, PluginExecution, PostProcessor, Priority, ReportResult,
};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
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
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .custom_flags(nix::libc::O_NOFOLLOW)
            .open(&path)
            .map_err(|e| format!("cannot create ZIP file: {e}"))?;
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
    ) -> Result<(), String> {
        context.checkpoint()?;
        let Some(json_path) = result.json_path.clone() else {
            return Ok(()); // No report to archive
        };

        let dir = json_path
            .parent()
            .ok_or_else(|| "no parent directory".to_string())?;
        let stem = json_path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| "no file stem".to_string())?;

        // Collect files matching the report basename
        let files = collect_report_files(dir, stem, context)?;
        if files.is_empty() {
            return Ok(());
        }

        let zip_path = dir.join(format!("{stem}.zip"));
        let tmp_path = dir.join(format!(".{stem}.zip-{}.tmp", uuid::Uuid::new_v4()));
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

        // Atomic rename
        if let Err(e) = fs::rename(&pending_archive.path, &zip_path) {
            return Err(format!("ZIP rename failed: {e}"));
        }
        pending_archive.mark_published();

        after_archive_publish();

        // The published ZIP is canonical immediately. Commit every result
        // field represented by the archive before any cleanup checkpoint can
        // return on cancellation.
        result.json_path = Some(zip_path);
        if raw_was_archived {
            result.raw_path = None;
        }

        // Remove original files (best-effort). On cancellation, leaving an
        // original behind is safe because ReportResult already names the ZIP.
        for file in &files {
            context.checkpoint()?;
            let _ = fs::remove_file(&file.path);
        }

        context.checkpoint()
    }

    #[cfg(test)]
    fn process_with_after_archive_publish(
        result: &mut ReportResult,
        context: &PluginContext,
        after_archive_publish: impl FnMut(),
    ) -> Result<(), String> {
        Self::process_impl(result, context, after_archive_publish, || {}, || {})
    }

    #[cfg(test)]
    fn process_with_after_archive_chunk(
        result: &mut ReportResult,
        context: &PluginContext,
        after_archive_chunk: impl FnMut(),
    ) -> Result<(), String> {
        Self::process_impl(result, context, || {}, after_archive_chunk, || {})
    }

    #[cfg(test)]
    fn process_with_after_archive_finalize(
        result: &mut ReportResult,
        context: &PluginContext,
        after_archive_finalize: impl FnMut(),
    ) -> Result<(), String> {
        Self::process_impl(result, context, || {}, || {}, after_archive_finalize)
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
        Self::process_impl(result, context, || {}, || {}, || {})
    }
}

fn collect_report_files(
    dir: &Path,
    stem: &str,
    context: &PluginContext,
) -> Result<Vec<ArchiveEntry>, String> {
    context.checkpoint()?;
    let entries =
        fs::read_dir(dir).map_err(|e| format!("cannot read directory '{}': {e}", dir.display()))?;

    let mut files = Vec::new();
    let mut total_bytes = 0_u64;
    let mut matching_entries = 0_usize;
    for entry in entries {
        context.checkpoint()?;
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if !matches_report_file(&name_str, stem)
            || name_str.ends_with(".zip")
            || name_str.ends_with(".tmp")
        {
            continue;
        }

        matching_entries = matching_entries
            .checked_add(1)
            .ok_or_else(|| "archive entry count overflow".to_string())?;
        if matching_entries > MAX_ARCHIVE_ENTRIES {
            return Err(format!(
                "report family exceeds archive entry limit ({MAX_ARCHIVE_ENTRIES})"
            ));
        }

        let file_type = entry
            .file_type()
            .map_err(|e| format!("cannot inspect '{}': {e}", entry.path().display()))?;
        if !file_type.is_file() {
            continue;
        }

        let path = entry.path();
        let metadata = entry
            .metadata()
            .map_err(|e| format!("cannot inspect '{}': {e}", path.display()))?;
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

fn matches_report_file(name: &str, stem: &str) -> bool {
    name.strip_prefix(stem).is_some_and(|suffix| {
        suffix.is_empty() || suffix.starts_with('.') || suffix.starts_with('_')
    })
}

fn write_zip(
    file: fs::File,
    files: &[ArchiveEntry],
    context: &PluginContext,
    mut after_chunk: impl FnMut(),
) -> Result<(), String> {
    context.checkpoint()?;
    let mut writer = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
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

    writer
        .finish()
        .map_err(|e| format!("ZIP finalize failed: {e}"))?;
    Ok(())
}

fn open_regular_file(path: &Path) -> Result<fs::File, String> {
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| format!("cannot open '{}': {e}", path.display()))?;
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

#[cfg(test)]
#[path = "../../tests/unit/postprocessors/zip_archiver_tests.rs"]
mod tests;
