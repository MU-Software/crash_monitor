//! Post-processor: relocate finished reports from `pending/` to `sent/`.
//!
//! Runs near the end of the post-processor chain (after PNG conversion,
//! feedback dialog, ZIP archival). For each `result.json_path`, finds every
//! file in the same directory that shares the report's basename prefix and
//! moves them all to the sibling `sent/` directory. Mutates
//! `result.json_path` so downstream notifiers see the new path.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use crate::pipeline::{
    CrashEvent, Plugin, PluginContext, PluginExecution, PostProcessor, Priority, ReportResult,
};

// Large enough for the bounded ZIPArchiver output plus compression/header
// overhead, while still preventing an unbounded cross-filesystem copy.
const MAX_MOVE_FILE_BYTES: u64 = 576 * 1024 * 1024;
const STREAM_BUFFER_BYTES: usize = 64 * 1024;

#[derive(Default)]
pub struct MoveToSent {
    sent_dir_override: Option<PathBuf>,
}

impl MoveToSent {
    /// Use `paths::sent_dir()` at process time. Production default.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Force a specific sent directory. Used by tests with tempdir layouts.
    #[cfg(test)]
    #[must_use]
    pub fn with_dir(sent_dir: PathBuf) -> Self {
        Self {
            sent_dir_override: Some(sent_dir),
        }
    }

    fn resolve_sent_dir(&self, pending: &Path) -> Result<PathBuf, String> {
        if let Some(ref p) = self.sent_dir_override {
            fs::create_dir_all(p).map_err(|e| format!("create sent override: {e}"))?;
            return Ok(p.clone());
        }
        // Production: prefer the documented `sent_dir()` (under data_dir).
        if let Ok(p) = crate::utils::paths::sent_dir() {
            return Ok(p);
        }
        // Fallback: sibling of pending. Used when output_dir is overridden
        // (e.g., integration tests) but sent_dir_override isn't.
        let sibling = crate::utils::paths::sent_dir_for(pending);
        fs::create_dir_all(&sibling).map_err(|e| format!("create sent sibling: {e}"))?;
        Ok(sibling)
    }

    fn process_impl(
        &self,
        result: &mut ReportResult,
        context: &PluginContext,
        mut after_move: impl FnMut(&Path, &Path),
    ) -> Result<(), String> {
        context.checkpoint()?;
        let Some(json_path) = result.json_path.clone() else {
            return Ok(()); // Nothing written → nothing to move
        };
        let raw_path = result.raw_path.clone();
        let pending_dir = json_path
            .parent()
            .ok_or_else(|| "json_path has no parent".to_string())?
            .to_path_buf();

        let stem = json_path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| "json_path stem unreadable".to_string())?
            .to_string();

        let sent_dir = self.resolve_sent_dir(&pending_dir)?;

        let read_dir = fs::read_dir(&pending_dir)
            .map_err(|e| format!("read_dir {}: {e}", pending_dir.display()))?;
        for entry in read_dir {
            context.checkpoint()?;
            let Ok(entry) = entry else { continue };
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !name.starts_with(&stem) {
                continue;
            }
            // Match either exact basename or basename followed by `.` / `_`.
            // Prevents accidental prefix collision between e.g. `crash_..._123`
            // and `crash_..._1234`.
            let suffix = &name[stem.len()..];
            if !suffix.is_empty() && !suffix.starts_with('.') && !suffix.starts_with('_') {
                continue;
            }

            let dest = sent_dir.join(name);
            if let Err(e) = move_file(&path, &dest, context) {
                eprintln!(
                    "[monitor] MoveToSent: failed to move {} → {}: {e}",
                    path.display(),
                    dest.display()
                );
                continue;
            }

            after_move(&path, &dest);

            // A successful rename/copy+delete makes the old path stale. Commit
            // the matching ReportResult field before the next checkpoint can
            // observe cancellation.
            if path == json_path {
                result.json_path = Some(dest.clone());
            }
            if raw_path.as_ref() == Some(&path) {
                result.raw_path = Some(dest);
            }
        }

        context.checkpoint()
    }

    #[cfg(test)]
    fn process_with_after_move(
        &self,
        result: &mut ReportResult,
        context: &PluginContext,
        after_move: impl FnMut(&Path, &Path),
    ) -> Result<(), String> {
        self.process_impl(result, context, after_move)
    }
}

impl Plugin for MoveToSent {
    fn name(&self) -> &'static str {
        "MoveToSent"
    }
    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }
    fn priority(&self) -> Priority {
        Priority::Low
    }
}

impl PostProcessor for MoveToSent {
    fn process(
        &self,
        _event: &CrashEvent,
        result: &mut ReportResult,
        context: &PluginContext,
    ) -> Result<(), String> {
        self.process_impl(result, context, |_, _| {})
    }
}

/// Move a file from `src` to `dst`. Falls back to copy + delete when `rename`
/// fails with `EXDEV` (cross-filesystem) — relevant for some test sandboxes.
fn move_file(src: &Path, dst: &Path, context: &PluginContext) -> Result<(), String> {
    move_file_with(
        src,
        dst,
        context,
        |source, destination| fs::rename(source, destination),
        || {},
        || {},
    )
}

fn move_file_with(
    src: &Path,
    dst: &Path,
    context: &PluginContext,
    rename: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
    after_copy_chunk: impl FnMut(),
    after_copy_complete: impl FnOnce(),
) -> Result<(), String> {
    context.checkpoint()?;
    let metadata = fs::symlink_metadata(src)
        .map_err(|e| format!("cannot inspect source '{}': {e}", src.display()))?;
    validate_regular_size(src, &metadata)?;
    context.checkpoint()?;

    match rename(src, dst) {
        Ok(()) => Ok(()),
        Err(error) if error.raw_os_error() == Some(nix::libc::EXDEV) => copy_across_filesystems(
            src,
            dst,
            context,
            &metadata,
            after_copy_chunk,
            after_copy_complete,
        ),
        Err(error) => Err(format!("rename: {error}")),
    }
}

fn copy_across_filesystems(
    src: &Path,
    dst: &Path,
    context: &PluginContext,
    initial_metadata: &fs::Metadata,
    mut after_copy_chunk: impl FnMut(),
    after_copy_complete: impl FnOnce(),
) -> Result<(), String> {
    context.checkpoint()?;
    let mut input = fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(src)
        .map_err(|e| format!("cannot open source '{}': {e}", src.display()))?;
    let source_metadata = input
        .metadata()
        .map_err(|e| format!("cannot inspect source '{}': {e}", src.display()))?;
    validate_regular_size(src, &source_metadata)?;
    if !same_file(initial_metadata, &source_metadata)
        || initial_metadata.len() != source_metadata.len()
    {
        return Err(format!("source changed before copy: '{}'", src.display()));
    }
    context.checkpoint()?;

    let tmp_path = move_tmp_path(dst)?;
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&tmp_path)
        .map_err(|e| {
            format!(
                "cannot create move temporary file '{}': {e}",
                tmp_path.display()
            )
        })?;

    let copy_result = (|| -> Result<(), String> {
        let mut copied = 0_u64;
        let mut buffer = vec![0_u8; STREAM_BUFFER_BYTES];
        loop {
            context.checkpoint()?;
            let count = input
                .read(&mut buffer)
                .map_err(|e| format!("copy read '{}': {e}", src.display()))?;
            if count == 0 {
                break;
            }
            let count_u64 =
                u64::try_from(count).map_err(|_| "copy read size overflow".to_string())?;
            copied = copied
                .checked_add(count_u64)
                .ok_or_else(|| "copy size overflow".to_string())?;
            if copied > MAX_MOVE_FILE_BYTES {
                return Err(format!(
                    "source '{}' exceeded move limit while copying",
                    src.display()
                ));
            }
            output
                .write_all(&buffer[..count])
                .map_err(|e| format!("copy write '{}': {e}", tmp_path.display()))?;
            after_copy_chunk();
            context.checkpoint()?;
        }
        if copied != source_metadata.len() {
            return Err(format!(
                "source '{}' changed size while copying ({} != {})",
                src.display(),
                copied,
                source_metadata.len()
            ));
        }
        after_copy_complete();
        output
            .set_permissions(source_metadata.permissions())
            .map_err(|e| format!("cannot set move temporary permissions: {e}"))?;
        drop(output);

        let current_metadata = fs::symlink_metadata(src)
            .map_err(|e| format!("cannot re-inspect source '{}': {e}", src.display()))?;
        if !same_file(&source_metadata, &current_metadata)
            || source_metadata.len() != current_metadata.len()
        {
            return Err(format!("source changed during copy: '{}'", src.display()));
        }

        // This is the transactional commit boundary. Cancellation may abort
        // before publish, but not between publishing the destination, deleting
        // the source, and updating ReportResult in the caller.
        context.checkpoint()?;
        fs::rename(&tmp_path, dst).map_err(|e| format!("publish copied file: {e}"))?;
        fs::remove_file(src).map_err(|e| format!("remove source after copy: {e}"))?;
        Ok(())
    })();

    if copy_result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    copy_result
}

fn validate_regular_size(path: &Path, metadata: &fs::Metadata) -> Result<(), String> {
    if !metadata.file_type().is_file() {
        return Err(format!(
            "source is not a regular file: '{}'",
            path.display()
        ));
    }
    if metadata.len() > MAX_MOVE_FILE_BYTES {
        return Err(format!(
            "source '{}' exceeds move limit ({} > {MAX_MOVE_FILE_BYTES})",
            path.display(),
            metadata.len()
        ));
    }
    Ok(())
}

fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

fn move_tmp_path(dst: &Path) -> Result<PathBuf, String> {
    let name = dst
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("destination has no valid filename: '{}'", dst.display()))?;
    Ok(dst.with_file_name(format!(".{name}.move-{}.tmp", uuid::Uuid::new_v4())))
}

#[cfg(test)]
#[path = "../../tests/unit/postprocessors/move_to_sent_tests.rs"]
mod tests;
