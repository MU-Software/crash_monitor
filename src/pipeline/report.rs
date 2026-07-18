//! Crash/snapshot report generation (JSON serialization only).
//! No raw data type imports — receives pre-formatted data from `report_formatter`.

use crate::platform;
use crate::preprocessors::report_formatter;
use crate::utils::paths::{
    create_private_file, open_private_directory, open_private_file, publish_private_path,
};

use chrono::Local;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use zip::write::SimpleFileOptions;

use super::artifact::{
    ArtifactKind, ArtifactTransaction, MANIFEST_FILE_NAME, MANIFEST_SCHEMA_VERSION, ReportId,
    load_manifest,
};
use super::types::{
    CollectedData, CrashEvent, Diagnostics, ReportType, TerminationEvidence, TerminationReason,
};

pub const REPORT_SCHEMA_VERSION: u32 = 1;

// ═══════════════════════════════════════════════════
//  Serde report structures (design doc lines 1026-1153)
// ═══════════════════════════════════════════════════

#[derive(Serialize, Deserialize)]
pub struct CrashReport {
    pub header: ReportHeader,
    /// Process termination metadata for exit/signal failure reports.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub termination: Option<TerminationReason>,
    #[serde(default)]
    pub build: Option<BuildReport>,
    #[serde(default)]
    pub exception: Option<ExceptionReport>,
    #[serde(default)]
    pub crash_context: Option<CrashContextReport>,
    #[serde(default)]
    pub threads: Vec<ThreadReport>,
    #[serde(default)]
    pub breadcrumbs: Option<Vec<BreadcrumbReport>>,
    #[serde(default)]
    pub loaded_images: Vec<LoadedImageReport>,
    #[serde(default)]
    pub memory_map: Vec<VmRegionReport>,
    #[serde(default)]
    pub heap_summary: Option<HeapSummary>,
    #[serde(default)]
    pub session: Option<SessionReport>,
    #[serde(default)]
    pub settings_snapshot: Option<SettingsSnapshotReport>,
    #[serde(default)]
    pub fingerprint: Option<String>,
    #[serde(default)]
    pub environment: Option<EnvironmentReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_output: Option<crate::platform::ChildOutputSnapshot>,
    #[serde(default)]
    pub attachments: Vec<serde_json::Value>,
    #[serde(default)]
    pub user_feedback: Option<serde_json::Value>,
    #[serde(rename = "_diagnostics")]
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub diagnostics: Option<serde_json::Value>,
}

#[derive(Serialize, Deserialize)]
pub struct ReportHeader {
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report_id: Option<ReportId>,
    pub timestamp: String,
    pub pid: u32,
    pub process: String,
    pub collector: String,
    #[serde(rename = "type")]
    pub report_type: ReportType,
    /// Confidence classification for a SIGKILL-derived termination report.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub termination_evidence: Option<TerminationEvidence>,
    /// ANR-specific: trigger description (e.g., `watchdog_5s`).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub trigger: Option<String>,
    /// ANR-specific: how long the app was unresponsive (milliseconds).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub hang_duration_ms: Option<u64>,
}

impl ReportHeader {
    fn new(event: &CrashEvent) -> Self {
        let (trigger, hang_duration_ms) = match event.report_type {
            ReportType::Anr => (
                Some(format!(
                    "watchdog_{}s",
                    event.hang_duration_ms.unwrap_or(0) / 1000
                )),
                event.hang_duration_ms,
            ),
            ReportType::Oom => (Some("sigkill".to_string()), None),
            ReportType::Crash
            | ReportType::Snapshot
            | ReportType::ExitFailure
            | ReportType::SignalFailure => (None, None),
        };
        let termination_evidence = match (event.report_type, event.termination) {
            (ReportType::Oom, Some(TerminationReason::Signaled { signal: 9, .. })) => {
                Some(TerminationEvidence::PossibleOom)
            }
            (ReportType::SignalFailure, Some(TerminationReason::Signaled { signal: 9, .. })) => {
                Some(TerminationEvidence::UnknownSigkill)
            }
            _ => None,
        };

        Self {
            version: REPORT_SCHEMA_VERSION,
            report_id: Some(event.report_id.clone()),
            timestamp: Local::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, false),
            pid: event.pid,
            process: event.process_name.clone(),
            collector: "crash_monitor".into(),
            report_type: event.report_type,
            termination_evidence,
            trigger,
            hang_duration_ms,
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct BuildReport {
    pub app_version: String,
    pub build_number: u32,
    pub git_hash: String,
    pub git_dirty: bool,
    pub build_type: String,
    pub build_preset: String,
    pub build_timestamp: String,
    pub compiler: String,
    pub os: String,
}

#[derive(Serialize, Deserialize)]
pub struct CrashContextReport {
    #[serde(default)]
    pub source: ReportValueSource,
    #[serde(default)]
    pub annotations: BTreeMap<String, serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_start_ns: Option<u64>,
    #[serde(default)]
    pub heartbeat_counter: u64,
}

#[derive(Serialize, Deserialize)]
pub struct BreadcrumbReport {
    pub time_ns: u64,
    pub thread: u32,
    pub cat: String,
    pub sev: String,
    pub file: String,
    pub line: u32,
    pub msg: String,
}

#[derive(Serialize, Deserialize)]
pub struct SettingsSnapshotReport {
    #[serde(default)]
    pub source: ReportValueSource,
    pub world_bounds: [i32; 6],
    pub palette_count: i32,
    pub history_max: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extra: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct EnvironmentReport {
    pub kernel_release: String,
    pub kernel_version: String,
    pub arch: String,
    pub hostname: String,
    pub variables_source: String,
    #[serde(default)]
    pub env_vars: BTreeMap<String, String>,
}

#[derive(Serialize, Deserialize)]
pub struct ExceptionReport {
    #[serde(rename = "type")]
    pub exc_type: String,
    #[serde(default)]
    pub type_code: u32,
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_name: Option<String>,
    #[serde(default)]
    pub code_value: String,
    pub subcode: String,
    /// Raw MIG exception code array, preserving its original element count.
    #[serde(default)]
    pub raw_codes: Vec<String>,
    #[serde(default)]
    pub severity: crate::platform::ExceptionSeverity,
    pub signal: String,
    #[serde(default)]
    pub signal_is_approximate: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fault_address: Option<String>,
}

#[derive(Serialize, Deserialize)]
pub struct ThreadReport {
    pub index: u32,
    pub id: u64,
    #[serde(default)]
    pub name: Option<String>,
    pub crashed: bool,
    #[serde(default)]
    pub registers: BTreeMap<String, String>,
    #[serde(default)]
    pub backtrace: Vec<BacktraceFrame>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unwind_method: Option<String>,
    #[serde(default)]
    pub unwind_truncated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unwind_note: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub stack_memory: Option<StackMemoryReport>,
}

#[derive(Serialize, Deserialize)]
pub struct StackMemoryReport {
    pub sp: String,
    pub size: u64,
    pub hex_dump: String,
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Serialize, Deserialize)]
pub struct BacktraceFrame {
    pub address: String,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub offset: Option<String>,
    #[serde(default)]
    pub symbol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub column: Option<u32>,
}

#[derive(Serialize, Deserialize)]
pub struct LoadedImageReport {
    pub path: String,
    pub base: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub slide: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uuid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub architecture: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_start: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_end: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub segments: Vec<LoadedImageSegmentReport>,
}

#[derive(Serialize, Deserialize)]
pub struct LoadedImageSegmentReport {
    pub name: String,
    pub start: String,
    pub end: String,
}

#[derive(Serialize, Deserialize)]
pub struct VmRegionReport {
    pub address: String,
    pub size: String,
    pub prot: String,
    pub info: String,
}

#[derive(Serialize, Deserialize)]
pub struct HeapSummary {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_vm: Option<TaskVmSummaryReport>,
    pub zones: Vec<HeapZoneReport>,
}

#[derive(Serialize, Deserialize)]
pub struct TaskVmSummaryReport {
    pub virtual_size_bytes: u64,
    pub resident_size_bytes: u64,
    pub physical_footprint_bytes: u64,
    pub internal_bytes: u64,
    pub compressed_bytes: u64,
}

#[derive(Serialize, Deserialize)]
pub struct HeapZoneReport {
    pub name: String,
    #[serde(alias = "in_use_bytes")]
    pub resident_bytes_estimate: u64,
    #[serde(alias = "in_use_count")]
    pub region_count: u64,
    #[serde(default)]
    pub virtual_size_bytes: u64,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct SessionReport {
    pub id: String,
    pub start: String,
    pub duration_s: u64,
}

/// Origin of values copied from the producer-owned shared-memory region.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ReportValueSource {
    #[default]
    ProducerSharedMemory,
}

// ═══════════════════════════════════════════════════
//  Report loading (Phase 5 — CLI analysis commands)
// ═══════════════════════════════════════════════════

/// Maximum report file size to read (256 MB). Prevents OOM on garbage input.
const MAX_REPORT_SIZE: u64 = 256 * 1024 * 1024;
const MAX_ZIP_ARCHIVE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_ZIP_ENTRIES: usize = 256;
const MAX_ZIP_COMPRESSION_RATIO: u64 = 1_000;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReportLoadError {
    InvalidJson(String),
    UnsupportedVersion { found: u32, supported: u32 },
}

impl std::fmt::Display for ReportLoadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidJson(error) => write!(formatter, "invalid report JSON: {error}"),
            Self::UnsupportedVersion { found, supported } => write!(
                formatter,
                "unsupported report schema version {found}; this build supports version {supported}"
            ),
        }
    }
}

impl std::error::Error for ReportLoadError {}

/// Decode one report document and enforce its explicit schema version.
///
/// # Errors
/// Returns a structured invalid-JSON or unsupported-version error.
pub fn decode_report(bytes: &[u8]) -> Result<CrashReport, ReportLoadError> {
    #[derive(Deserialize)]
    struct VersionEnvelope {
        header: VersionHeader,
    }
    #[derive(Deserialize)]
    struct VersionHeader {
        version: u32,
    }

    let envelope: VersionEnvelope = serde_json::from_slice(bytes)
        .map_err(|error| ReportLoadError::InvalidJson(error.to_string()))?;
    if envelope.header.version != REPORT_SCHEMA_VERSION {
        return Err(ReportLoadError::UnsupportedVersion {
            found: envelope.header.version,
            supported: REPORT_SCHEMA_VERSION,
        });
    }
    serde_json::from_slice(bytes).map_err(|error| ReportLoadError::InvalidJson(error.to_string()))
}

/// Load and parse a crash report from a `.json` file or a `.zip` archive.
///
/// When `path` has a `.zip` extension (as produced by `ZIPArchiver`), the
/// report JSON entry is extracted in-memory — no temp files. The archive's
/// `<stem>.json` entry is preferred, falling back to the first `*.json` entry.
///
/// Returns a human-readable error on any failure (file not found,
/// permission denied, invalid JSON/ZIP, schema mismatch).
///
/// # Errors
/// Returns `Err(String)` if the file cannot be read, exceeds the size cap,
/// contains invalid/incompatible JSON, or is a ZIP with no report entry.
pub fn load_report(path: &Path) -> Result<CrashReport, String> {
    let bytes = if is_zip_path(path) {
        read_report_json_from_zip(path)?
    } else {
        read_plain_report(path)?
    };
    let mut report = decode_report(&bytes)
        .map_err(|error| format!("cannot load report '{}': {error}", path.display()))?;
    overlay_manifest_diagnostics(path, &mut report);
    Ok(report)
}

/// Overlay terminal diagnostics only when `path` is the canonical artifact of
/// a valid sibling manifest for the same report. External standalone reports
/// keep their embedded diagnostics and malformed optional manifests do not
/// make an otherwise readable report unusable.
fn overlay_manifest_diagnostics(path: &Path, report: &mut CrashReport) {
    let Some(report_dir) = path.parent() else {
        return;
    };
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return;
    };
    let Ok(manifest) = load_manifest(&report_dir.join(MANIFEST_FILE_NAME)) else {
        return;
    };
    if manifest.schema_version != MANIFEST_SCHEMA_VERSION
        || report_dir.file_name().and_then(|name| name.to_str())
            != Some(manifest.report_id.as_str())
        || report.header.report_id.as_ref() != Some(&manifest.report_id)
    {
        return;
    }
    let expected_kind = if is_zip_path(path) {
        ArtifactKind::Archive
    } else {
        ArtifactKind::Report
    };
    if !manifest
        .artifacts
        .iter()
        .any(|artifact| artifact.path == file_name && artifact.kind == expected_kind)
    {
        return;
    }
    if let Some(final_diagnostics) = manifest.final_diagnostics {
        report.diagnostics = Some(final_diagnostics);
    }
}

/// Persist the terminal wait status into an already finalized crash report.
///
/// Mach exception capture must reply before the child can reach a terminal
/// `waitpid` state, so that status is only known after the normal pipeline has
/// produced its JSON (or ZIP) artifact. This function atomically replaces the
/// artifact while preserving every non-report ZIP entry.
///
/// # Errors
/// Returns `Err(String)` if the existing report cannot be loaded, serialized,
/// rewritten, or atomically replaced.
pub fn update_termination(path: &Path, reason: TerminationReason) -> Result<(), String> {
    prepare_managed_report_parent(path)?;
    let original = if is_zip_path(path) {
        read_private_report_json_from_zip(path)?
    } else {
        read_private_plain_report(path)?
    };
    decode_report(&original)
        .map_err(|error| format!("cannot update report '{}': {error}", path.display()))?;
    let mut document: serde_json::Value = serde_json::from_slice(&original)
        .map_err(|e| format!("invalid report JSON in '{}': {e}", path.display()))?;
    let object = document
        .as_object_mut()
        .ok_or_else(|| format!("report JSON in '{}' is not an object", path.display()))?;
    object.insert(
        "termination".to_string(),
        serde_json::to_value(reason)
            .map_err(|e| format!("cannot serialize termination reason: {e}"))?,
    );
    let bytes = serde_json::to_vec_pretty(&document)
        .map_err(|e| format!("cannot serialize updated report '{}': {e}", path.display()))?;

    if is_zip_path(path) {
        rewrite_report_json_in_zip(path, &bytes)
    } else {
        atomic_replace(path, &bytes)
    }
}

pub(crate) fn atomic_replace(path: &Path, bytes: &[u8]) -> Result<(), String> {
    prepare_managed_report_parent(path)?;
    let existed = validate_private_existing_file(path)?;
    let tmp_path = termination_tmp_path(path)?;
    let mut file = create_private_file(&tmp_path).map_err(|e| {
        format!(
            "cannot create private temporary report '{}': {e}",
            tmp_path.display()
        )
    })?;
    let write_result = (|| -> Result<(), String> {
        file.write_all(bytes)
            .map_err(|e| format!("cannot write private temporary report: {e}"))?;
        file.flush()
            .map_err(|e| format!("cannot flush private temporary report: {e}"))?;
        file.sync_all()
            .map_err(|e| format!("cannot sync private temporary report: {e}"))?;
        Ok(())
    })();
    if let Err(e) = write_result {
        let _ = fs::remove_file(&tmp_path);
        return Err(format!(
            "cannot write temporary report '{}': {e}",
            tmp_path.display()
        ));
    }
    drop(file);
    let publish_result = if existed {
        fs::rename(&tmp_path, path).map_err(|error| error.to_string())
    } else {
        publish_private_path(&tmp_path, path)
    };
    if let Err(e) = publish_result {
        let _ = fs::remove_file(&tmp_path);
        return Err(format!("cannot replace report '{}': {e}", path.display()));
    }
    let parent = path
        .parent()
        .ok_or_else(|| format!("report path has no parent: '{}'", path.display()))?;
    open_private_directory(parent)?
        .sync_all()
        .map_err(|e| format!("cannot sync report directory '{}': {e}", parent.display()))?;
    Ok(())
}

fn rewrite_report_json_in_zip(path: &Path, report_json: &[u8]) -> Result<(), String> {
    let tmp_path = termination_tmp_path(path)?;
    let mut owns_tmp = false;
    let rewrite_result = (|| -> Result<(), String> {
        let source = open_private_existing_file(path)
            .map_err(|e| format!("cannot safely open ZIP '{}': {e}", path.display()))?;
        let mut archive = zip::ZipArchive::new(source)
            .map_err(|e| format!("invalid ZIP '{}': {e}", path.display()))?;
        let preferred = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .map(|stem| format!("{stem}.json"));
        let report_index = find_report_json_index(&mut archive, preferred.as_deref())
            .ok_or_else(|| format!("no report JSON (*.json) inside ZIP '{}'", path.display()))?;

        let destination = create_private_file(&tmp_path)
            .map_err(|e| format!("cannot create temporary ZIP '{}': {e}", tmp_path.display()))?;
        owns_tmp = true;
        let mut writer = zip::ZipWriter::new(destination);
        writer.set_raw_comment(archive.comment().to_vec().into_boxed_slice());

        for index in 0..archive.len() {
            let entry = archive.by_index(index).map_err(|e| {
                format!("cannot open ZIP entry {index} in '{}': {e}", path.display())
            })?;
            if index == report_index {
                let name = entry.name().to_string();
                let compression = entry.compression();
                let last_modified = entry.last_modified();
                drop(entry);

                let mut options = SimpleFileOptions::default()
                    .compression_method(compression)
                    .unix_permissions(0o600);
                if let Some(modified) = last_modified {
                    options = options.last_modified_time(modified);
                }
                writer
                    .start_file(name, options)
                    .map_err(|e| format!("cannot replace report entry in ZIP: {e}"))?;
                writer
                    .write_all(report_json)
                    .map_err(|e| format!("cannot write updated report entry in ZIP: {e}"))?;
            } else {
                writer
                    .raw_copy_file(entry)
                    .map_err(|e| format!("cannot preserve ZIP entry {index}: {e}"))?;
            }
        }

        let output = writer
            .finish()
            .map_err(|e| format!("cannot finalize updated ZIP '{}': {e}", path.display()))?;
        output
            .sync_all()
            .map_err(|e| format!("cannot sync updated ZIP '{}': {e}", path.display()))?;
        Ok(())
    })();

    if let Err(e) = rewrite_result {
        if owns_tmp {
            let _ = fs::remove_file(&tmp_path);
        }
        return Err(e);
    }
    if let Err(e) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(format!("cannot replace ZIP '{}': {e}", path.display()));
    }
    let parent = path
        .parent()
        .ok_or_else(|| format!("ZIP path has no parent: '{}'", path.display()))?;
    open_private_directory(parent)?
        .sync_all()
        .map_err(|e| format!("cannot sync ZIP directory '{}': {e}", parent.display()))?;
    Ok(())
}

fn open_private_existing_file(path: &Path) -> Result<fs::File, String> {
    open_private_file(path)
}

fn validate_private_existing_file(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(_) => open_private_file(path).map(|_| true).map_err(|error| {
            format!(
                "cannot validate existing report '{}': {error}",
                path.display()
            )
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(format!(
            "cannot inspect report '{}': {error}",
            path.display()
        )),
    }
}

fn prepare_managed_report_parent(path: &Path) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| format!("managed report path has no parent: '{}'", path.display()))?;
    open_private_directory(parent).map(|_| ()).map_err(|error| {
        format!(
            "cannot prepare private report directory '{}': {error}",
            parent.display()
        )
    })
}

fn termination_tmp_path(path: &Path) -> Result<PathBuf, String> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("report path has no valid filename: '{}'", path.display()))?;
    Ok(path.with_file_name(format!(
        ".{file_name}.termination-{}.tmp",
        uuid::Uuid::new_v4()
    )))
}

/// True if `path` has a `.zip` extension (case-insensitive).
fn is_zip_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("zip"))
}

/// Read a plain (uncompressed) report file, enforcing the size cap.
fn read_plain_report(path: &Path) -> Result<Vec<u8>, String> {
    let file = open_external_regular_file(path, MAX_REPORT_SIZE)?;
    read_plain_report_file(file, path)
}

/// Read a monitor-managed report through the private-storage policy. Unlike
/// [`read_plain_report`], this rejects unsafe ownership, modes, ACLs, and every
/// symlink component before the report can be rewritten in place.
fn read_private_plain_report(path: &Path) -> Result<Vec<u8>, String> {
    let file = open_private_existing_file(path).map_err(|error| {
        format!(
            "cannot safely open private report '{}': {error}",
            path.display()
        )
    })?;
    read_plain_report_file(file, path)
}

fn read_plain_report_file(mut file: fs::File, path: &Path) -> Result<Vec<u8>, String> {
    let metadata = file
        .metadata()
        .map_err(|error| format!("cannot inspect report '{}': {error}", path.display()))?;
    if metadata.len() > MAX_REPORT_SIZE {
        return Err(format!(
            "report file too large ({} bytes, max {} bytes)",
            metadata.len(),
            MAX_REPORT_SIZE
        ));
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    Read::by_ref(&mut file)
        .take(MAX_REPORT_SIZE + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read '{}': {error}", path.display()))?;
    if bytes.len() as u64 > MAX_REPORT_SIZE {
        return Err(format!(
            "report file grew beyond the maximum of {MAX_REPORT_SIZE} bytes"
        ));
    }
    Ok(bytes)
}

/// Extract the report JSON bytes from a `ZIPArchiver`-produced archive.
///
/// Prefers the entry named `<zip_stem>.json` (the report always shares the
/// archive's stem), else the first `*.json` entry. The decompressed entry
/// size is capped at `MAX_REPORT_SIZE` (zip-bomb guard).
fn read_report_json_from_zip(path: &Path) -> Result<Vec<u8>, String> {
    let file = open_external_regular_file(path, MAX_ZIP_ARCHIVE_BYTES)?;
    read_report_json_from_zip_file(path, file)
}

fn read_private_report_json_from_zip(path: &Path) -> Result<Vec<u8>, String> {
    let file = open_private_existing_file(path).map_err(|error| {
        format!(
            "cannot safely open private ZIP '{}': {error}",
            path.display()
        )
    })?;
    read_report_json_from_zip_file(path, file)
}

fn read_report_json_from_zip_file(path: &Path, file: fs::File) -> Result<Vec<u8>, String> {
    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| format!("invalid ZIP '{}': {e}", path.display()))?;

    if archive.len() > MAX_ZIP_ENTRIES {
        return Err(format!(
            "ZIP contains too many entries ({} > {MAX_ZIP_ENTRIES})",
            archive.len()
        ));
    }
    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .map_err(|error| format!("cannot inspect ZIP entry: {error}"))?;
        let name = entry.name();
        if name.contains('/') || name.contains('\\') {
            return Err(format!("nested ZIP entry is not allowed: {name:?}"));
        }
    }

    // `<stem>.zip` contains `<stem>.json`; prefer that over any attachment JSON.
    let preferred = path
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| format!("{s}.json"));

    let idx = find_report_json_index(&mut archive, preferred.as_deref())
        .ok_or_else(|| format!("no report JSON (*.json) inside ZIP '{}'", path.display()))?;

    let mut entry = archive
        .by_index(idx)
        .map_err(|e| format!("cannot open ZIP entry in '{}': {e}", path.display()))?;

    if entry.size() > MAX_REPORT_SIZE {
        return Err(format!(
            "report JSON in ZIP too large ({} bytes, max {} bytes)",
            entry.size(),
            MAX_REPORT_SIZE
        ));
    }
    let compressed_size = entry.compressed_size();

    let mut bytes = Vec::new();
    Read::by_ref(&mut entry)
        .take(MAX_REPORT_SIZE + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("cannot read ZIP entry in '{}': {e}", path.display()))?;
    if bytes.len() as u64 > MAX_REPORT_SIZE {
        return Err(format!(
            "report JSON decompressed beyond the maximum of {MAX_REPORT_SIZE} bytes"
        ));
    }
    let actual_size = bytes.len() as u64;
    if actual_size > 0
        && (compressed_size == 0
            || actual_size / compressed_size.max(1) > MAX_ZIP_COMPRESSION_RATIO)
    {
        return Err(format!(
            "report JSON compression ratio exceeds {MAX_ZIP_COMPRESSION_RATIO}:1"
        ));
    }
    Ok(bytes)
}

fn open_external_regular_file(path: &Path, max_bytes: u64) -> Result<fs::File, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("cannot inspect '{}': {error}", path.display()))?;
    if !metadata.file_type().is_file() || metadata.len() > max_bytes {
        return Err(format!(
            "input must be a regular file no larger than {max_bytes} bytes: '{}'",
            path.display()
        ));
    }
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)
        .map_err(|error| format!("cannot safely open '{}': {error}", path.display()))?;
    let opened = file
        .metadata()
        .map_err(|error| format!("cannot inspect opened '{}': {error}", path.display()))?;
    if !opened.file_type().is_file() || opened.len() > max_bytes {
        return Err(format!(
            "opened input violates file policy: '{}'",
            path.display()
        ));
    }
    Ok(file)
}

/// Find the report JSON entry index inside a ZIP archive.
///
/// Returns the index of `preferred` (matched by basename, case-insensitive)
/// if present, else the first entry whose basename ends in `.json`.
fn find_report_json_index<R: std::io::Read + std::io::Seek>(
    archive: &mut zip::ZipArchive<R>,
    preferred: Option<&str>,
) -> Option<usize> {
    let mut first_json: Option<usize> = None;
    for i in 0..archive.len() {
        let Ok(entry) = archive.by_index(i) else {
            continue;
        };
        let name = entry.name();
        let base = name.rsplit('/').next().unwrap_or(name);
        if let Some(pref) = preferred
            && base.eq_ignore_ascii_case(pref)
        {
            return Some(i);
        }
        if first_json.is_none() && base.to_ascii_lowercase().ends_with(".json") {
            first_json = Some(i);
        }
    }
    first_json
}

// ═══════════════════════════════════════════════════
//  Report filename helper
// ═══════════════════════════════════════════════════

#[must_use]
pub fn report_filename(report_type: ReportType, pid: u32) -> String {
    let now = Local::now();
    format!("{}_{}_{}", report_type, now.format("%Y%m%d_%H%M%S"), pid)
}

// ═══════════════════════════════════════════════════
//  Build + write report (Stage 2)
// ═══════════════════════════════════════════════════

/// Build a report from pipeline event + collected data + diagnostics.
#[must_use]
pub fn build_report(
    event: &CrashEvent,
    data: &CollectedData,
    diagnostics: &Diagnostics,
) -> CrashReport {
    let header = ReportHeader::new(event);

    let exception = if event.is_crash() {
        let exception_type = event.exception_type.unwrap_or(0);
        let policy = platform::exception_policy(exception_type);
        let decoded = platform::decode_exception(exception_type, &event.exception_codes);
        // The MIG code array is the single authoritative exception payload.
        // Legacy scalar projections may be absent or inconsistent and are not
        // allowed to contradict the preserved raw values in one report.
        let code_value = format!("{:#x}", event.exception_codes.first().copied().unwrap_or(0));
        let subcode = format!("{:#x}", event.exception_codes.get(1).copied().unwrap_or(0));
        Some(ExceptionReport {
            exc_type: platform::exception_type_name(exception_type).into(),
            type_code: exception_type,
            code: decoded.code_name.unwrap_or(&code_value).into(),
            code_name: decoded.code_name.map(str::to_string),
            code_value,
            subcode,
            raw_codes: event
                .exception_codes
                .iter()
                .map(|code| format!("{code:#x}"))
                .collect(),
            severity: policy
                .severity
                .unwrap_or(platform::ExceptionSeverity::Fatal),
            signal: decoded.signal.into(),
            signal_is_approximate: decoded.signal_is_approximate,
            fault_address: decoded.fault_address.map(|address| format!("{address:#x}")),
        })
    } else {
        None
    };

    let formatted = report_formatter::format(data, diagnostics);

    CrashReport {
        header,
        termination: event.termination,
        build: formatted.build,
        exception,
        crash_context: formatted.crash_context,
        threads: formatted.threads,
        breadcrumbs: (!formatted.breadcrumbs.is_empty()).then_some(formatted.breadcrumbs),
        loaded_images: formatted.loaded_images,
        memory_map: formatted.memory_map,
        heap_summary: formatted.heap_summary,
        session: formatted.session,
        settings_snapshot: formatted.settings_snapshot,
        fingerprint: data.fingerprint.clone(),
        environment: formatted.environment,
        process_output: formatted.process_output,
        attachments: formatted.attachments,
        user_feedback: None, // Patched post-write by FeedbackPostProcessor
        diagnostics: formatted.diagnostics_json,
    }
}

/// Write the full report JSON + raw RGBA screenshot dumps to disk.
///
/// Screenshots are persisted as `.rgba` files with attachment metadata tagged
/// `format = "rgba"`. The `PNGConverter` post-processor (if enabled) converts
/// them to PNG and rewrites the attachment entries.
///
/// # Errors
/// Returns an error if JSON serialization or file I/O fails.
pub fn write_report(
    transaction: &ArtifactTransaction,
    report: &mut CrashReport,
    screenshots: &[crate::shm::RawScreenshot],
) -> Result<PathBuf, String> {
    for (i, shot) in screenshots.iter().enumerate() {
        let rgba_name = format!("screenshot-{i:03}.rgba");
        if let Err(e) =
            transaction.write_bytes(&rgba_name, ArtifactKind::ScreenshotRgba, &shot.rgba)
        {
            eprintln!("[monitor] Failed to write screenshot RGBA: {e}");
            continue;
        }
        let mut attachment = crate::postprocessors::png_converter::rgba_attachment(
            &format!("screenshot_{i:03}"),
            &rgba_name,
            shot.width,
            shot.height,
            shot.rgba.len() as u64,
            shot.tier,
        );
        if let Some(metadata) = attachment.as_object_mut() {
            metadata.insert("timestamp_ns".into(), shot.timestamp_ns.into());
        }
        report.attachments.push(attachment);
    }

    // Defense in depth: post-formatting consumers and future fields must pass
    // through the same privacy policy immediately before serialization.
    let sanitized = crate::preprocessors::Sanitizer::new().sanitize_to_value(&*report)?;
    let json = serde_json::to_vec_pretty(&sanitized)
        .map_err(|e| format!("JSON serialization failed: {e}"))?;
    transaction
        .write_bytes("report.json", ArtifactKind::Report, &json)
        .map_err(|e| format!("Failed to write report: {e}"))
}

#[cfg(test)]
#[path = "../../tests/unit/pipeline/report_tests.rs"]
mod tests;
