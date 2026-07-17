//! Post-processor: convert raw RGBA screenshot dumps to PNG.
//!
//! `write_report` writes screenshots as `.rgba` files plus attachment entries
//! tagged `format = "rgba"`. This processor walks those entries, encodes them
//! to PNG, deletes the originals, and rewrites the report JSON. Conversion
//! failures keep the `.rgba` in place (partial-success preservation, principle #3).

use std::fs;

use crate::pipeline::report::{self};
use crate::pipeline::{CrashEvent, Plugin, PostProcessor, Priority, ReportResult};

pub struct PNGConverter;

impl Plugin for PNGConverter {
    fn name(&self) -> &'static str {
        "PNGConverter"
    }
    fn priority(&self) -> Priority {
        Priority::Medium
    }
}

impl PostProcessor for PNGConverter {
    fn process(&self, _event: &CrashEvent, result: &mut ReportResult) -> Result<(), String> {
        let Some(ref json_path) = result.json_path else {
            return Ok(()); // No report → nothing to do
        };

        let mut crash_report = report::load_report(json_path)?;
        let dir = json_path
            .parent()
            .ok_or_else(|| "report has no parent dir".to_string())?
            .to_path_buf();

        let mut any_changed = false;
        for attachment in &mut crash_report.attachments {
            if convert_one(&dir, attachment) {
                any_changed = true;
            }
        }

        if any_changed {
            let json = serde_json::to_string_pretty(&crash_report)
                .map_err(|e| format!("JSON serialization failed: {e}"))?;
            fs::write(json_path, json).map_err(|e| format!("Failed to write report: {e}"))?;
        }
        Ok(())
    }
}

/// Convert one `.rgba` attachment to `.png` in place. Returns true if the
/// attachment JSON was modified (caller persists the change).
fn convert_one(dir: &std::path::Path, attachment: &mut serde_json::Value) -> bool {
    if attachment.get("format").and_then(|v| v.as_str()) != Some("rgba") {
        return false;
    }
    let Some(file_name) = attachment.get("file").and_then(|v| v.as_str()) else {
        return false;
    };
    let Some(width) = attachment.get("width").and_then(serde_json::Value::as_u64) else {
        return false;
    };
    let Some(height) = attachment.get("height").and_then(serde_json::Value::as_u64) else {
        return false;
    };
    let Ok(width) = u32::try_from(width) else {
        return false;
    };
    let Ok(height) = u32::try_from(height) else {
        return false;
    };

    let rgba_path = dir.join(file_name);
    let rgba_bytes = match fs::read(&rgba_path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[monitor] PNGConverter: cannot read {file_name}: {e}");
            return false;
        }
    };

    let png_bytes = match encode_png(&rgba_bytes, width, height) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[monitor] PNGConverter: encode failed for {file_name}: {e}");
            return false;
        }
    };

    let png_name = file_name.trim_end_matches(".rgba").to_string() + ".png";
    let png_path = dir.join(&png_name);
    if let Err(e) = fs::write(&png_path, &png_bytes) {
        eprintln!("[monitor] PNGConverter: write PNG failed for {png_name}: {e}");
        return false;
    }

    // PNG written successfully → remove original RGBA + patch the attachment.
    let _ = fs::remove_file(&rgba_path);
    let Some(obj) = attachment.as_object_mut() else {
        return false;
    };
    obj.insert("file".into(), serde_json::Value::String(png_name));
    obj.insert("format".into(), serde_json::Value::String("png".into()));
    obj.insert(
        "size".into(),
        serde_json::Value::Number((png_bytes.len() as u64).into()),
    );
    obj.remove("width");
    obj.remove("height");
    true
}

/// Encode RGBA pixel data to PNG. Returns the PNG bytes.
///
/// # Errors
/// Returns an error when zero-dimension input or `png` crate I/O errors.
#[allow(clippy::doc_markdown)]
pub fn encode_png(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
    let mut buf: Vec<u8> = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut buf, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| format!("PNG header: {e}"))?;
        writer
            .write_image_data(rgba)
            .map_err(|e| format!("PNG data: {e}"))?;
    }
    Ok(buf)
}

/// Build the attachment JSON entry for a raw RGBA screenshot written to disk.
/// Used by `write_report` so the `PNGConverter` post-processor can later locate
/// and convert the file.
#[must_use]
pub fn rgba_attachment(
    label: &str,
    file_name: &str,
    width: u32,
    height: u32,
    size: u64,
) -> serde_json::Value {
    serde_json::json!({
        "label": label,
        "file": file_name,
        "format": "rgba",
        "width": width,
        "height": height,
        "size": size,
    })
}

#[cfg(test)]
#[path = "../../tests/unit/postprocessors/png_converter_tests.rs"]
mod tests;
