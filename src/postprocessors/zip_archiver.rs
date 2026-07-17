//! Post-processor: bundle report files into a single ZIP archive.
//!
//! Collects all files sharing the same basename prefix as the report JSON
//! (e.g., screenshots), compresses them into a `.zip`, then removes originals.

use crate::pipeline::{CrashEvent, Plugin, PostProcessor, Priority, ReportResult};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use zip::write::SimpleFileOptions;

pub struct ZIPArchiver;

impl Plugin for ZIPArchiver {
    fn name(&self) -> &'static str {
        "ZIPArchiver"
    }
    fn priority(&self) -> Priority {
        Priority::Low
    }
    fn depends_on(&self) -> &'static [&'static str] {
        // Must run after FeedbackPostProcessor patches user_feedback into the JSON.
        // Soft-validated: FeedbackPostProcessor may not be registered (no dialog binary).
        &["FeedbackDialog"]
    }
}

impl PostProcessor for ZIPArchiver {
    fn process(&self, _event: &CrashEvent, result: &mut ReportResult) -> Result<(), String> {
        let Some(json_path) = result.json_path.as_ref() else {
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
        let files = collect_report_files(dir, stem)?;
        if files.is_empty() {
            return Ok(());
        }

        let zip_path = dir.join(format!("{stem}.zip"));
        let tmp_path = dir.join(format!("{stem}.zip.tmp"));

        // Write ZIP to temporary file
        if let Err(e) = write_zip(&tmp_path, &files) {
            // Clean up partial ZIP
            let _ = fs::remove_file(&tmp_path);
            return Err(format!("ZIP creation failed: {e}"));
        }

        // Atomic rename
        if let Err(e) = fs::rename(&tmp_path, &zip_path) {
            // Fallback: try direct copy
            let _ = fs::remove_file(&tmp_path);
            return Err(format!("ZIP rename failed: {e}"));
        }

        // Remove original files (best-effort)
        for file in &files {
            let _ = fs::remove_file(file);
        }

        // ZIP is now the canonical report artifact. Keeping this path exact is
        // required by later post-processors, notifiers, and crash-status
        // finalization after the child has been reaped.
        result.json_path = Some(zip_path);

        Ok(())
    }
}

fn collect_report_files(dir: &std::path::Path, stem: &str) -> Result<Vec<PathBuf>, String> {
    let entries =
        fs::read_dir(dir).map_err(|e| format!("cannot read directory '{}': {e}", dir.display()))?;

    let mut files = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if name_str.starts_with(stem) && !name_str.ends_with(".zip") && !name_str.ends_with(".tmp")
        {
            let path = entry.path();
            if path.is_file() {
                files.push(path);
            }
        }
    }
    Ok(files)
}

fn write_zip(zip_path: &std::path::Path, files: &[PathBuf]) -> Result<(), String> {
    let file = fs::File::create(zip_path).map_err(|e| format!("cannot create ZIP file: {e}"))?;
    let mut writer = zip::ZipWriter::new(file);
    let options = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    for path in files {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| "invalid filename".to_string())?;
        let data = fs::read(path).map_err(|e| format!("cannot read '{}': {e}", path.display()))?;

        writer
            .start_file(name, options)
            .map_err(|e| format!("ZIP start_file failed: {e}"))?;
        writer
            .write_all(&data)
            .map_err(|e| format!("ZIP write failed: {e}"))?;
    }

    writer
        .finish()
        .map_err(|e| format!("ZIP finalize failed: {e}"))?;
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/unit/postprocessors/zip_archiver_tests.rs"]
mod tests;
