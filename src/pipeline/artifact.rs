//! Event-scoped artifact transactions.
//!
//! A report is assembled in a hidden, report-specific staging directory.  All
//! artifact names are registered explicitly, the manifest is written last,
//! and the complete directory is published with one atomic rename.  Readers
//! therefore either see no report directory or one containing a committed
//! manifest and its exact artifact set.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use chrono::Local;
use nix::fcntl::{Flock, FlockArg};
use serde::{Deserialize, Deserializer, Serialize};

use crate::utils::paths::{
    create_private_directory, create_private_file, ensure_private_directory,
    open_private_directory, open_private_file, open_private_file_optional, publish_private_path,
};

use super::{CrashEvent, ReportType};

pub(crate) const MANIFEST_SCHEMA_VERSION: u32 = 1;
pub(crate) const MANIFEST_FILE_NAME: &str = "manifest.json";
const STAGING_PREFIX: &str = ".report-";
const STAGING_SUFFIX: &str = ".pending";
pub(crate) const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_RECOVERY_ROOT_ENTRIES: usize = 10_000;
const MAX_RECOVERY_REPORT_ENTRIES: usize = 10_000;
const MAX_RECOVERY_ARTIFACTS: usize = 10_000;
const RECOVERY_DEADLINE: Duration = Duration::from_secs(2);

/// Reports that have been published but are still being consumed by
/// after-commit processors or notifiers in this process. Retention uses this
/// registry to defer pruning instead of removing a path from under a live
/// consumer. The lease is deliberately process-local: a crashed process has
/// no remaining notifier to protect, and startup recovery may prune normally.
static PUBLICATION_LEASES: OnceLock<Mutex<BTreeSet<PathBuf>>> = OnceLock::new();

#[cfg(test)]
thread_local! {
    static TEST_DIRECTORY_SYNC_FAILURE: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

fn publication_leases() -> &'static Mutex<BTreeSet<PathBuf>> {
    PUBLICATION_LEASES.get_or_init(|| Mutex::new(BTreeSet::new()))
}

fn register_publication_lease(report_dir: &Path) {
    lock(publication_leases()).insert(report_dir.to_path_buf());
}

/// Whether a committed report is still being consumed by a live pipeline in
/// this process.
#[must_use]
pub(crate) fn is_report_publication_leased(report_dir: &Path) -> bool {
    lock(publication_leases()).contains(report_dir)
}

#[cfg(test)]
pub(crate) fn with_test_directory_sync_failure<T>(path: &Path, action: impl FnOnce() -> T) -> T {
    struct Reset(Option<PathBuf>);

    impl Drop for Reset {
        fn drop(&mut self) {
            TEST_DIRECTORY_SYNC_FAILURE.with(|failure_path| {
                failure_path.replace(self.0.take());
            });
        }
    }

    let previous = TEST_DIRECTORY_SYNC_FAILURE
        .with(|failure_path| failure_path.replace(Some(path.to_path_buf())));
    let _reset = Reset(previous);
    action()
}

/// Globally unique identity allocated exactly once for one trigger event.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
pub struct ReportId(String);

impl ReportId {
    #[must_use]
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().simple().to_string())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn parse(value: String) -> Result<Self, String> {
        if value.len() == 32 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            Ok(Self(value))
        } else {
            Err("report id must contain exactly 32 ASCII hexadecimal characters".into())
        }
    }
}

impl Default for ReportId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for ReportId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for ReportId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

/// Immutable paths and identity shared by every stage of one report.
#[derive(Debug)]
pub struct ReportContext {
    report_id: ReportId,
    output_root: PathBuf,
    staging_dir: PathBuf,
    report_type: ReportType,
    pid: u32,
    process_name: String,
}

impl ReportContext {
    #[must_use]
    pub fn new(event: &CrashEvent, output_root: &Path) -> Self {
        let report_id = event.report_id.clone();
        let staging_name = format!("{STAGING_PREFIX}{report_id}{STAGING_SUFFIX}");
        Self {
            staging_dir: output_root.join(staging_name),
            output_root: output_root.to_path_buf(),
            report_id,
            report_type: event.report_type,
            pid: event.pid,
            process_name: event.process_name.clone(),
        }
    }

    #[must_use]
    pub fn report_id(&self) -> &ReportId {
        &self.report_id
    }

    #[must_use]
    pub fn output_root(&self) -> &Path {
        &self.output_root
    }

    #[must_use]
    pub fn staging_dir(&self) -> &Path {
        &self.staging_dir
    }
}

/// Stable artifact role recorded in the exact manifest.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Report,
    ThreadRaw,
    BreadcrumbsRaw,
    ContextRaw,
    ScreenshotRgba,
    ScreenshotPng,
    Attachment,
    Archive,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestArtifact {
    pub path: String,
    pub kind: ArtifactKind,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReportManifest {
    pub schema_version: u32,
    pub report_id: ReportId,
    pub report_type: ReportType,
    pub pid: u32,
    pub process: String,
    pub committed_at: String,
    pub destination: ManifestDestination,
    pub artifacts: Vec<ManifestArtifact>,
}

/// Recovery-safe publication policy.  A prepared report may remain in the
/// pending root, or move to one sibling directory such as `sent/`; arbitrary
/// absolute paths are never trusted from a manifest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ManifestDestination {
    OutputRoot,
    Sibling { directory: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TransactionState {
    Open,
    Preparing,
    Prepared,
    Committed,
}

#[derive(Debug)]
struct TransactionCore {
    artifacts: BTreeMap<String, ArtifactKind>,
    durability_warnings: Vec<String>,
    destination_root: PathBuf,
    state: TransactionState,
    active_operations: usize,
    committed_report: Option<CommittedReport>,
    publication_lease: Option<PathBuf>,
}

/// Mutable artifact registry paired with one immutable [`ReportContext`].
///
/// Mutability is deliberately confined to transaction state; identity and
/// path derivation never change after construction.
#[derive(Debug)]
pub struct ArtifactTransaction {
    report: Arc<ReportContext>,
    core: Mutex<TransactionCore>,
    /// Advisory ownership on the staging directory inode. Recovery and
    /// retention only move directories whose live owner no longer holds it.
    _owner_lock: Flock<File>,
}

#[derive(Clone, Debug)]
pub struct CommittedReport {
    pub report_id: ReportId,
    pub report_dir: PathBuf,
    pub manifest_path: PathBuf,
    /// A directory fsync failed after the atomic rename had already made this
    /// report visible. Publication cannot safely be rolled back at that point.
    pub durability_warnings: Vec<String>,
}

struct TransactionOperation<'a> {
    transaction: &'a ArtifactTransaction,
}

impl Drop for TransactionOperation<'_> {
    fn drop(&mut self) {
        let mut core = lock(&self.transaction.core);
        core.active_operations = core.active_operations.saturating_sub(1);
    }
}

impl ArtifactTransaction {
    /// Create a hidden report-specific staging directory.
    ///
    /// # Errors
    /// Returns an error if the output root or unique staging directory cannot
    /// be created.
    pub fn begin(report: ReportContext) -> Result<Arc<Self>, String> {
        Self::begin_shared(Arc::new(report))
    }

    pub(crate) fn begin_shared(report: Arc<ReportContext>) -> Result<Arc<Self>, String> {
        ensure_real_directory(report.output_root())?;
        create_private_directory(report.staging_dir())?;
        let owner_lock = match try_lock_report_directory(report.staging_dir()) {
            Ok(Some(owner_lock)) => owner_lock,
            Ok(None) => {
                let _ = fs::remove_dir(report.staging_dir());
                return Err(format!(
                    "new report staging directory is unexpectedly locked: '{}'",
                    report.staging_dir().display()
                ));
            }
            Err(error) => {
                let _ = fs::remove_dir(report.staging_dir());
                return Err(error);
            }
        };
        let destination_root = report.output_root().to_path_buf();
        Ok(Arc::new(Self {
            report,
            _owner_lock: owner_lock,
            core: Mutex::new(TransactionCore {
                artifacts: BTreeMap::new(),
                durability_warnings: Vec::new(),
                destination_root,
                state: TransactionState::Open,
                active_operations: 0,
                committed_report: None,
                publication_lease: None,
            }),
        }))
    }

    #[must_use]
    pub fn report_context(&self) -> &ReportContext {
        &self.report
    }

    pub(crate) fn report_context_arc(&self) -> Arc<ReportContext> {
        self.report.clone()
    }

    #[must_use]
    pub fn staging_dir(&self) -> &Path {
        self.report.staging_dir()
    }

    /// Request publication under another report root (for example `sent/`).
    /// The staging directory itself is not exposed or moved until commit.
    ///
    /// # Errors
    /// Returns an error if another transaction operation is active, the
    /// transaction is sealed, or the destination is not an allowed sibling.
    pub fn set_destination_root(&self, destination_root: &Path) -> Result<(), String> {
        let _operation = self.begin_operation()?;
        destination_policy(self.report.output_root(), destination_root)?;
        lock(&self.core).destination_root = destination_root.to_path_buf();
        Ok(())
    }

    /// Atomically write, sync, publish, and register one flat artifact.
    ///
    /// # Errors
    /// Returns an error for an invalid or duplicate name, transaction state
    /// conflict, writer/file-sync failure, or atomic publication failure.
    /// A directory-sync failure after publication is retained as a durability
    /// warning so the registry can still describe the visible artifact exactly.
    pub fn write_artifact(
        &self,
        file_name: &str,
        kind: ArtifactKind,
        write: impl FnOnce(&mut File) -> Result<(), String>,
    ) -> Result<PathBuf, String> {
        self.write_artifact_with_directory_sync(file_name, kind, write, sync_directory)
    }

    fn write_artifact_with_directory_sync(
        &self,
        file_name: &str,
        kind: ArtifactKind,
        write: impl FnOnce(&mut File) -> Result<(), String>,
        sync_staging_directory: impl FnOnce(&Path) -> Result<(), String>,
    ) -> Result<PathBuf, String> {
        let _operation = self.begin_operation()?;
        validate_artifact_name(file_name)?;
        let final_path = self.staging_dir().join(file_name);
        if final_path.exists() {
            return Err(format!(
                "artifact already exists: '{}'",
                final_path.display()
            ));
        }
        let temporary_path = self.staging_dir().join(format!(
            ".{file_name}.{}.artifact.tmp",
            uuid::Uuid::new_v4()
        ));
        let mut temporary = create_private_file(&temporary_path).map_err(|error| {
            format!(
                "cannot create temporary artifact '{}': {error}",
                temporary_path.display()
            )
        })?;

        let write_result = (|| {
            write(&mut temporary)?;
            temporary
                .flush()
                .map_err(|error| format!("cannot flush artifact '{file_name}': {error}"))?;
            temporary
                .sync_all()
                .map_err(|error| format!("cannot sync artifact '{file_name}': {error}"))?;
            drop(temporary);
            publish_private_path(&temporary_path, &final_path).map_err(|error| {
                format!(
                    "cannot publish artifact '{}' as '{}': {error}",
                    temporary_path.display(),
                    final_path.display()
                )
            })?;
            // Publication is the state transition: once the final name exists,
            // make the exact registry agree before any fallible durability
            // operation. The file was created and validated by this method, so
            // insertion cannot fail after publication.
            lock(&self.core)
                .artifacts
                .insert(file_name.to_string(), kind);
            if let Err(error) = sync_staging_directory(self.staging_dir()) {
                self.record_durability_warning(format!(
                    "artifact '{file_name}' was published but its staging directory could not be synced: {error}"
                ));
            }
            Ok(())
        })();

        if write_result.is_err() {
            let _ = fs::remove_file(&temporary_path);
        }
        write_result.map(|()| final_path)
    }

    /// Write one in-memory artifact through the transaction's atomic writer.
    ///
    /// # Errors
    /// Returns the same validation, state, I/O, and durability errors as
    /// [`Self::write_artifact`].
    pub fn write_bytes(
        &self,
        file_name: &str,
        kind: ArtifactKind,
        bytes: &[u8],
    ) -> Result<PathBuf, String> {
        self.write_artifact(file_name, kind, |file| {
            file.write_all(bytes)
                .map_err(|error| format!("cannot write artifact '{file_name}': {error}"))
        })
    }

    /// Register an artifact atomically published by an audited component.
    ///
    /// # Errors
    /// Returns an error if the transaction is busy or sealed, the path is not
    /// a flat staging child, or the child is not a regular file.
    pub fn register_file(&self, path: &Path, kind: ArtifactKind) -> Result<(), String> {
        let _operation = self.begin_operation()?;
        self.register_file_during_operation(path, kind)
    }

    /// Preserve a non-fatal durability failure that occurred after an artifact
    /// had already crossed its atomic publication boundary.
    pub(crate) fn record_durability_warning(&self, warning: String) {
        lock(&self.core).durability_warnings.push(warning);
    }

    fn register_file_during_operation(
        &self,
        path: &Path,
        kind: ArtifactKind,
    ) -> Result<(), String> {
        let name = artifact_name_in(path, self.staging_dir())?;
        let file = open_private_file(path).map_err(|error| {
            format!("cannot safely open artifact '{}': {error}", path.display())
        })?;
        let metadata = file
            .metadata()
            .map_err(|error| format!("cannot inspect artifact '{}': {error}", path.display()))?;
        if !metadata.file_type().is_file() {
            return Err(format!(
                "artifact is not a regular file: '{}'",
                path.display()
            ));
        }
        lock(&self.core).artifacts.insert(name, kind);
        Ok(())
    }

    /// Remove one staging child from the exact artifact registry.
    ///
    /// # Errors
    /// Returns an error if the transaction is busy or sealed, or if the path
    /// is not a valid flat child of this transaction's staging directory.
    pub fn unregister_file(&self, path: &Path) -> Result<(), String> {
        let _operation = self.begin_operation()?;
        let name = artifact_name_in(path, self.staging_dir())?;
        lock(&self.core).artifacts.remove(&name);
        Ok(())
    }

    #[must_use]
    pub fn artifact_paths(&self) -> Vec<PathBuf> {
        lock(&self.core)
            .artifacts
            .keys()
            .map(|name| self.staging_dir().join(name))
            .collect()
    }

    /// Immutable descriptor available after the atomic directory publish.
    #[must_use]
    pub fn committed_report(&self) -> Option<CommittedReport> {
        lock(&self.core).committed_report.clone()
    }

    /// Commit the manifest last and atomically publish the complete directory.
    ///
    /// # Errors
    /// Returns an error if the transaction cannot be sealed, its exact
    /// artifact set is invalid, durability preparation fails, or the final
    /// directory cannot be atomically published.
    pub fn commit(&self) -> Result<CommittedReport, String> {
        self.commit_with_hooks(|| Ok(()), || {})
    }

    #[cfg(test)]
    fn commit_with_hook(
        &self,
        after_manifest_sync: impl FnOnce() -> Result<(), String>,
    ) -> Result<CommittedReport, String> {
        self.commit_with_hooks(after_manifest_sync, || {})
    }

    fn commit_with_hooks(
        &self,
        after_manifest_sync: impl FnOnce() -> Result<(), String>,
        after_directory_publish: impl FnOnce(),
    ) -> Result<CommittedReport, String> {
        self.commit_with_all_hooks(|| {}, after_manifest_sync, || {}, after_directory_publish)
    }

    fn commit_with_all_hooks(
        &self,
        after_begin_commit: impl FnOnce(),
        after_manifest_sync: impl FnOnce() -> Result<(), String>,
        before_directory_publish: impl FnOnce(),
        after_directory_publish: impl FnOnce(),
    ) -> Result<CommittedReport, String> {
        let (registered, destination_root) = self.begin_commit()?;
        after_begin_commit();
        let manifest = self.build_manifest(&registered, &destination_root)?;
        let manifest_json = serde_json::to_vec_pretty(&manifest)
            .map_err(|error| format!("cannot serialize report manifest: {error}"))?;
        if manifest_json.len() as u64 > MAX_MANIFEST_BYTES {
            return Err(format!(
                "report manifest exceeds size limit ({} > {MAX_MANIFEST_BYTES})",
                manifest_json.len()
            ));
        }
        let temporary_manifest = self.staging_dir().join(format!(
            ".{MANIFEST_FILE_NAME}.{}.tmp",
            uuid::Uuid::new_v4()
        ));
        let manifest_path = self.staging_dir().join(MANIFEST_FILE_NAME);
        let mut file = create_private_file(&temporary_manifest)
            .map_err(|error| format!("cannot create temporary report manifest: {error}"))?;
        let prepare_result = (|| -> Result<(), String> {
            file.write_all(&manifest_json)
                .map_err(|error| format!("cannot write report manifest: {error}"))?;
            file.flush()
                .map_err(|error| format!("cannot flush report manifest: {error}"))?;
            file.sync_all()
                .map_err(|error| format!("cannot sync report manifest: {error}"))?;
            drop(file);
            publish_private_path(&temporary_manifest, &manifest_path)
                .map_err(|error| format!("cannot commit report manifest: {error}"))?;
            lock(&self.core).state = TransactionState::Prepared;
            sync_directory(self.staging_dir())?;
            after_manifest_sync()
        })();
        if prepare_result.is_err() {
            let _ = fs::remove_file(&temporary_manifest);
            return prepare_result.map(|()| unreachable!());
        }

        ensure_real_directory(&destination_root)?;
        let report_dir = destination_root.join(self.report.report_id().as_str());
        match fs::symlink_metadata(&report_dir) {
            Ok(_) => {
                return Err(format!(
                    "report destination already exists: '{}'",
                    report_dir.display()
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("cannot inspect report destination: {error}")),
        }
        before_directory_publish();
        publish_private_path(self.staging_dir(), &report_dir).map_err(|error| {
            format!(
                "cannot publish report directory '{}' as '{}': {error}",
                self.staging_dir().display(),
                report_dir.display()
            )
        })?;
        register_publication_lease(&report_dir);
        let mut committed = CommittedReport {
            report_id: self.report.report_id().clone(),
            manifest_path: report_dir.join(MANIFEST_FILE_NAME),
            report_dir,
            durability_warnings: lock(&self.core).durability_warnings.clone(),
        };
        {
            let mut core = lock(&self.core);
            core.state = TransactionState::Committed;
            core.committed_report = Some(committed.clone());
            core.publication_lease = Some(committed.report_dir.clone());
        }
        after_directory_publish();

        if let Err(error) = sync_directory(&destination_root) {
            committed.durability_warnings.push(error);
        }
        if destination_root != self.report.output_root()
            && let Err(error) = sync_directory(self.report.output_root())
        {
            committed.durability_warnings.push(error);
        }
        lock(&self.core).committed_report = Some(committed.clone());

        Ok(committed)
    }

    /// Release the short-lived protection held from atomic publication until
    /// all report-path consumers and final-cleanup processors have completed.
    pub(crate) fn release_publication_lease(&self) {
        let report_dir = lock(&self.core).publication_lease.take();
        if let Some(report_dir) = report_dir {
            lock(publication_leases()).remove(&report_dir);
        }
    }

    fn build_manifest(
        &self,
        registered: &BTreeMap<String, ArtifactKind>,
        destination_root: &Path,
    ) -> Result<ReportManifest, String> {
        validate_exact_directory(self.staging_dir(), registered, false)?;
        let mut artifacts = Vec::new();
        for (name, kind) in registered {
            let path = self.staging_dir().join(name);
            let file = open_private_file(&path).map_err(|error| {
                format!("manifest artifact '{}' is missing: {error}", path.display())
            })?;
            let metadata = file.metadata().map_err(|error| {
                format!(
                    "cannot inspect manifest artifact '{}': {error}",
                    path.display()
                )
            })?;
            file.sync_all()
                .map_err(|error| format!("cannot sync artifact '{}': {error}", path.display()))?;
            artifacts.push(ManifestArtifact {
                path: name.clone(),
                kind: *kind,
                size: metadata.len(),
            });
        }
        Ok(ReportManifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            report_id: self.report.report_id().clone(),
            report_type: self.report.report_type,
            pid: self.report.pid,
            process: self.report.process_name.clone(),
            committed_at: Local::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, false),
            destination: destination_policy(self.report.output_root(), destination_root)?,
            artifacts,
        })
    }

    fn begin_operation(&self) -> Result<TransactionOperation<'_>, String> {
        let mut core = lock(&self.core);
        match core.state {
            TransactionState::Open if core.active_operations == 0 => {
                core.active_operations = 1;
                Ok(TransactionOperation { transaction: self })
            }
            TransactionState::Open => Err("another report transaction operation is active".into()),
            TransactionState::Preparing => Err("report transaction is preparing".into()),
            TransactionState::Prepared => Err("report transaction is already prepared".into()),
            TransactionState::Committed => Err("report transaction is already committed".into()),
        }
    }

    fn begin_commit(&self) -> Result<(BTreeMap<String, ArtifactKind>, PathBuf), String> {
        let mut core = lock(&self.core);
        match core.state {
            TransactionState::Open if core.active_operations == 0 => {
                core.state = TransactionState::Preparing;
                Ok((core.artifacts.clone(), core.destination_root.clone()))
            }
            TransactionState::Open => {
                Err("report transaction still has an active operation".into())
            }
            TransactionState::Preparing => Err("report transaction is preparing".into()),
            TransactionState::Prepared => Err("report transaction is already prepared".into()),
            TransactionState::Committed => Err("report transaction is already committed".into()),
        }
    }
}

impl Drop for ArtifactTransaction {
    fn drop(&mut self) {
        self.release_publication_lease();
        if matches!(
            lock(&self.core).state,
            TransactionState::Open | TransactionState::Preparing
        ) {
            let _ = fs::remove_dir_all(self.staging_dir());
        }
    }
}

/// Recover a transaction that had committed and synced its manifest but was
/// interrupted before the atomic directory publish.
///
/// Incomplete staging directories without a manifest remain hidden.  They are
/// intentionally not deleted here because a different live monitor may still
/// own them; the age/owner policy belongs to the broader startup scavenger.
///
/// # Errors
/// Returns an error when the output root cannot be safely enumerated. Unsafe
/// individual candidates are logged and left hidden for later inspection.
pub fn recover_prepared_reports(output_root: &Path) -> Result<usize, String> {
    match fs::symlink_metadata(output_root) {
        Ok(_) => ensure_private_directory(output_root)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(format!(
                "cannot inspect report output root '{}': {error}",
                output_root.display()
            ));
        }
    }
    recover_prepared_reports_with_limits(output_root, RecoveryLimits::default())
}

#[derive(Clone, Copy)]
struct RecoveryLimits {
    root_entries: usize,
    report_entries: usize,
    artifacts: usize,
    deadline: Duration,
}

impl Default for RecoveryLimits {
    fn default() -> Self {
        Self {
            root_entries: MAX_RECOVERY_ROOT_ENTRIES,
            report_entries: MAX_RECOVERY_REPORT_ENTRIES,
            artifacts: MAX_RECOVERY_ARTIFACTS,
            deadline: RECOVERY_DEADLINE,
        }
    }
}

fn recover_prepared_reports_with_limits(
    output_root: &Path,
    limits: RecoveryLimits,
) -> Result<usize, String> {
    let started = Instant::now();
    let deadline = started.checked_add(limits.deadline).unwrap_or(started);
    let entries = match fs::read_dir(output_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(format!(
                "cannot scan report output root '{}': {error}",
                output_root.display()
            ));
        }
    };
    let mut recovered = 0_usize;
    for (inspected, entry) in entries.enumerate() {
        if inspected >= limits.root_entries {
            eprintln!(
                "[monitor] prepared report recovery stopped at the bounded root-entry limit ({})",
                limits.root_entries
            );
            break;
        }
        if Instant::now() >= deadline {
            eprintln!(
                "[monitor] prepared report recovery stopped at its {}ms startup deadline",
                limits.deadline.as_millis()
            );
            break;
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                eprintln!("[monitor] ignoring unreadable report staging entry: {error}");
                continue;
            }
        };
        match recover_prepared_entry(output_root, &entry, deadline, limits) {
            Ok(true) => recovered = recovered.saturating_add(1),
            Ok(false) => {}
            Err(error) => eprintln!(
                "[monitor] ignoring prepared report candidate '{}': {error}",
                entry.path().display()
            ),
        }
    }
    Ok(recovered)
}

fn recover_prepared_entry(
    output_root: &Path,
    entry: &fs::DirEntry,
    deadline: Instant,
    limits: RecoveryLimits,
) -> Result<bool, String> {
    recover_prepared_entry_with_hook(output_root, entry, deadline, limits, |_| {})
}

fn recover_prepared_entry_with_hook(
    output_root: &Path,
    entry: &fs::DirEntry,
    deadline: Instant,
    limits: RecoveryLimits,
    before_directory_publish: impl FnOnce(&Path),
) -> Result<bool, String> {
    if Instant::now() >= deadline {
        return Ok(false);
    }
    let file_type = entry
        .file_type()
        .map_err(|error| format!("cannot inspect staging entry type: {error}"))?;
    if !file_type.is_dir() {
        return Ok(false);
    }
    let name = entry.file_name();
    let name = name.to_string_lossy();
    let Some(report_id) = staging_report_id(name.as_ref()) else {
        return Ok(false);
    };
    let Some(_owner_lock) = try_lock_report_directory(&entry.path())? else {
        return Ok(false);
    };
    let manifest_path = entry.path().join(MANIFEST_FILE_NAME);
    let manifest_bytes = match read_manifest_file(&manifest_path) {
        Ok(bytes) => bytes,
        Err(ManifestReadError::NotFound) => return Ok(false),
        Err(ManifestReadError::Unsafe(error)) => return Err(error),
    };
    let manifest: ReportManifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|error| format!("malformed prepared manifest: {error}"))?;
    if manifest.schema_version != MANIFEST_SCHEMA_VERSION
        || manifest.report_id.as_str() != report_id
    {
        return Err("prepared manifest identity mismatch".into());
    }
    if manifest.artifacts.len() > limits.artifacts {
        return Err(format!(
            "prepared manifest contains more than {} artifacts",
            limits.artifacts
        ));
    }
    let registered = manifest_registry(&manifest)?;
    validate_exact_directory_bounded(
        &entry.path(),
        &registered,
        true,
        limits.report_entries,
        Some(deadline),
    )?;
    validate_manifest_sizes(&entry.path(), &manifest, Some(deadline))?;
    let destination_root = destination_from_policy(output_root, &manifest.destination)?;
    ensure_real_directory(&destination_root)?;
    let destination = destination_root.join(report_id);
    match fs::symlink_metadata(&destination) {
        Ok(_) => {
            return Err(format!(
                "report destination already exists: '{}'",
                destination.display()
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("cannot inspect report destination: {error}")),
    }
    ensure_before_deadline(Some(deadline))?;
    before_directory_publish(&destination);
    publish_private_path(&entry.path(), &destination).map_err(|error| {
        format!(
            "cannot publish prepared directory as '{}': {error}",
            destination.display()
        )
    })?;

    // Rename is the publication point. Durability failures are warnings and
    // must not erase the recovered count or stop later candidates.
    if let Err(error) = sync_directory(&destination_root) {
        eprintln!(
            "[monitor] recovered report '{}' but destination durability sync failed: {error}",
            destination.display()
        );
    }
    if destination_root != output_root
        && let Err(error) = sync_directory(output_root)
    {
        eprintln!(
            "[monitor] recovered report '{}' but staging-root durability sync failed: {error}",
            destination.display()
        );
    }
    Ok(true)
}

/// Load one bounded, no-follow manifest.
///
/// # Errors
/// Returns an error if the manifest is missing, unsafe, oversized, unreadable,
/// or does not deserialize into the supported schema.
pub fn load_manifest(path: &Path) -> Result<ReportManifest, String> {
    let bytes = read_manifest_file(path).map_err(|error| match error {
        ManifestReadError::NotFound => {
            format!(
                "cannot read report manifest '{}': not found",
                path.display()
            )
        }
        ManifestReadError::Unsafe(error) => {
            format!("cannot read report manifest '{}': {error}", path.display())
        }
    })?;
    serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid report manifest '{}': {error}", path.display()))
}

fn artifact_name_in(path: &Path, directory: &Path) -> Result<String, String> {
    if path.parent() != Some(directory) {
        return Err(format!(
            "artifact '{}' is outside report staging directory '{}'",
            path.display(),
            directory.display()
        ));
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("artifact has no valid filename: '{}'", path.display()))?;
    validate_artifact_name(name)?;
    Ok(name.to_string())
}

fn validate_artifact_name(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name == MANIFEST_FILE_NAME
        || name.starts_with('.')
        || Path::new(name).components().count() != 1
    {
        return Err(format!("invalid report artifact name: {name:?}"));
    }
    Ok(())
}

fn destination_policy(
    output_root: &Path,
    destination_root: &Path,
) -> Result<ManifestDestination, String> {
    if destination_root == output_root {
        return Ok(ManifestDestination::OutputRoot);
    }
    let output_parent = output_root
        .parent()
        .ok_or_else(|| "report output root has no parent".to_string())?;
    if destination_root.parent() != Some(output_parent) {
        return Err(format!(
            "report destination '{}' is not a sibling of output root '{}'",
            destination_root.display(),
            output_root.display()
        ));
    }
    let directory = destination_root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "report destination has no valid directory name".to_string())?;
    validate_directory_component(directory)?;
    Ok(ManifestDestination::Sibling {
        directory: directory.to_string(),
    })
}

fn destination_from_policy(
    output_root: &Path,
    destination: &ManifestDestination,
) -> Result<PathBuf, String> {
    match destination {
        ManifestDestination::OutputRoot => Ok(output_root.to_path_buf()),
        ManifestDestination::Sibling { directory } => {
            validate_directory_component(directory)?;
            let parent = output_root
                .parent()
                .ok_or_else(|| "report output root has no parent".to_string())?;
            Ok(parent.join(directory))
        }
    }
}

fn validate_directory_component(component: &str) -> Result<(), String> {
    let path = Path::new(component);
    if component.is_empty()
        || component.starts_with('.')
        || path.components().count() != 1
        || !matches!(
            path.components().next(),
            Some(std::path::Component::Normal(_))
        )
    {
        return Err(format!(
            "invalid report destination component: {component:?}"
        ));
    }
    Ok(())
}

fn manifest_registry(manifest: &ReportManifest) -> Result<BTreeMap<String, ArtifactKind>, String> {
    let mut registered = BTreeMap::new();
    for artifact in &manifest.artifacts {
        validate_artifact_name(&artifact.path)?;
        if registered
            .insert(artifact.path.clone(), artifact.kind)
            .is_some()
        {
            return Err(format!("duplicate manifest artifact: {:?}", artifact.path));
        }
    }
    Ok(registered)
}

fn validate_exact_directory(
    directory: &Path,
    registered: &BTreeMap<String, ArtifactKind>,
    manifest_expected: bool,
) -> Result<(), String> {
    validate_exact_directory_bounded(
        directory,
        registered,
        manifest_expected,
        MAX_RECOVERY_REPORT_ENTRIES,
        None,
    )
}

fn validate_exact_directory_bounded(
    directory: &Path,
    registered: &BTreeMap<String, ArtifactKind>,
    manifest_expected: bool,
    max_entries: usize,
    deadline: Option<Instant>,
) -> Result<(), String> {
    let mut actual = BTreeMap::<String, u64>::new();
    let mut saw_manifest = false;
    let entries = fs::read_dir(directory)
        .map_err(|error| format!("cannot enumerate report directory: {error}"))?;
    for (inspected, entry) in entries.enumerate() {
        ensure_before_deadline(deadline)?;
        if inspected >= max_entries {
            return Err(format!(
                "report directory contains more than {max_entries} entries"
            ));
        }
        let entry =
            entry.map_err(|error| format!("cannot read report directory entry: {error}"))?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| "report directory contains a non-UTF-8 name".to_string())?;
        if manifest_expected && name == MANIFEST_FILE_NAME {
            open_private_file(&entry.path())
                .map_err(|error| format!("cannot safely inspect report manifest: {error}"))?;
            saw_manifest = true;
            continue;
        }
        let file = open_private_file(&entry.path())
            .map_err(|error| format!("cannot safely inspect report artifact {name:?}: {error}"))?;
        let metadata = file
            .metadata()
            .map_err(|error| format!("cannot inspect report artifact {name:?}: {error}"))?;
        actual.insert(name, metadata.len());
    }
    let expected: Vec<&String> = registered.keys().collect();
    let found: Vec<&String> = actual.keys().collect();
    if expected != found {
        return Err(format!(
            "report artifact set differs from manifest: expected {expected:?}, found {found:?}"
        ));
    }
    if manifest_expected && !saw_manifest {
        return Err("prepared report has no manifest".to_string());
    }
    Ok(())
}

fn validate_manifest_sizes(
    directory: &Path,
    manifest: &ReportManifest,
    deadline: Option<Instant>,
) -> Result<(), String> {
    for artifact in &manifest.artifacts {
        ensure_before_deadline(deadline)?;
        let path = directory.join(&artifact.path);
        let file = open_private_file(&path)
            .map_err(|error| format!("cannot safely open manifest artifact: {error}"))?;
        let metadata = file
            .metadata()
            .map_err(|error| format!("cannot inspect manifest artifact: {error}"))?;
        if !metadata.file_type().is_file() {
            return Err(format!(
                "manifest artifact is not a regular file: {:?}",
                artifact.path
            ));
        }
        let size = metadata.len();
        if size != artifact.size {
            return Err(format!(
                "manifest size mismatch for {:?}: expected {}, found {size}",
                artifact.path, artifact.size
            ));
        }
    }
    Ok(())
}

enum ManifestReadError {
    NotFound,
    Unsafe(String),
}

fn read_manifest_file(path: &Path) -> Result<Vec<u8>, ManifestReadError> {
    let file = open_private_file_optional(path)
        .map_err(|error| {
            ManifestReadError::Unsafe(format!("manifest cannot be opened safely: {error}"))
        })?
        .ok_or(ManifestReadError::NotFound)?;
    let metadata = file
        .metadata()
        .map_err(|error| ManifestReadError::Unsafe(format!("cannot inspect manifest: {error}")))?;
    if !metadata.file_type().is_file() {
        return Err(ManifestReadError::Unsafe(
            "manifest is not a regular file".to_string(),
        ));
    }
    if metadata.len() > MAX_MANIFEST_BYTES {
        return Err(ManifestReadError::Unsafe(format!(
            "manifest exceeds size limit ({} > {MAX_MANIFEST_BYTES})",
            metadata.len()
        )));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    file.take(MAX_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| ManifestReadError::Unsafe(format!("cannot read manifest: {error}")))?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(ManifestReadError::Unsafe(
            "manifest grew beyond size limit while reading".to_string(),
        ));
    }
    Ok(bytes)
}

fn ensure_before_deadline(deadline: Option<Instant>) -> Result<(), String> {
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        Err("prepared report recovery deadline elapsed".into())
    } else {
        Ok(())
    }
}

/// Acquire advisory ownership of a real directory without following a final
/// symlink. `Ok(None)` means another cooperating monitor currently owns it.
pub(crate) fn try_lock_report_directory(path: &Path) -> Result<Option<Flock<File>>, String> {
    let directory = open_private_directory(path)?;
    let metadata = directory.metadata().map_err(|error| {
        format!(
            "cannot inspect report directory '{}': {error}",
            path.display()
        )
    })?;
    if !metadata.file_type().is_dir() {
        return Err(format!(
            "report path is not a real directory: '{}'",
            path.display()
        ));
    }
    match Flock::lock(directory, FlockArg::LockExclusiveNonblock) {
        Ok(owner_lock) => Ok(Some(owner_lock)),
        Err((_directory, nix::errno::Errno::EAGAIN)) => Ok(None),
        Err((_directory, error)) => Err(format!(
            "cannot lock report directory '{}': {error}",
            path.display()
        )),
    }
}

fn ensure_real_directory(path: &Path) -> Result<(), String> {
    let existed = match fs::symlink_metadata(path) {
        Ok(_) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => {
            return Err(format!(
                "cannot inspect report destination '{}': {error}",
                path.display()
            ));
        }
    };
    ensure_private_directory(path)?;
    if !existed {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        // The private-directory helper already persists each creation. Keep
        // this pipeline-level parent sync as an explicit pre-publication
        // durability boundary (and as the injectable failure point in tests).
        sync_directory(parent)?;
    }
    Ok(())
}

fn staging_report_id(name: &str) -> Option<&str> {
    let id = name
        .strip_prefix(STAGING_PREFIX)?
        .strip_suffix(STAGING_SUFFIX)?;
    (id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit())).then_some(id)
}

fn sync_directory(path: &Path) -> Result<(), String> {
    #[cfg(test)]
    if TEST_DIRECTORY_SYNC_FAILURE
        .with(|failure_path| failure_path.borrow().as_deref() == Some(path))
    {
        return Err(format!(
            "cannot sync directory '{}': simulated directory sync failure",
            path.display()
        ));
    }

    open_private_directory(path)?
        .sync_all()
        .map_err(|error| format!("cannot sync directory '{}': {error}", path.display()))
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::{CrashEvent, ReportType};

    fn event() -> CrashEvent {
        CrashEvent {
            report_id: ReportId::new(),
            report_type: ReportType::Crash,
            termination: None,
            exception_type: None,
            exception_code: None,
            exception_subcode: None,
            exception_codes: Vec::new(),
            crashed_thread: None,
            bail_on_suspend_failure: false,
            pid: 1234,
            process_name: "fixture".into(),
            hang_duration_ms: None,
        }
    }

    #[test]
    fn commit_publishes_exact_manifest_and_no_staging_directory() {
        let root = tempfile::tempdir().unwrap();
        let event = event();
        let transaction =
            ArtifactTransaction::begin(ReportContext::new(&event, root.path())).unwrap();
        transaction
            .write_bytes("report.json", ArtifactKind::Report, b"{}")
            .unwrap();
        transaction
            .write_bytes("thread.raw", ArtifactKind::ThreadRaw, b"raw")
            .unwrap();

        let committed = transaction.commit().unwrap();
        let manifest = load_manifest(&committed.manifest_path).unwrap();

        assert_eq!(manifest.report_id, event.report_id);
        assert_eq!(manifest.schema_version, MANIFEST_SCHEMA_VERSION);
        assert_eq!(manifest.report_type, ReportType::Crash);
        assert_eq!(manifest.pid, 1234);
        assert_eq!(manifest.process, "fixture");
        assert_eq!(manifest.destination, ManifestDestination::OutputRoot);
        assert_eq!(
            manifest
                .artifacts
                .iter()
                .map(|artifact| (artifact.path.as_str(), artifact.kind, artifact.size))
                .collect::<Vec<_>>(),
            vec![
                ("report.json", ArtifactKind::Report, 2),
                ("thread.raw", ArtifactKind::ThreadRaw, 3),
            ]
        );
        assert!(committed.report_dir.join("report.json").exists());
        let mut final_names = std::fs::read_dir(&committed.report_dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        final_names.sort();
        assert_eq!(final_names, ["manifest.json", "report.json", "thread.raw"]);
        assert!(!transaction.staging_dir().exists());
    }

    #[test]
    fn published_artifact_directory_sync_failure_is_committed_as_warning() {
        let root = tempfile::tempdir().unwrap();
        let event = event();
        let transaction =
            ArtifactTransaction::begin(ReportContext::new(&event, root.path())).unwrap();

        let path = transaction
            .write_artifact_with_directory_sync(
                "report.json",
                ArtifactKind::Report,
                |file| file.write_all(b"{}").map_err(|error| error.to_string()),
                |_| Err("injected staging directory sync failure".into()),
            )
            .unwrap();

        assert_eq!(transaction.artifact_paths(), vec![path]);
        let committed = transaction.commit().unwrap();
        assert!(
            committed
                .durability_warnings
                .iter()
                .any(|warning| warning.contains("injected staging directory sync failure"))
        );
        let manifest = load_manifest(&committed.manifest_path).unwrap();
        assert_eq!(manifest.artifacts.len(), 1);
        assert_eq!(manifest.artifacts[0].path, "report.json");
        assert!(committed.report_dir.join("report.json").is_file());
    }

    #[test]
    fn load_manifest_rejects_symlinks_and_oversized_files() {
        let root = tempfile::tempdir().unwrap();
        ensure_private_directory(root.path()).unwrap();
        let target = root.path().join("target.json");
        std::fs::write(&target, b"{}").unwrap();
        let symlink = root.path().join("manifest-symlink.json");
        std::os::unix::fs::symlink(&target, &symlink).unwrap();

        let symlink_error = load_manifest(&symlink).unwrap_err();
        assert!(symlink_error.contains("opened safely"), "{symlink_error}");

        let oversized = root.path().join("manifest-oversized.json");
        let file = std::fs::File::create(&oversized).unwrap();
        file.set_len(MAX_MANIFEST_BYTES + 1).unwrap();
        let oversized_error = load_manifest(&oversized).unwrap_err();
        assert!(
            oversized_error.contains("exceeds size limit"),
            "{oversized_error}"
        );
    }

    #[test]
    fn prepared_manifest_is_recovered_after_publish_interruption() {
        let root = tempfile::tempdir().unwrap();
        let pending = root.path().join("pending");
        let sent = root.path().join("sent");
        std::fs::create_dir(&pending).unwrap();
        let first_event = event();
        let report_id = first_event.report_id.clone();
        let transaction =
            ArtifactTransaction::begin(ReportContext::new(&first_event, &pending)).unwrap();
        transaction.set_destination_root(&sent).unwrap();
        transaction
            .write_bytes("report.json", ArtifactKind::Report, b"{}")
            .unwrap();

        let error = transaction
            .commit_with_hook(|| Err("simulated termination after manifest fsync".into()))
            .unwrap_err();
        assert!(error.contains("simulated termination"));
        assert!(!sent.join(report_id.as_str()).exists());
        assert!(transaction.staging_dir().join(MANIFEST_FILE_NAME).exists());
        drop(transaction);

        assert_eq!(recover_prepared_reports(&pending).unwrap(), 1);
        let report_dir = sent.join(report_id.as_str());
        assert!(report_dir.join(MANIFEST_FILE_NAME).exists());
        assert!(report_dir.join("report.json").exists());
        assert_eq!(recover_prepared_reports(&pending).unwrap(), 0);
    }

    #[test]
    fn recovery_publish_race_never_replaces_a_competing_destination() {
        let root = tempfile::tempdir().unwrap();
        let pending = root.path().join("pending");
        let sent = root.path().join("sent");
        std::fs::create_dir(&pending).unwrap();
        let event = event();
        let report_id = event.report_id.clone();
        let transaction = ArtifactTransaction::begin(ReportContext::new(&event, &pending)).unwrap();
        transaction.set_destination_root(&sent).unwrap();
        transaction
            .write_bytes("report.json", ArtifactKind::Report, b"{}")
            .unwrap();
        transaction
            .commit_with_hook(|| Err("pause after prepare".into()))
            .unwrap_err();
        let staging = transaction.staging_dir().to_path_buf();
        drop(transaction);

        let entry = std::fs::read_dir(&pending)
            .unwrap()
            .map(Result::unwrap)
            .find(|entry| entry.path() == staging)
            .unwrap();
        let error = recover_prepared_entry_with_hook(
            &pending,
            &entry,
            Instant::now() + Duration::from_secs(1),
            RecoveryLimits::default(),
            |destination| {
                std::fs::create_dir(destination).unwrap();
                std::fs::write(destination.join("sentinel"), b"preserve").unwrap();
            },
        )
        .unwrap_err();

        let destination = sent.join(report_id.as_str());
        assert!(error.contains("exclusively publish"), "{error}");
        assert_eq!(
            std::fs::read(destination.join("sentinel")).unwrap(),
            b"preserve"
        );
        assert!(staging.join(MANIFEST_FILE_NAME).is_file());
    }

    #[test]
    fn manifest_rename_is_preserved_when_staging_directory_sync_fails() {
        let root = tempfile::tempdir().unwrap();
        let event = event();
        let report_id = event.report_id.clone();
        let transaction =
            ArtifactTransaction::begin(ReportContext::new(&event, root.path())).unwrap();
        transaction
            .write_bytes("report.json", ArtifactKind::Report, b"{}")
            .unwrap();

        let error = with_test_directory_sync_failure(transaction.staging_dir(), || {
            transaction.commit().unwrap_err()
        });
        assert!(
            error.contains("simulated directory sync failure"),
            "{error}"
        );
        assert!(transaction.staging_dir().join(MANIFEST_FILE_NAME).is_file());
        let staging = transaction.staging_dir().to_path_buf();
        drop(transaction);

        assert!(staging.join(MANIFEST_FILE_NAME).is_file());
        assert_eq!(recover_prepared_reports(root.path()).unwrap(), 1);
        assert!(
            root.path()
                .join(report_id.as_str())
                .join(MANIFEST_FILE_NAME)
                .is_file()
        );
    }

    #[test]
    fn newly_created_sibling_destination_syncs_its_parent_before_publish() {
        let root = tempfile::tempdir().unwrap();
        let pending = root.path().join("pending");
        let sent = root.path().join("sent");
        std::fs::create_dir(&pending).unwrap();
        let event = event();
        let transaction = ArtifactTransaction::begin(ReportContext::new(&event, &pending)).unwrap();
        transaction.set_destination_root(&sent).unwrap();
        transaction
            .write_bytes("report.json", ArtifactKind::Report, b"{}")
            .unwrap();

        let error =
            with_test_directory_sync_failure(root.path(), || transaction.commit().unwrap_err());

        assert!(
            error.contains("simulated directory sync failure"),
            "{error}"
        );
        assert!(sent.is_dir());
        assert!(!sent.join(event.report_id.as_str()).exists());
        assert!(transaction.staging_dir().join(MANIFEST_FILE_NAME).is_file());
    }

    #[test]
    fn recovery_skips_a_live_prepared_owner_then_recovers_after_drop() {
        let root = tempfile::tempdir().unwrap();
        let pending = root.path().join("pending");
        let sent = root.path().join("sent");
        std::fs::create_dir(&pending).unwrap();
        let first_event = event();
        let report_id = first_event.report_id.clone();
        let transaction =
            ArtifactTransaction::begin(ReportContext::new(&first_event, &pending)).unwrap();
        transaction.set_destination_root(&sent).unwrap();
        transaction
            .write_bytes("report.json", ArtifactKind::Report, b"{}")
            .unwrap();
        transaction
            .commit_with_hook(|| Err("pause after prepare".into()))
            .unwrap_err();

        assert_eq!(recover_prepared_reports(&pending).unwrap(), 0);
        assert!(!sent.join(report_id.as_str()).exists());

        drop(transaction);
        assert_eq!(recover_prepared_reports(&pending).unwrap(), 1);
        assert!(
            sent.join(report_id.as_str())
                .join(MANIFEST_FILE_NAME)
                .is_file()
        );
    }

    #[test]
    fn recovery_rejects_symlink_manifest_and_truncated_artifact() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let pending = root.path().join("pending");
        std::fs::create_dir(&pending).unwrap();

        let linked = pending.join(format!(
            "{STAGING_PREFIX}{}{STAGING_SUFFIX}",
            "0".repeat(32)
        ));
        std::fs::create_dir(&linked).unwrap();
        let outside = root.path().join("outside-manifest.json");
        std::fs::write(&outside, b"{}").unwrap();
        symlink(&outside, linked.join(MANIFEST_FILE_NAME)).unwrap();

        let first_event = event();
        let report_id = first_event.report_id.clone();
        let transaction =
            ArtifactTransaction::begin(ReportContext::new(&first_event, &pending)).unwrap();
        transaction
            .write_bytes("report.json", ArtifactKind::Report, b"original")
            .unwrap();
        transaction
            .commit_with_hook(|| Err("pause after prepare".into()))
            .unwrap_err();
        let staging = transaction.staging_dir().to_path_buf();
        drop(transaction);
        std::fs::write(staging.join("report.json"), b"x").unwrap();

        assert_eq!(recover_prepared_reports(&pending).unwrap(), 0);
        assert!(linked.exists());
        assert!(staging.exists());
        assert!(!pending.join(report_id.as_str()).exists());
    }

    #[test]
    fn recovery_limits_stop_without_exposing_a_candidate() {
        let root = tempfile::tempdir().unwrap();
        let pending = root.path().join("pending");
        std::fs::create_dir(&pending).unwrap();
        let first_event = event();
        let report_id = first_event.report_id.clone();
        let transaction =
            ArtifactTransaction::begin(ReportContext::new(&first_event, &pending)).unwrap();
        transaction
            .write_bytes("report.json", ArtifactKind::Report, b"{}")
            .unwrap();
        transaction
            .commit_with_hook(|| Err("pause after prepare".into()))
            .unwrap_err();
        drop(transaction);

        let recovered = recover_prepared_reports_with_limits(
            &pending,
            RecoveryLimits {
                root_entries: 0,
                report_entries: 1,
                artifacts: 1,
                deadline: Duration::from_secs(1),
            },
        )
        .unwrap();

        assert_eq!(recovered, 0);
        assert!(!pending.join(report_id.as_str()).exists());
    }

    #[test]
    fn uncommitted_partial_report_is_never_visible() {
        let root = tempfile::tempdir().unwrap();
        let event = event();
        let report_id = event.report_id.clone();
        let transaction =
            ArtifactTransaction::begin(ReportContext::new(&event, root.path())).unwrap();
        transaction
            .write_bytes("report.json", ArtifactKind::Report, b"partial")
            .unwrap();

        assert!(!root.path().join(report_id.as_str()).exists());
        assert_eq!(recover_prepared_reports(root.path()).unwrap(), 0);
        drop(transaction);
        assert!(!root.path().join(report_id.as_str()).exists());
    }

    #[test]
    fn commit_rejects_unregistered_staging_file() {
        let root = tempfile::tempdir().unwrap();
        let event = event();
        let transaction =
            ArtifactTransaction::begin(ReportContext::new(&event, root.path())).unwrap();
        transaction
            .write_bytes("report.json", ArtifactKind::Report, b"{}")
            .unwrap();
        std::fs::write(transaction.staging_dir().join("unregistered.bin"), b"extra").unwrap();

        let error = transaction.commit().unwrap_err();
        assert!(error.contains("artifact set differs"), "{error}");
        assert!(!root.path().join(event.report_id.as_str()).exists());
    }

    #[test]
    fn malformed_prepared_entry_does_not_block_valid_recovery() {
        let root = tempfile::tempdir().unwrap();
        let pending = root.path().join("pending");
        std::fs::create_dir(&pending).unwrap();
        let malformed = pending.join(format!(
            "{STAGING_PREFIX}{}{STAGING_SUFFIX}",
            "0".repeat(32)
        ));
        std::fs::create_dir(&malformed).unwrap();
        std::fs::write(malformed.join(MANIFEST_FILE_NAME), b"not-json").unwrap();

        let first_event = event();
        let report_id = first_event.report_id.clone();
        let transaction =
            ArtifactTransaction::begin(ReportContext::new(&first_event, &pending)).unwrap();
        transaction
            .write_bytes("report.json", ArtifactKind::Report, b"{}")
            .unwrap();
        transaction
            .commit_with_hook(|| Err("interrupt".into()))
            .unwrap_err();
        drop(transaction);

        let second_event = event();
        let second_report_id = second_event.report_id.clone();
        let second =
            ArtifactTransaction::begin(ReportContext::new(&second_event, &pending)).unwrap();
        second
            .write_bytes("report.json", ArtifactKind::Report, b"{\"second\":true}")
            .unwrap();
        second
            .commit_with_hook(|| Err("interrupt".into()))
            .unwrap_err();
        drop(second);

        assert_eq!(recover_prepared_reports(&pending).unwrap(), 2);
        assert!(
            pending
                .join(report_id.as_str())
                .join("manifest.json")
                .exists()
        );
        assert!(
            pending
                .join(second_report_id.as_str())
                .join(MANIFEST_FILE_NAME)
                .exists()
        );
        assert!(malformed.exists());
    }

    #[test]
    fn report_id_deserialization_rejects_path_components_and_wrong_lengths() {
        assert!(serde_json::from_str::<ReportId>(r#""../../escape""#).is_err());
        assert!(serde_json::from_str::<ReportId>(r#""abcd""#).is_err());
        let valid = "0123456789abcdef0123456789ABCDEF";
        assert_eq!(
            serde_json::from_str::<ReportId>(&format!(r#""{valid}""#))
                .unwrap()
                .as_str(),
            valid
        );
    }

    #[test]
    fn commit_rejects_concurrent_artifact_operation_without_waiting() {
        let root = tempfile::tempdir().unwrap();
        let transaction =
            ArtifactTransaction::begin(ReportContext::new(&event(), root.path())).unwrap();
        let writer_transaction = transaction.clone();
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let writer = std::thread::spawn(move || {
            writer_transaction.write_artifact("report.json", ArtifactKind::Report, |file| {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                file.write_all(b"{}")
                    .map_err(|error| format!("write test artifact: {error}"))
            })
        });
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("writer did not enter the transaction within five seconds");

        let started = std::time::Instant::now();
        let error = transaction.commit().unwrap_err();
        assert!(error.contains("active operation"), "{error}");
        assert!(started.elapsed() < std::time::Duration::from_millis(100));

        release_tx.send(()).unwrap();
        writer.join().unwrap().unwrap();
        assert!(transaction.commit().is_ok());
    }

    #[test]
    fn concurrent_commit_attempts_have_exactly_one_winner() {
        let root = tempfile::tempdir().unwrap();
        let transaction =
            ArtifactTransaction::begin(ReportContext::new(&event(), root.path())).unwrap();
        transaction
            .write_bytes("report.json", ArtifactKind::Report, b"{}")
            .unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let transaction = transaction.clone();
            let barrier = barrier.clone();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                transaction.commit()
            }));
        }
        barrier.wait();
        let outcomes = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();

        assert_eq!(outcomes.iter().filter(|outcome| outcome.is_ok()).count(), 1);
        assert_eq!(
            outcomes.iter().filter(|outcome| outcome.is_err()).count(),
            1
        );
        let error = outcomes.into_iter().find_map(Result::err).unwrap();
        assert!(
            error.contains("preparing") || error.contains("already committed"),
            "{error}"
        );
    }

    #[test]
    fn committed_transaction_rejects_late_writer() {
        let root = tempfile::tempdir().unwrap();
        let transaction =
            ArtifactTransaction::begin(ReportContext::new(&event(), root.path())).unwrap();
        transaction
            .write_bytes("report.json", ArtifactKind::Report, b"{}")
            .unwrap();
        transaction.commit().unwrap();

        let error = transaction
            .write_bytes("late.bin", ArtifactKind::Attachment, b"late")
            .unwrap_err();
        assert!(error.contains("already committed"), "{error}");
    }

    #[test]
    fn preparing_transaction_rejects_late_writer_without_waiting() {
        let root = tempfile::tempdir().unwrap();
        let transaction =
            ArtifactTransaction::begin(ReportContext::new(&event(), root.path())).unwrap();
        transaction
            .write_bytes("report.json", ArtifactKind::Report, b"{}")
            .unwrap();
        let committing = transaction.clone();
        let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(1);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let worker = std::thread::spawn(move || {
            committing.commit_with_all_hooks(
                || {
                    entered_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                },
                || Ok(()),
                || {},
                || {},
            )
        });
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("commit did not enter Preparing within five seconds");

        let started = std::time::Instant::now();
        let error = transaction
            .write_bytes("late.bin", ArtifactKind::Attachment, b"late")
            .unwrap_err();
        assert!(error.contains("preparing"), "{error}");
        assert!(started.elapsed() < std::time::Duration::from_millis(100));

        release_tx.send(()).unwrap();
        worker.join().unwrap().unwrap();
    }

    #[test]
    fn commit_refuses_to_replace_an_existing_report_directory() {
        let root = tempfile::tempdir().unwrap();
        let event = event();
        let transaction =
            ArtifactTransaction::begin(ReportContext::new(&event, root.path())).unwrap();
        transaction
            .write_bytes("report.json", ArtifactKind::Report, b"{}")
            .unwrap();
        let existing = root.path().join(event.report_id.as_str());
        std::fs::create_dir(&existing).unwrap();
        std::fs::write(existing.join("sentinel"), b"preserve").unwrap();

        let error = transaction.commit().unwrap_err();

        assert!(error.contains("already exists"), "{error}");
        assert_eq!(
            std::fs::read(existing.join("sentinel")).unwrap(),
            b"preserve"
        );
    }

    #[test]
    fn commit_publish_race_never_replaces_a_competing_destination() {
        let root = tempfile::tempdir().unwrap();
        let event = event();
        let transaction =
            ArtifactTransaction::begin(ReportContext::new(&event, root.path())).unwrap();
        transaction
            .write_bytes("report.json", ArtifactKind::Report, b"{}")
            .unwrap();
        let destination = root.path().join(event.report_id.as_str());

        let error = transaction
            .commit_with_all_hooks(
                || {},
                || Ok(()),
                || {
                    std::fs::create_dir(&destination).unwrap();
                    std::fs::write(destination.join("sentinel"), b"preserve").unwrap();
                },
                || {},
            )
            .unwrap_err();

        assert!(error.contains("exclusively publish"), "{error}");
        assert_eq!(
            std::fs::read(destination.join("sentinel")).unwrap(),
            b"preserve"
        );
        assert!(transaction.staging_dir().join(MANIFEST_FILE_NAME).is_file());
    }

    #[test]
    fn commit_refuses_to_follow_an_existing_destination_symlink() {
        let root = tempfile::tempdir().unwrap();
        let event = event();
        let transaction =
            ArtifactTransaction::begin(ReportContext::new(&event, root.path())).unwrap();
        transaction
            .write_bytes("report.json", ArtifactKind::Report, b"{}")
            .unwrap();
        let outside = root.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("sentinel"), b"preserve").unwrap();
        let destination = root.path().join(event.report_id.as_str());
        std::os::unix::fs::symlink(&outside, &destination).unwrap();

        let error = transaction.commit().unwrap_err();

        assert!(error.contains("already exists"), "{error}");
        assert!(
            std::fs::symlink_metadata(destination)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read(outside.join("sentinel")).unwrap(),
            b"preserve"
        );
    }

    #[test]
    fn recovery_refuses_to_follow_an_existing_destination_symlink() {
        let root = tempfile::tempdir().unwrap();
        let pending = root.path().join("pending");
        let sent = root.path().join("sent");
        let event = event();
        let transaction = ArtifactTransaction::begin(ReportContext::new(&event, &pending)).unwrap();
        transaction.set_destination_root(&sent).unwrap();
        transaction
            .write_bytes("report.json", ArtifactKind::Report, b"{}")
            .unwrap();
        transaction
            .commit_with_hook(|| Err("interrupt before publish".into()))
            .unwrap_err();
        drop(transaction);

        let outside = root.path().join("outside");
        std::fs::create_dir(&sent).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("sentinel"), b"preserve").unwrap();
        let destination = sent.join(event.report_id.as_str());
        std::os::unix::fs::symlink(&outside, &destination).unwrap();

        assert_eq!(recover_prepared_reports(&pending).unwrap(), 0);
        assert!(
            std::fs::symlink_metadata(destination)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            std::fs::read(outside.join("sentinel")).unwrap(),
            b"preserve"
        );
    }

    #[test]
    fn commit_and_recovery_preserve_broken_destination_symlinks() {
        let commit_root = tempfile::tempdir().unwrap();
        let commit_event = event();
        let transaction =
            ArtifactTransaction::begin(ReportContext::new(&commit_event, commit_root.path()))
                .unwrap();
        transaction
            .write_bytes("report.json", ArtifactKind::Report, b"{}")
            .unwrap();
        let commit_destination = commit_root.path().join(commit_event.report_id.as_str());
        std::os::unix::fs::symlink(commit_root.path().join("missing"), &commit_destination)
            .unwrap();

        assert!(transaction.commit().unwrap_err().contains("already exists"));
        assert!(
            std::fs::symlink_metadata(&commit_destination)
                .unwrap()
                .file_type()
                .is_symlink()
        );

        let recovery_root = tempfile::tempdir().unwrap();
        let pending = recovery_root.path().join("pending");
        let sent = recovery_root.path().join("sent");
        let recovery_event = event();
        let recovery =
            ArtifactTransaction::begin(ReportContext::new(&recovery_event, &pending)).unwrap();
        recovery.set_destination_root(&sent).unwrap();
        recovery
            .write_bytes("report.json", ArtifactKind::Report, b"{}")
            .unwrap();
        recovery
            .commit_with_hook(|| Err("interrupt before publish".into()))
            .unwrap_err();
        drop(recovery);
        std::fs::create_dir(&sent).unwrap();
        let recovery_destination = sent.join(recovery_event.report_id.as_str());
        std::os::unix::fs::symlink(recovery_root.path().join("missing"), &recovery_destination)
            .unwrap();

        assert_eq!(recover_prepared_reports(&pending).unwrap(), 0);
        assert!(
            std::fs::symlink_metadata(recovery_destination)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    #[ignore = "spawned explicitly to simulate abrupt process termination"]
    fn hard_kill_process_helper() {
        let Ok(mode) = std::env::var("CRASH_MONITOR_P013_KILL_MODE") else {
            return;
        };
        let root = PathBuf::from(std::env::var_os("CRASH_MONITOR_P013_KILL_ROOT").unwrap());
        let report_id =
            ReportId::parse(std::env::var("CRASH_MONITOR_P013_KILL_REPORT_ID").unwrap()).unwrap();
        let mut event = event();
        event.report_id = report_id;
        let pending = root.join("pending");
        let sent = root.join("sent");
        std::fs::create_dir_all(&pending).unwrap();
        let transaction = ArtifactTransaction::begin(ReportContext::new(&event, &pending)).unwrap();
        transaction
            .write_bytes("report.json", ArtifactKind::Report, b"{}")
            .unwrap();

        match mode.as_str() {
            "partial" => std::process::exit(81),
            "prepared" => {
                transaction.set_destination_root(&sent).unwrap();
                let _ = transaction.commit_with_hook(|| std::process::exit(82));
            }
            "published" => {
                transaction.set_destination_root(&sent).unwrap();
                let _ = transaction.commit_with_hooks(|| Ok(()), || std::process::exit(83));
            }
            other => panic!("unknown hard-kill mode: {other}"),
        }
    }

    #[test]
    fn abrupt_exit_keeps_partial_hidden_and_restart_recovers_prepared_report() {
        fn run_helper(root: &Path, mode: &str, report_id: &ReportId, expected_code: i32) {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "pipeline::artifact::tests::hard_kill_process_helper",
                    "--ignored",
                    "--nocapture",
                ])
                .env("CRASH_MONITOR_P013_KILL_MODE", mode)
                .env("CRASH_MONITOR_P013_KILL_ROOT", root)
                .env("CRASH_MONITOR_P013_KILL_REPORT_ID", report_id.as_str())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let status = loop {
                if let Some(status) = child.try_wait().unwrap() {
                    break status;
                }
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    panic!("hard-kill helper mode {mode:?} exceeded five seconds");
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            };
            let mut stderr = String::new();
            if let Some(mut pipe) = child.stderr.take() {
                std::io::Read::read_to_string(&mut pipe, &mut stderr).unwrap();
            }
            assert_eq!(
                status.code(),
                Some(expected_code),
                "helper stderr: {stderr}"
            );
        }

        let partial_root = tempfile::tempdir().unwrap();
        let partial_id = ReportId::new();
        run_helper(partial_root.path(), "partial", &partial_id, 81);
        let partial_pending = partial_root.path().join("pending");
        assert!(!partial_pending.join(partial_id.as_str()).exists());
        assert!(
            partial_pending
                .join(format!("{STAGING_PREFIX}{partial_id}{STAGING_SUFFIX}"))
                .exists()
        );
        assert_eq!(recover_prepared_reports(&partial_pending).unwrap(), 0);
        assert!(!partial_pending.join(partial_id.as_str()).exists());

        let prepared_root = tempfile::tempdir().unwrap();
        let prepared_id = ReportId::new();
        run_helper(prepared_root.path(), "prepared", &prepared_id, 82);
        let prepared_pending = prepared_root.path().join("pending");
        let prepared_sent = prepared_root.path().join("sent");
        assert!(!prepared_sent.join(prepared_id.as_str()).exists());
        assert_eq!(recover_prepared_reports(&prepared_pending).unwrap(), 1);
        assert!(
            prepared_sent
                .join(prepared_id.as_str())
                .join(MANIFEST_FILE_NAME)
                .exists()
        );

        let published_root = tempfile::tempdir().unwrap();
        let published_id = ReportId::new();
        run_helper(published_root.path(), "published", &published_id, 83);
        let published_pending = published_root.path().join("pending");
        let published_sent = published_root.path().join("sent");
        let published_dir = published_sent.join(published_id.as_str());
        assert!(published_dir.join(MANIFEST_FILE_NAME).exists());
        assert!(published_dir.join("report.json").exists());
        assert_eq!(recover_prepared_reports(&published_pending).unwrap(), 0);
    }
}
