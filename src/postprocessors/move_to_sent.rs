//! Post-processor: relocate finished reports from `pending/` to `sent/`.
//!
//! Runs near the end of the post-processor chain (after PNG conversion,
//! feedback dialog, ZIP archival). Event-scoped transactions select `sent/`
//! as their atomic publication root; legacy callers move only explicitly
//! named `ReportResult` paths and never infer a family by basename prefix.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use crate::pipeline::{
    CrashEvent, Plugin, PluginContext, PluginExecution, PostProcessor, Priority, ReportResult,
};
use crate::utils::paths::{
    create_private_file, ensure_private_directory, open_private_directory, open_private_file,
    publish_private_path,
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
            ensure_private_directory(p).map_err(|e| format!("create sent override: {e}"))?;
            return Ok(p.clone());
        }
        // Production: prefer the documented `sent_dir()` (under data_dir).
        if let Ok(p) = crate::utils::paths::sent_dir() {
            return Ok(p);
        }
        // Fallback: sibling of pending. Used when output_dir is overridden
        // (e.g., integration tests) but sent_dir_override isn't.
        let sibling = crate::utils::paths::sent_dir_for(pending);
        ensure_private_directory(&sibling).map_err(|e| format!("create sent sibling: {e}"))?;
        Ok(sibling)
    }

    fn resolve_transaction_sent_dir(&self, output_root: &Path) -> Result<PathBuf, String> {
        if let Some(ref path) = self.sent_dir_override {
            ensure_private_directory(path)
                .map_err(|error| format!("create sent override: {error}"))?;
            return Ok(path.clone());
        }
        let sibling = crate::utils::paths::sent_dir_for(output_root);
        ensure_private_directory(&sibling)
            .map_err(|error| format!("create sent sibling: {error}"))?;
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
        if let Some(transaction) = context.artifact_transaction() {
            let sent_dir =
                self.resolve_transaction_sent_dir(transaction.report_context().output_root())?;
            transaction.set_destination_root(&sent_dir)?;
            return context.checkpoint();
        }
        let pending_dir = json_path
            .parent()
            .ok_or_else(|| "json_path has no parent".to_string())?
            .to_path_buf();

        let sent_dir = self.resolve_sent_dir(&pending_dir)?;
        let paths: Vec<PathBuf> = if result.artifact_paths.is_empty() {
            [Some(json_path.clone()), raw_path.clone()]
                .into_iter()
                .flatten()
                .collect()
        } else {
            result.artifact_paths.clone()
        };
        for path in paths {
            context.checkpoint()?;
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };

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
                result.raw_path = Some(dest.clone());
            }
            if let Some(artifact) = result
                .artifact_paths
                .iter_mut()
                .find(|artifact| **artifact == path)
            {
                *artifact = dest;
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
    fn order_after(&self) -> &'static [&'static str] {
        &["ZIPArchiver"]
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
    let source_device = fs::symlink_metadata(src)
        .map_err(|error| format!("cannot inspect source '{}': {error}", src.display()))?
        .dev();
    let destination_parent = dst
        .parent()
        .ok_or_else(|| format!("destination has no parent: '{}'", dst.display()))?;
    let destination_device = fs::metadata(destination_parent)
        .map_err(|error| {
            format!(
                "cannot inspect destination directory '{}': {error}",
                destination_parent.display()
            )
        })?
        .dev();
    move_file_with(
        src,
        dst,
        context,
        move |source, destination| {
            if source_device != destination_device {
                return Err(std::io::Error::from_raw_os_error(nix::libc::EXDEV));
            }
            publish_private_path(source, destination).map_err(std::io::Error::other)
        },
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
    move_file_with_operations(
        src,
        dst,
        context,
        rename,
        (after_copy_chunk, after_copy_complete),
        sync_parent_directory,
        |path| fs::remove_file(path),
    )
}

fn move_file_with_operations(
    src: &Path,
    dst: &Path,
    context: &PluginContext,
    rename: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
    copy_hooks: (impl FnMut(), impl FnOnce()),
    mut sync_parent: impl FnMut(&Path, &str) -> Result<(), String>,
    remove_source: impl FnOnce(&Path) -> std::io::Result<()>,
) -> Result<(), String> {
    context.checkpoint()?;
    let source_parent = src
        .parent()
        .ok_or_else(|| format!("source has no parent: '{}'", src.display()))?;
    open_private_directory(source_parent).map_err(|error| {
        format!(
            "cannot safely open source directory '{}': {error}",
            source_parent.display()
        )
    })?;
    let destination_parent = dst
        .parent()
        .ok_or_else(|| format!("destination has no parent: '{}'", dst.display()))?;
    if destination_parent != source_parent {
        open_private_directory(destination_parent).map_err(|error| {
            format!(
                "cannot safely open destination directory '{}': {error}",
                destination_parent.display()
            )
        })?;
    }
    let metadata = fs::symlink_metadata(src)
        .map_err(|e| format!("cannot inspect source '{}': {e}", src.display()))?;
    validate_regular_size(src, &metadata)?;
    let source = open_private_file(src)
        .map_err(|e| format!("cannot safely open source '{}': {e}", src.display()))?;
    let opened_metadata = source
        .metadata()
        .map_err(|e| format!("cannot inspect source '{}': {e}", src.display()))?;
    if !same_file(&metadata, &opened_metadata) || metadata.len() != opened_metadata.len() {
        return Err(format!("source changed before move: '{}'", src.display()));
    }
    context.checkpoint()?;

    match rename(src, dst) {
        Ok(()) => {
            warn_directory_sync(dst, "destination", &mut sync_parent);
            if src.parent() != dst.parent() {
                warn_directory_sync(src, "source", &mut sync_parent);
            }
            Ok(())
        }
        Err(error) if error.raw_os_error() == Some(nix::libc::EXDEV) => copy_across_filesystems(
            src,
            dst,
            context,
            &metadata,
            copy_hooks,
            &mut sync_parent,
            remove_source,
        ),
        Err(error) => Err(format!("rename: {error}")),
    }
}

fn copy_across_filesystems(
    src: &Path,
    dst: &Path,
    context: &PluginContext,
    initial_metadata: &fs::Metadata,
    copy_hooks: (impl FnMut(), impl FnOnce()),
    sync_parent: &mut impl FnMut(&Path, &str) -> Result<(), String>,
    remove_source: impl FnOnce(&Path) -> std::io::Result<()>,
) -> Result<(), String> {
    let (mut after_copy_chunk, after_copy_complete) = copy_hooks;
    context.checkpoint()?;
    let mut input = open_private_file(src)
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
    let mut output = create_private_file(&tmp_path).map_err(|e| {
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
            .flush()
            .map_err(|e| format!("cannot flush move temporary file: {e}"))?;
        output
            .sync_all()
            .map_err(|e| format!("cannot sync move temporary file: {e}"))?;
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
        publish_private_path(&tmp_path, dst).map_err(|e| format!("publish copied file: {e}"))?;
        warn_directory_sync(dst, "destination", sync_parent);
        match remove_source(src) {
            Ok(()) => warn_directory_sync(src, "source", sync_parent),
            Err(error) => eprintln!(
                "[monitor] MoveToSent: destination '{}' is canonical but redundant source '{}' could not be removed: {error}",
                dst.display(),
                src.display()
            ),
        }
        Ok(())
    })();

    if copy_result.is_err() {
        let _ = fs::remove_file(&tmp_path);
    }
    copy_result
}

fn warn_directory_sync(
    path: &Path,
    kind: &str,
    sync_parent: &mut impl FnMut(&Path, &str) -> Result<(), String>,
) {
    if let Err(error) = sync_parent(path, kind) {
        eprintln!(
            "[monitor] MoveToSent: {kind} '{}' changed atomically but its parent directory could not be synced: {error}",
            path.display()
        );
    }
}

fn sync_parent_directory(path: &Path, kind: &str) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("{kind} path has no parent: '{}'", path.display()))?;
    open_private_directory(parent)?.sync_all().map_err(|error| {
        format!(
            "cannot sync {kind} directory '{}': {error}",
            parent.display()
        )
    })
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
