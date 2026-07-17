//! Crash/snapshot report generation (JSON serialization only).
//! No raw data type imports — receives pre-formatted data from `report_formatter`.

use crate::platform;
use crate::preprocessors::report_formatter;

use chrono::Local;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use super::types::{CollectedData, CrashEvent, Diagnostics, ReportType};

// ═══════════════════════════════════════════════════
//  Serde report structures (design doc lines 1026-1153)
// ═══════════════════════════════════════════════════

#[derive(Serialize, Deserialize)]
pub struct CrashReport {
    pub header: ReportHeader,
    #[serde(default)]
    pub build: Option<serde_json::Value>, // Phase 4
    #[serde(default)]
    pub exception: Option<ExceptionReport>,
    #[serde(default)]
    pub crash_context: Option<serde_json::Value>, // Phase 4
    #[serde(default)]
    pub threads: Vec<ThreadReport>,
    #[serde(default)]
    pub breadcrumbs: Option<serde_json::Value>, // Phase 4
    #[serde(default)]
    pub loaded_images: Vec<LoadedImageReport>,
    #[serde(default)]
    pub memory_map: Vec<VmRegionReport>,
    #[serde(default)]
    pub heap_summary: Option<HeapSummary>,
    #[serde(default)]
    pub session: Option<SessionReport>,
    #[serde(default)]
    pub settings_snapshot: Option<serde_json::Value>,
    #[serde(default)]
    pub fingerprint: Option<String>,
    #[serde(default)]
    pub environment: Option<serde_json::Value>,
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
    pub timestamp: String,
    pub pid: u32,
    pub process: String,
    pub collector: String,
    #[serde(rename = "type")]
    pub report_type: ReportType,
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
            ReportType::Crash | ReportType::Snapshot => (None, None),
        };

        Self {
            version: 1,
            timestamp: Local::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, false),
            pid: event.pid,
            process: event.process_name.clone(),
            collector: "mbb_monitor".into(),
            report_type: event.report_type,
            trigger,
            hang_duration_ms,
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct ExceptionReport {
    #[serde(rename = "type")]
    pub exc_type: String,
    pub code: String,
    pub subcode: String,
    pub signal: String,
    pub fault_address: String,
}

#[derive(Serialize, Deserialize)]
pub struct ThreadReport {
    pub index: u32,
    pub id: u32,
    #[serde(default)]
    pub name: Option<String>,
    pub crashed: bool,
    #[serde(default)]
    pub registers: BTreeMap<String, String>,
    #[serde(default)]
    pub backtrace: Vec<BacktraceFrame>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default)]
    pub stack_memory: Option<StackMemoryReport>,
}

#[derive(Serialize, Deserialize)]
pub struct StackMemoryReport {
    pub sp: String,
    pub size: u64,
    pub hex_dump: String,
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
    pub zones: Vec<HeapZoneReport>,
}

#[derive(Serialize, Deserialize)]
pub struct HeapZoneReport {
    pub name: String,
    pub in_use_bytes: u64,
    pub in_use_count: u64,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct SessionReport {
    pub id: String,
    pub start: String,
    pub duration_s: u64,
}

// ═══════════════════════════════════════════════════
//  Report loading (Phase 5 — CLI analysis commands)
// ═══════════════════════════════════════════════════

/// Maximum report file size to read (256 MB). Prevents OOM on garbage input.
const MAX_REPORT_SIZE: u64 = 256 * 1024 * 1024;

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
    serde_json::from_slice(&bytes)
        .map_err(|e| format!("invalid report JSON in '{}': {e}", path.display()))
}

/// True if `path` has a `.zip` extension (case-insensitive).
fn is_zip_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("zip"))
}

/// Read a plain (uncompressed) report file, enforcing the size cap.
fn read_plain_report(path: &Path) -> Result<Vec<u8>, String> {
    let metadata =
        fs::metadata(path).map_err(|e| format!("cannot read '{}': {e}", path.display()))?;
    if metadata.len() > MAX_REPORT_SIZE {
        return Err(format!(
            "report file too large ({} bytes, max {} bytes)",
            metadata.len(),
            MAX_REPORT_SIZE
        ));
    }
    fs::read(path).map_err(|e| format!("cannot read '{}': {e}", path.display()))
}

/// Extract the report JSON bytes from a `ZIPArchiver`-produced archive.
///
/// Prefers the entry named `<zip_stem>.json` (the report always shares the
/// archive's stem), else the first `*.json` entry. The decompressed entry
/// size is capped at `MAX_REPORT_SIZE` (zip-bomb guard).
fn read_report_json_from_zip(path: &Path) -> Result<Vec<u8>, String> {
    let file =
        fs::File::open(path).map_err(|e| format!("cannot open '{}': {e}", path.display()))?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| format!("invalid ZIP '{}': {e}", path.display()))?;

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

    let mut bytes = Vec::new();
    std::io::Read::read_to_end(&mut entry, &mut bytes)
        .map_err(|e| format!("cannot read ZIP entry in '{}': {e}", path.display()))?;
    Ok(bytes)
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
        Some(ExceptionReport {
            exc_type: platform::exception_type_name(event.exception_type.unwrap_or(0)).into(),
            code: platform::kern_return_name(event.exception_code.unwrap_or(0)).into(),
            subcode: format!("{:#x}", event.exception_subcode.unwrap_or(0)),
            signal: platform::exception_to_signal(event.exception_type.unwrap_or(0)).into(),
            fault_address: format!("{:#x}", event.exception_subcode.unwrap_or(0)),
        })
    } else {
        None
    };

    let formatted = report_formatter::format(data, diagnostics);

    CrashReport {
        header,
        build: formatted.build,
        exception,
        crash_context: formatted.crash_context,
        threads: formatted.threads,
        breadcrumbs: if formatted.breadcrumbs.is_empty() {
            None
        } else {
            Some(serde_json::Value::Array(formatted.breadcrumbs))
        },
        loaded_images: formatted.loaded_images,
        memory_map: formatted.memory_map,
        heap_summary: formatted.heap_summary,
        session: formatted.session,
        settings_snapshot: formatted.settings_snapshot,
        fingerprint: data.fingerprint.clone(),
        environment: formatted.environment,
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
    dir: &Path,
    report: &mut CrashReport,
    screenshots: &[crate::shm::RawScreenshot],
) -> Result<PathBuf, String> {
    let basename = report_filename(report.header.report_type, report.header.pid);

    for (i, shot) in screenshots.iter().enumerate() {
        let rgba_name = format!("{basename}_screenshot_{i:03}.rgba");
        let rgba_path = dir.join(&rgba_name);
        if let Err(e) = fs::write(&rgba_path, &shot.rgba) {
            eprintln!("[monitor] Failed to write screenshot RGBA: {e}");
            continue;
        }
        report
            .attachments
            .push(crate::postprocessors::png_converter::rgba_attachment(
                &format!("screenshot_{i:03}"),
                &rgba_name,
                shot.width,
                shot.height,
                shot.rgba.len() as u64,
            ));
    }

    let json_path = dir.join(format!("{basename}.json"));
    let json = serde_json::to_string_pretty(report)
        .map_err(|e| format!("JSON serialization failed: {e}"))?;
    fs::write(&json_path, json).map_err(|e| format!("Failed to write report: {e}"))?;

    Ok(json_path)
}

#[cfg(test)]
#[path = "../../tests/unit/pipeline/report_tests.rs"]
mod tests;
