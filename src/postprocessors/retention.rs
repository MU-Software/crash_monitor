//! Post-processor: enforce retention policy on archived reports.
//!
//! Operates on the current report's final publication directory (normally
//! `sent/`), pruning oldest logical reports when any threshold is exceeded. A
//! committed report is one UUID directory whose manifest exactly names its
//! artifacts; it is atomically hidden before bounded flat deletion. Legacy flat
//! report files remain supported during migration.
//!
//! Thresholds:
//! - count > `max_count`
//! - total size > `max_total_bytes`
//! - age > `max_age_days`

use crate::pipeline::artifact::{
    MANIFEST_FILE_NAME, MANIFEST_SCHEMA_VERSION, MAX_MANIFEST_BYTES, is_report_publication_leased,
    try_lock_report_directory,
};
use crate::pipeline::{
    CrashEvent, Plugin, PluginContext, PluginExecution, PostProcessor, PostProcessorPhase,
    Priority, ReportManifest, ReportResult,
};
use crate::utils::paths::{
    create_private_file, ensure_private_directory, open_private_directory, open_private_file,
    publish_private_path, validate_private_file,
};
use nix::errno::Errno;
use nix::fcntl::{Flock, FlockArg};
use std::collections::BTreeMap;
use std::fs;
use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, TryLockError};
use std::thread;
use std::time::{Duration, SystemTime};

const MAX_RETENTION_SCAN_ENTRIES: usize = 10_000;
const MAX_REPORT_ENTRIES: usize = 10_000;
const RETENTION_LOCK_FILE_NAME: &str = ".retention.lock";
const RETENTION_LOCK_POLL_INTERVAL: Duration = Duration::from_millis(2);

static RETENTION_PROCESS_LOCK: Mutex<()> = Mutex::new(());

pub struct RetentionManager {
    max_count: usize,
    max_total_bytes: u64,
    max_age: Duration,
    /// Override target directory path (for testing).
    dir_override: Option<PathBuf>,
}

impl RetentionManager {
    #[must_use]
    pub fn new(max_count: usize, max_size_mb: u64, max_age_days: u64) -> Self {
        Self {
            max_count,
            max_total_bytes: max_size_mb.saturating_mul(1024 * 1024),
            max_age: Duration::from_secs(max_age_days.saturating_mul(86400)),
            dir_override: None,
        }
    }

    /// Create with explicit directory (for testing). Same units as `new()`.
    #[cfg(test)]
    #[must_use]
    pub fn with_dir(max_count: usize, max_size_mb: u64, max_age_days: u64, dir: PathBuf) -> Self {
        Self {
            max_count,
            max_total_bytes: max_size_mb.saturating_mul(1024 * 1024),
            max_age: Duration::from_secs(max_age_days.saturating_mul(86400)),
            dir_override: Some(dir),
        }
    }

    fn target_dir(&self, context: &PluginContext) -> Result<PathBuf, String> {
        match &self.dir_override {
            Some(p) => Ok(p.clone()),
            None => context
                .committed_report()
                .and_then(|report| report.report_dir.parent().map(Path::to_path_buf))
                .map_or_else(
                    || crate::utils::paths::sent_dir().map_err(|e| format!("sent_dir: {e}")),
                    Ok,
                ),
        }
    }
}

impl Plugin for RetentionManager {
    fn name(&self) -> &'static str {
        "RetentionManager"
    }
    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }
    fn priority(&self) -> Priority {
        Priority::Low
    }
    fn order_after(&self) -> &'static [&'static str] {
        // ZIPArchiver remains a direct constraint when MoveToSent is disabled.
        &["ZIPArchiver", "MoveToSent"]
    }
}

#[derive(Clone, Copy)]
enum ReportEntryKind {
    LegacyFile,
    CommittedDirectory,
}

struct ReportEntry {
    path: PathBuf,
    size: u64,
    modified: SystemTime,
    kind: ReportEntryKind,
    device: u64,
    inode: u64,
}

impl PostProcessor for RetentionManager {
    fn phase(&self) -> PostProcessorPhase {
        PostProcessorPhase::FinalCleanup
    }

    fn process(
        &self,
        _event: &CrashEvent,
        _result: &mut ReportResult,
        context: &PluginContext,
    ) -> Result<(), String> {
        context.checkpoint()?;
        let dir = self.target_dir(context)?;
        if let Some(transaction) = context.artifact_transaction() {
            crate::pipeline::artifact::scavenge_stale_pending(
                transaction.report_context().output_root(),
                self.max_age,
            )
            .map_err(|error| error.to_string())?;
        }
        let current_report_dir = context.committed_report().map(|report| report.report_dir);
        validate_retention_root(&dir)?;
        let _retention_lock = acquire_retention_lock(&dir, context)?;
        cleanup_retention_tombstones(&dir, context)?;

        let mut entries = collect_entries(&dir, context)?;
        if entries.is_empty() {
            return Ok(());
        }

        // Sort oldest first (by modification time)
        entries.sort_by_key(|e| e.modified);

        let now = SystemTime::now();

        // Pass 1: delete logical reports older than max_age. Never jump past a
        // leased or failed oldest candidate: doing so could delete a newer
        // report while another finalizer still needs the older one.
        while let Some(entry) = entries.first() {
            context.checkpoint()?;
            let age = now.duration_since(entry.modified).unwrap_or(Duration::ZERO);
            if age <= self.max_age || entry_is_leased(entry, current_report_dir.as_deref()) {
                break;
            }
            if !remove_entry(entry, current_report_dir.as_deref(), context) {
                break;
            }
            entries.remove(0);
        }

        // Pass 2: FinalCleanup runs after every report-path consumer, but the
        // publication lease remains active through this terminal phase. Keep
        // the current report and every foreign live finalizer protected.
        while entries.len() > self.max_count {
            context.checkpoint()?;
            let oldest = &entries[0];
            if entry_is_leased(oldest, current_report_dir.as_deref())
                || !remove_entry(oldest, current_report_dir.as_deref(), context)
            {
                break;
            }
            entries.remove(0);
        }

        // Pass 3: enforce the byte limit over complete committed reports.
        let mut total = 0_u64;
        for entry in &entries {
            context.checkpoint()?;
            total = total.saturating_add(entry.size);
        }
        while total > self.max_total_bytes && !entries.is_empty() {
            context.checkpoint()?;
            let oldest = &entries[0];
            if entry_is_leased(oldest, current_report_dir.as_deref())
                || !remove_entry(oldest, current_report_dir.as_deref(), context)
            {
                break;
            }
            total = total.saturating_sub(oldest.size);
            entries.remove(0);
        }

        context.checkpoint()?;
        let oldest_exceeds_age = entries.first().is_some_and(|entry| {
            now.duration_since(entry.modified).unwrap_or(Duration::ZERO) > self.max_age
        });
        if entries.len() > self.max_count || total > self.max_total_bytes || oldest_exceeds_age {
            return Err(format!(
                "retention quota is deferred by a live lease or failed oldest deletion (count {}/{}, bytes {}/{}, oldest_over_age={oldest_exceeds_age})",
                entries.len(),
                self.max_count,
                total,
                self.max_total_bytes
            ));
        }
        Ok(())
    }
}

fn entry_is_leased(entry: &ReportEntry, current_report_dir: Option<&Path>) -> bool {
    matches!(entry.kind, ReportEntryKind::CommittedDirectory)
        && (current_report_dir == Some(entry.path.as_path())
            || is_report_publication_leased(&entry.path))
}

fn collect_entries(
    dir: &std::path::Path,
    context: &PluginContext,
) -> Result<Vec<ReportEntry>, String> {
    collect_entries_bounded(dir, context, MAX_RETENTION_SCAN_ENTRIES)
}

fn collect_entries_bounded(
    dir: &Path,
    context: &PluginContext,
    max_entries: usize,
) -> Result<Vec<ReportEntry>, String> {
    context.checkpoint()?;
    validate_retention_root(dir)?;
    let read_dir =
        fs::read_dir(dir).map_err(|e| format!("cannot read '{}': {e}", dir.display()))?;

    let mut entries = Vec::new();
    for (inspected, entry) in read_dir.enumerate() {
        context.checkpoint()?;
        if inspected >= max_entries {
            return Err(format!(
                "retention scan exceeds the bounded limit of {max_entries} entries"
            ));
        }
        let entry = entry.map_err(|error| format!("cannot read retention entry: {error}"))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            return Err("retention root contains a non-UTF-8 entry".into());
        };
        if name.starts_with('.') {
            continue;
        }
        let file_type = entry
            .file_type()
            .map_err(|error| format!("cannot inspect retention entry {name:?}: {error}"))?;
        if file_type.is_file() {
            let file = open_private_file(&entry.path()).map_err(|error| {
                format!("cannot validate legacy retention entry {name:?}: {error}")
            })?;
            let metadata = file.metadata().map_err(|error| {
                format!("cannot inspect legacy retention entry {name:?}: {error}")
            })?;
            let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
            entries.push(ReportEntry {
                path: entry.path(),
                size: metadata.len(),
                modified,
                kind: ReportEntryKind::LegacyFile,
                device: metadata.dev(),
                inode: metadata.ino(),
            });
        } else if file_type.is_dir() && is_report_id(name) {
            entries.push(
                inspect_committed_report(&entry.path(), context).map_err(|error| {
                    format!(
                        "cannot safely inventory committed report '{}': {error}",
                        entry.path().display()
                    )
                })?,
            );
        } else if file_type.is_symlink() {
            return Err(format!("retention root contains a symlink entry: {name:?}"));
        }
    }
    Ok(entries)
}

struct RetentionLock {
    _process: MutexGuard<'static, ()>,
    _file: Flock<fs::File>,
}

fn validate_retention_root(dir: &Path) -> Result<(), String> {
    let metadata = fs::symlink_metadata(dir)
        .map_err(|error| format!("cannot inspect retention root '{}': {error}", dir.display()))?;
    if !metadata.file_type().is_dir() {
        return Err(format!(
            "retention root is not a real directory: '{}'",
            dir.display()
        ));
    }
    ensure_private_directory(dir)
        .map_err(|error| format!("retention root is not private '{}': {error}", dir.display()))
}

fn acquire_retention_lock(dir: &Path, context: &PluginContext) -> Result<RetentionLock, String> {
    let process = loop {
        context.checkpoint()?;
        match RETENTION_PROCESS_LOCK.try_lock() {
            Ok(lock) => break lock,
            Err(TryLockError::Poisoned(poisoned)) => break poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => thread::sleep(RETENTION_LOCK_POLL_INTERVAL),
        }
    };

    let lock_path = dir.join(RETENTION_LOCK_FILE_NAME);
    let (mut file, created) = open_retention_lock_file(&lock_path)?;
    if created {
        open_private_directory(dir)?
            .sync_all()
            .map_err(|error| format!("cannot sync retention root: {error}"))?;
    }

    loop {
        context.checkpoint()?;
        match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
            Ok(file_lock) => {
                return Ok(RetentionLock {
                    _process: process,
                    _file: file_lock,
                });
            }
            Err((returned_file, error)) if error == Errno::EWOULDBLOCK => {
                file = returned_file;
                thread::sleep(RETENTION_LOCK_POLL_INTERVAL);
            }
            Err((_returned_file, error)) => {
                return Err(format!(
                    "cannot acquire retention lock '{}': {error}",
                    lock_path.display()
                ));
            }
        }
    }
}

fn open_retention_lock_file(path: &Path) -> Result<(fs::File, bool), String> {
    match open_existing_retention_lock(path) {
        Ok(file) => {
            validate_private_file(&file, path).map_err(|error| {
                format!(
                    "cannot safely open retention lock '{}': {error}",
                    path.display()
                )
            })?;
            Ok((file, false))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match create_private_file(path) {
                Ok(file) => Ok((file, true)),
                Err(create_error) => {
                    let file = open_existing_retention_lock(path).map_err(|open_error| {
                        format!(
                            "cannot safely create retention lock '{}': {create_error}; retry open failed: {open_error}",
                            path.display()
                        )
                    })?;
                    validate_private_file(&file, path).map_err(|validation_error| {
                        format!(
                            "cannot safely open retention lock '{}': {validation_error}",
                            path.display()
                        )
                    })?;
                    Ok((file, false))
                }
            }
        }
        Err(error) => Err(format!(
            "cannot safely open retention lock '{}': {error}",
            path.display()
        )),
    }
}

fn open_existing_retention_lock(path: &Path) -> std::io::Result<fs::File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)
}

fn cleanup_retention_tombstones(dir: &Path, context: &PluginContext) -> Result<(), String> {
    let mut removed_any = false;
    for (inspected, entry) in fs::read_dir(dir)
        .map_err(|error| format!("cannot scan retention tombstones: {error}"))?
        .enumerate()
    {
        context.checkpoint()?;
        if inspected >= MAX_RETENTION_SCAN_ENTRIES {
            return Err(format!(
                "retention tombstone scan exceeds the bounded limit of {MAX_RETENTION_SCAN_ENTRIES} entries"
            ));
        }
        let entry = entry.map_err(|error| format!("cannot read retention tombstone: {error}"))?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !is_retention_tombstone_name(&name) {
            continue;
        }
        let file_type = entry
            .file_type()
            .map_err(|error| format!("cannot inspect retention tombstone {name:?}: {error}"))?;
        if !file_type.is_dir() {
            continue;
        }
        remove_stale_tombstone(&entry.path(), context)?;
        removed_any = true;
    }
    if removed_any {
        sync_directory(dir)?;
    }
    Ok(())
}

fn is_retention_tombstone_name(name: &str) -> bool {
    let Some(value) = name
        .strip_prefix(".retention-")
        .and_then(|name| name.strip_suffix(".deleting"))
    else {
        return false;
    };
    let Some((report_id, nonce)) = value.split_once('.') else {
        return false;
    };
    !nonce.contains('.') && is_report_id(report_id) && is_report_id(nonce)
}

fn remove_stale_tombstone(tombstone: &Path, context: &PluginContext) -> Result<(), String> {
    for (inspected, entry) in fs::read_dir(tombstone)
        .map_err(|error| format!("cannot enumerate stale retention tombstone: {error}"))?
        .enumerate()
    {
        context.checkpoint()?;
        if inspected >= MAX_REPORT_ENTRIES {
            return Err(format!(
                "retention tombstone contains more than {MAX_REPORT_ENTRIES} entries"
            ));
        }
        let entry = entry.map_err(|error| format!("cannot read tombstone entry: {error}"))?;
        let file_type = entry
            .file_type()
            .map_err(|error| format!("cannot inspect tombstone entry: {error}"))?;
        if file_type.is_dir() {
            return Err(format!(
                "retention tombstone contains an unexpected directory: '{}'",
                entry.path().display()
            ));
        }
        fs::remove_file(entry.path())
            .map_err(|error| format!("cannot remove stale tombstone entry: {error}"))?;
    }
    context.checkpoint()?;
    fs::remove_dir(tombstone)
        .map_err(|error| format!("cannot remove stale retention tombstone: {error}"))
}

fn inspect_committed_report(
    report_dir: &Path,
    context: &PluginContext,
) -> Result<ReportEntry, String> {
    context.checkpoint()?;
    let report_name = report_dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "report directory has no UTF-8 name".to_string())?;
    if !is_report_id(report_name) || report_name.starts_with('.') {
        return Err("report directory name is not a canonical report id".into());
    }
    let initial_metadata = fs::symlink_metadata(report_dir)
        .map_err(|error| format!("cannot inspect report directory: {error}"))?;
    if !initial_metadata.file_type().is_dir() {
        return Err("report path is not a real directory".into());
    }
    ensure_private_directory(report_dir)
        .map_err(|error| format!("report directory is not private: {error}"))?;
    let report_directory = open_private_directory(report_dir)?;
    let initial_metadata = report_directory
        .metadata()
        .map_err(|error| format!("cannot inspect opened report directory: {error}"))?;

    let mut expected = manifest_artifact_sizes(report_dir, report_name)?;

    let mut total_size = 0_u64;
    let mut saw_manifest = false;
    for (inspected, entry) in fs::read_dir(report_dir)
        .map_err(|error| format!("cannot enumerate report directory: {error}"))?
        .enumerate()
    {
        context.checkpoint()?;
        if inspected >= MAX_REPORT_ENTRIES {
            return Err(format!(
                "report contains more than {MAX_REPORT_ENTRIES} entries"
            ));
        }
        let entry = entry.map_err(|error| format!("cannot read report entry: {error}"))?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "report contains a non-UTF-8 entry".to_string())?;
        let file = open_private_file(&entry.path())
            .map_err(|error| format!("cannot validate report entry {name:?}: {error}"))?;
        let metadata = file
            .metadata()
            .map_err(|error| format!("cannot inspect report entry {name:?}: {error}"))?;
        if name == MANIFEST_FILE_NAME {
            if saw_manifest {
                return Err("report contains duplicate manifest entries".into());
            }
            saw_manifest = true;
            total_size = total_size.saturating_add(metadata.len());
            continue;
        }
        let Some(expected_size) = expected.remove(&name) else {
            return Err(format!("unmanifested report artifact: {name:?}"));
        };
        if metadata.len() != expected_size {
            return Err(format!(
                "manifest size mismatch for {name:?}: expected {expected_size}, found {}",
                metadata.len()
            ));
        }
        total_size = total_size.saturating_add(metadata.len());
    }
    if !saw_manifest {
        return Err("report has no manifest".into());
    }
    if !expected.is_empty() {
        return Err(format!(
            "manifest artifacts are missing from report: {:?}",
            expected.keys().collect::<Vec<_>>()
        ));
    }

    let final_metadata = fs::symlink_metadata(report_dir)
        .map_err(|error| format!("cannot re-inspect report directory: {error}"))?;
    if !final_metadata.file_type().is_dir()
        || final_metadata.dev() != initial_metadata.dev()
        || final_metadata.ino() != initial_metadata.ino()
    {
        return Err("report directory changed while it was inspected".into());
    }

    Ok(ReportEntry {
        path: report_dir.to_path_buf(),
        size: total_size,
        modified: final_metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH),
        kind: ReportEntryKind::CommittedDirectory,
        device: final_metadata.dev(),
        inode: final_metadata.ino(),
    })
}

fn manifest_artifact_sizes(
    report_dir: &Path,
    report_name: &str,
) -> Result<BTreeMap<String, u64>, String> {
    let manifest_path = report_dir.join(MANIFEST_FILE_NAME);
    let manifest_bytes = read_manifest_no_follow(&manifest_path)?;
    let manifest: ReportManifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|error| format!("invalid report manifest: {error}"))?;
    if manifest.schema_version != MANIFEST_SCHEMA_VERSION {
        return Err(format!(
            "unsupported manifest schema version: {}",
            manifest.schema_version
        ));
    }
    if manifest.report_id.as_str() != report_name {
        return Err("manifest report id does not match its directory".into());
    }

    let mut expected = BTreeMap::new();
    for artifact in manifest.artifacts {
        validate_artifact_name(&artifact.path)?;
        if expected.insert(artifact.path, artifact.size).is_some() {
            return Err("manifest contains a duplicate artifact".into());
        }
    }
    Ok(expected)
}

fn read_manifest_no_follow(path: &Path) -> Result<Vec<u8>, String> {
    let file =
        open_private_file(path).map_err(|error| format!("cannot safely open manifest: {error}"))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("cannot inspect manifest: {error}"))?;
    if !metadata.file_type().is_file() {
        return Err("manifest is not a regular file".into());
    }
    if metadata.len() > MAX_MANIFEST_BYTES {
        return Err(format!(
            "manifest exceeds size limit ({} > {MAX_MANIFEST_BYTES})",
            metadata.len()
        ));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    file.take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read manifest: {error}"))?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err("manifest grew beyond its size limit while being read".into());
    }
    Ok(bytes)
}

fn validate_artifact_name(name: &str) -> Result<(), String> {
    let path = Path::new(name);
    if name.is_empty()
        || name == MANIFEST_FILE_NAME
        || name.starts_with('.')
        || path.components().count() != 1
        || !matches!(
            path.components().next(),
            Some(std::path::Component::Normal(_))
        )
    {
        return Err(format!("invalid manifest artifact name: {name:?}"));
    }
    Ok(())
}

fn is_report_id(name: &str) -> bool {
    name.len() == 32 && name.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn remove_entry(
    entry: &ReportEntry,
    current_report_dir: Option<&Path>,
    context: &PluginContext,
) -> bool {
    let result = (|| -> Result<(), String> {
        context.checkpoint()?;
        let metadata = fs::symlink_metadata(&entry.path)
            .map_err(|error| format!("cannot re-inspect retention entry: {error}"))?;
        if metadata.dev() != entry.device || metadata.ino() != entry.inode {
            return Err("entry changed after retention scan".into());
        }
        match entry.kind {
            ReportEntryKind::LegacyFile => {
                if !metadata.file_type().is_file() {
                    return Err("legacy report is no longer a regular file".into());
                }
                fs::remove_file(&entry.path)
                    .map_err(|error| format!("cannot remove legacy report: {error}"))?;
                sync_parent_directory(&entry.path)
            }
            ReportEntryKind::CommittedDirectory => {
                if !metadata.file_type().is_dir() {
                    return Err("committed report is no longer a real directory".into());
                }
                ensure_private_directory(&entry.path).map_err(|error| {
                    format!("committed report directory is not private: {error}")
                })?;
                // The current transaction already owns this directory's
                // advisory lock. Other reports must be acquired independently
                // so a live finalizer in this or another process stays safe.
                let _report_lock = if current_report_dir == Some(entry.path.as_path()) {
                    None
                } else {
                    let Some(report_lock) = try_lock_report_directory(&entry.path)? else {
                        return Err("committed report is owned by another live monitor".into());
                    };
                    Some(report_lock)
                };
                let current = inspect_committed_report(&entry.path, context)?;
                if current.device != entry.device || current.inode != entry.inode {
                    return Err("committed report changed before deletion".into());
                }
                let parent = entry
                    .path
                    .parent()
                    .ok_or_else(|| "committed report has no parent directory".to_string())?;
                let report_name = entry
                    .path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .ok_or_else(|| "committed report has no UTF-8 name".to_string())?;
                let artifact_names = manifest_artifact_sizes(&entry.path, report_name)?
                    .into_keys()
                    .collect::<Vec<_>>();
                let tombstone = parent.join(format!(
                    ".retention-{report_name}.{}.deleting",
                    uuid::Uuid::new_v4().simple()
                ));
                publish_private_path(&entry.path, &tombstone).map_err(|error| {
                    format!("cannot atomically hide committed report before deletion: {error}")
                })?;

                // Once the atomic rename succeeds, no subset of the report is
                // visible as a committed report. Cleanup failures therefore
                // leave only a hidden tombstone for the next serialized
                // retention pass rather than an exposed artifact fragment.
                if let Err(error) = sync_directory(parent) {
                    eprintln!(
                        "[monitor] RetentionManager: cannot sync hidden report deletion '{}': {error}",
                        entry.path.display()
                    );
                }
                if let Err(error) = remove_flat_tombstone(&tombstone, &artifact_names, context) {
                    eprintln!(
                        "[monitor] RetentionManager: hidden deletion tombstone '{}' needs scavenging: {error}",
                        tombstone.display()
                    );
                } else if let Err(error) = sync_directory(parent) {
                    eprintln!(
                        "[monitor] RetentionManager: cannot sync tombstone cleanup '{}': {error}",
                        tombstone.display()
                    );
                }
                Ok(())
            }
        }
    })();
    if let Err(error) = result {
        eprintln!(
            "[monitor] RetentionManager: failed to delete '{}': {error}",
            entry.path.display()
        );
        false
    } else {
        true
    }
}

fn remove_flat_tombstone(
    tombstone: &Path,
    artifact_names: &[String],
    context: &PluginContext,
) -> Result<(), String> {
    for name in artifact_names {
        context.checkpoint()?;
        let path = tombstone.join(name);
        let metadata = fs::symlink_metadata(&path)
            .map_err(|error| format!("cannot inspect tombstoned artifact {name:?}: {error}"))?;
        if !metadata.file_type().is_file() {
            return Err(format!(
                "tombstoned artifact is not a regular file: {name:?}"
            ));
        }
        fs::remove_file(&path)
            .map_err(|error| format!("cannot remove tombstoned artifact {name:?}: {error}"))?;
    }

    context.checkpoint()?;
    let manifest_path = tombstone.join(MANIFEST_FILE_NAME);
    let manifest_metadata = fs::symlink_metadata(&manifest_path)
        .map_err(|error| format!("cannot inspect tombstoned manifest: {error}"))?;
    if !manifest_metadata.file_type().is_file() {
        return Err("tombstoned manifest is not a regular file".into());
    }
    fs::remove_file(&manifest_path)
        .map_err(|error| format!("cannot remove tombstoned manifest: {error}"))?;

    context.checkpoint()?;
    let mut remaining = fs::read_dir(tombstone)
        .map_err(|error| format!("cannot verify tombstone is empty: {error}"))?;
    if remaining.next().is_some() {
        return Err("tombstone contains an unexpected entry".into());
    }
    fs::remove_dir(tombstone)
        .map_err(|error| format!("cannot remove empty report tombstone: {error}"))
}

fn sync_parent_directory(path: &Path) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("retention entry has no parent: '{}'", path.display()))?;
    sync_directory(parent)
}

fn sync_directory(path: &Path) -> Result<(), String> {
    open_private_directory(path)?
        .sync_all()
        .map_err(|error| format!("cannot sync directory '{}': {error}", path.display()))
}

#[cfg(test)]
#[path = "../../tests/unit/postprocessors/retention_tests.rs"]
mod tests;
