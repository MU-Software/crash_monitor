//! Post-processor: convert raw RGBA screenshot dumps to PNG.
//!
//! `write_report` writes screenshots as `.rgba` files plus attachment entries
//! tagged `format = "rgba"`. This processor walks those entries, encodes them
//! to PNG, deletes the originals, and rewrites the report JSON. Conversion
//! failures keep the `.rgba` in place (partial-success preservation, principle #3).

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use crate::pipeline::report::CrashReport;
use crate::pipeline::{
    ArtifactKind, CrashEvent, Plugin, PluginContext, PluginExecution, PostProcessor, Priority,
    ReportResult,
};
use crate::utils::paths::{
    create_private_file, open_private_directory, open_private_file, publish_private_path,
};

pub struct PNGConverter;

const MAX_RGBA_DIMENSION: u32 = 8192;
const MAX_RGBA_BYTES: usize = 128 * 1024 * 1024;
const RGBA_READ_CHUNK_BYTES: usize = 64 * 1024;
const MAX_RGBA_FILENAME_BYTES: usize = 255;
const MAX_REPORT_JSON_BYTES: usize = 256 * 1024 * 1024;

#[derive(Clone, Copy)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

struct ConvertedRgba {
    path: PathBuf,
    png_path: PathBuf,
    identity: FileIdentity,
}

struct PendingTempFile {
    path: Option<PathBuf>,
}

impl PendingTempFile {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn published(&mut self) {
        self.path = None;
    }
}

impl Drop for PendingTempFile {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = fs::remove_file(path);
        }
    }
}

impl PNGConverter {
    fn process_impl(
        result: &mut ReportResult,
        context: &PluginContext,
        mut after_png_write: impl FnMut(),
        mut sync_published_directory: impl FnMut(&Path) -> Result<(), String>,
    ) -> Result<(), String> {
        context.checkpoint()?;
        let Some(json_path) = result.json_path.as_ref() else {
            return Ok(()); // No report → nothing to do
        };

        let dir = json_path
            .parent()
            .ok_or_else(|| "report has no parent dir".to_string())?
            .to_path_buf();
        // Report files are managed artifacts. Correct owned mode drift on the
        // containing directory before opening any report or attachment so the
        // no-follow private-file checks apply consistently to legacy reports.
        open_private_directory(&dir)?;
        let mut crash_report = load_report_for_conversion(json_path, context)?;

        let mut converted_rgba = Vec::new();
        for attachment in &mut crash_report.attachments {
            // Before the first filesystem change, cancellation can return
            // immediately. Once a PNG has been published, finish committing
            // the matching JSON before observing cancellation again.
            if converted_rgba.is_empty() {
                context.checkpoint()?;
            }
            if let Some(rgba_path) = convert_one(
                &dir,
                attachment,
                context,
                &mut after_png_write,
                &mut sync_published_directory,
            ) {
                if let Some(transaction) = context.artifact_transaction() {
                    transaction.register_file(&rgba_path.png_path, ArtifactKind::ScreenshotPng)?;
                }
                if !result.artifact_paths.contains(&rgba_path.png_path) {
                    result.artifact_paths.push(rgba_path.png_path.clone());
                }
                converted_rgba.push(rgba_path);
            }
        }

        if converted_rgba.is_empty() {
            return context.checkpoint();
        }

        let json = serde_json::to_vec_pretty(&crash_report)
            .map_err(|e| format!("JSON serialization failed: {e}"))?;
        replace_report_atomically(json_path, &json)?;

        // The JSON now points at the successfully published PNGs. Deleting
        // the old RGBA files afterwards makes every interruption safe: a
        // failure before this point leaves the original report usable, while
        // a failure here can only leave redundant RGBA files behind.
        for converted in converted_rgba {
            match remove_if_unchanged(&converted.path, converted.identity) {
                Ok(true) => {
                    if let Some(transaction) = context.artifact_transaction() {
                        transaction.unregister_file(&converted.path)?;
                    }
                    result.artifact_paths.retain(|path| path != &converted.path);
                }
                Ok(false) => {}
                Err(error) => eprintln!(
                    "[monitor] PNGConverter: cannot remove converted RGBA '{}': {error}",
                    converted.path.display()
                ),
            }
        }

        context.checkpoint()
    }

    #[cfg(test)]
    fn process_with_after_png_write(
        result: &mut ReportResult,
        context: &PluginContext,
        after_png_write: impl FnMut(),
    ) -> Result<(), String> {
        Self::process_impl(result, context, after_png_write, sync_private_directory)
    }

    #[cfg(test)]
    fn process_with_directory_sync(
        result: &mut ReportResult,
        context: &PluginContext,
        sync_published_directory: impl FnMut(&Path) -> Result<(), String>,
    ) -> Result<(), String> {
        Self::process_impl(result, context, || {}, sync_published_directory)
    }
}

impl Plugin for PNGConverter {
    fn name(&self) -> &'static str {
        "PNGConverter"
    }
    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }
    fn priority(&self) -> Priority {
        Priority::Medium
    }
}

impl PostProcessor for PNGConverter {
    fn process(
        &self,
        _event: &CrashEvent,
        result: &mut ReportResult,
        context: &PluginContext,
    ) -> Result<(), String> {
        Self::process_impl(result, context, || {}, sync_private_directory)
    }
}

/// Convert one `.rgba` attachment to `.png` in place. Returns the old RGBA
/// path when the attachment JSON was modified (caller persists the change
/// before deleting that path).
fn convert_one(
    dir: &Path,
    attachment: &mut serde_json::Value,
    context: &PluginContext,
    after_png_write: &mut impl FnMut(),
    sync_published_directory: &mut impl FnMut(&Path) -> Result<(), String>,
) -> Option<ConvertedRgba> {
    if context.is_timed_out() {
        return None;
    }
    if attachment.get("format").and_then(|v| v.as_str()) != Some("rgba") {
        return None;
    }
    let file_name = attachment.get("file").and_then(|v| v.as_str())?;
    if !is_safe_rgba_basename(file_name) {
        eprintln!("[monitor] PNGConverter: unsafe RGBA attachment path ignored: {file_name:?}");
        return None;
    }
    let width = attachment
        .get("width")
        .and_then(serde_json::Value::as_u64)?;
    let height = attachment
        .get("height")
        .and_then(serde_json::Value::as_u64)?;
    let width = u32::try_from(width).ok()?;
    let height = u32::try_from(height).ok()?;
    let expected_len = match checked_rgba_len(width, height) {
        Ok(expected_len) => expected_len,
        Err(error) => {
            eprintln!("[monitor] PNGConverter: invalid RGBA dimensions for {file_name}: {error}");
            return None;
        }
    };

    let rgba_path = dir.join(file_name);
    let (rgba_bytes, identity) = match read_rgba_exact(&rgba_path, expected_len, context) {
        Ok(read) => read,
        Err(error) => {
            eprintln!("[monitor] PNGConverter: cannot read {file_name}: {error}");
            return None;
        }
    };
    if context.is_timed_out() {
        return None;
    }

    let png_bytes = match encode_png(&rgba_bytes, width, height) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[monitor] PNGConverter: encode failed for {file_name}: {e}");
            return None;
        }
    };
    if context.is_timed_out() {
        return None;
    }

    let png_name = file_name
        .strip_suffix(".rgba")
        .map(|stem| format!("{stem}.png"))?;
    let png_path = dir.join(&png_name);
    if let Err(e) = publish_png(&png_path, &png_bytes, context, sync_published_directory) {
        eprintln!("[monitor] PNGConverter: write PNG failed for {png_name}: {e}");
        return None;
    }
    after_png_write();

    // No cancellation checkpoint is allowed between publishing the PNG and
    // committing this attachment through the caller's atomic JSON rewrite.
    let obj = attachment.as_object_mut()?;
    obj.insert("file".into(), serde_json::Value::String(png_name));
    obj.insert("format".into(), serde_json::Value::String("png".into()));
    obj.insert(
        "size".into(),
        serde_json::Value::Number((png_bytes.len() as u64).into()),
    );
    obj.remove("width");
    obj.remove("height");
    Some(ConvertedRgba {
        path: rgba_path,
        png_path,
        identity,
    })
}

fn is_safe_rgba_basename(file_name: &str) -> bool {
    if file_name.is_empty()
        || file_name.len() > MAX_RGBA_FILENAME_BYTES
        || file_name.as_bytes().contains(&0)
    {
        return false;
    }
    let path = Path::new(file_name);
    let mut components = path.components();
    matches!(components.next(), Some(Component::Normal(_)))
        && components.next().is_none()
        && path
            .extension()
            .is_some_and(|extension| extension == "rgba")
}

fn checked_rgba_len(width: u32, height: u32) -> Result<usize, String> {
    if width == 0 || height == 0 {
        return Err("dimensions must be non-zero".to_string());
    }
    if width > MAX_RGBA_DIMENSION || height > MAX_RGBA_DIMENSION {
        return Err(format!(
            "dimensions exceed {MAX_RGBA_DIMENSION} pixels per axis"
        ));
    }

    let width = usize::try_from(width).map_err(|_| "width does not fit usize".to_string())?;
    let height = usize::try_from(height).map_err(|_| "height does not fit usize".to_string())?;
    let bytes = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| "width * height * 4 overflowed".to_string())?;
    if bytes > MAX_RGBA_BYTES {
        return Err(format!("RGBA input exceeds {MAX_RGBA_BYTES} bytes"));
    }
    Ok(bytes)
}

fn read_rgba_exact(
    path: &Path,
    expected_len: usize,
    context: &PluginContext,
) -> Result<(Vec<u8>, FileIdentity), String> {
    context.checkpoint()?;
    let mut file =
        open_private_file(path).map_err(|error| format!("RGBA input is not private: {error}"))?;
    context.checkpoint()?;

    let metadata = file.metadata().map_err(|error| error.to_string())?;
    if !metadata.file_type().is_file() {
        return Err("RGBA input is not a regular file".to_string());
    }
    let expected_u64 = u64::try_from(expected_len)
        .map_err(|_| "expected RGBA size does not fit u64".to_string())?;
    if metadata.len() != expected_u64 {
        return Err(format!(
            "RGBA size mismatch: expected {expected_len} bytes, found {}",
            metadata.len()
        ));
    }

    // Probe up to expected + 1 so a file that grows after fstat cannot turn
    // this cooperative read into unbounded allocation or work.
    let probe_limit = expected_len
        .checked_add(1)
        .ok_or_else(|| "RGBA probe limit overflowed".to_string())?;
    let mut bytes = Vec::with_capacity(expected_len);
    let mut chunk = vec![0_u8; RGBA_READ_CHUNK_BYTES];
    loop {
        context.checkpoint()?;
        let remaining = probe_limit.saturating_sub(bytes.len());
        if remaining == 0 {
            return Err(format!(
                "RGBA size mismatch: expected {expected_len} bytes, input grew while reading"
            ));
        }
        let slice_len = remaining.min(chunk.len());
        match file.read(&mut chunk[..slice_len]) {
            Ok(0) => break,
            Ok(read) => {
                bytes.extend_from_slice(&chunk[..read]);
                if bytes.len() > expected_len {
                    return Err(format!(
                        "RGBA size mismatch: expected {expected_len} bytes, input grew while reading"
                    ));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    if bytes.len() != expected_len {
        return Err(format!(
            "RGBA size mismatch: expected {expected_len} bytes, read {}",
            bytes.len()
        ));
    }
    Ok((
        bytes,
        FileIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        },
    ))
}

fn replace_report_atomically(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut sync = sync_private_directory;
    publish_bytes_atomically(path, bytes, None, "report", &mut sync)
}

fn publish_png(
    path: &Path,
    bytes: &[u8],
    context: &PluginContext,
    sync_published_directory: &mut impl FnMut(&Path) -> Result<(), String>,
) -> Result<(), String> {
    publish_bytes_atomically(path, bytes, Some(context), "PNG", sync_published_directory)
}

fn publish_bytes_atomically(
    path: &Path,
    bytes: &[u8],
    context: Option<&PluginContext>,
    kind: &str,
    sync_published_directory: &mut impl FnMut(&Path) -> Result<(), String>,
) -> Result<(), String> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| format!("{kind} path has no valid filename: '{}'", path.display()))?;
    let tmp_path = path.with_file_name(format!(
        ".{file_name}.png-converter-{}.tmp",
        uuid::Uuid::new_v4()
    ));
    let mut tmp = create_private_file(&tmp_path)
        .map_err(|error| format!("Failed to create {kind} temporary file: {error}"))?;
    let mut pending = PendingTempFile::new(tmp_path.clone());

    for chunk in bytes.chunks(RGBA_READ_CHUNK_BYTES) {
        if let Some(context) = context {
            context.checkpoint()?;
        }
        tmp.write_all(chunk)
            .map_err(|error| format!("Failed to write {kind} temporary file: {error}"))?;
    }
    tmp.flush()
        .map_err(|error| format!("Failed to flush {kind} temporary file: {error}"))?;
    tmp.sync_all()
        .map_err(|error| format!("Failed to sync {kind} temporary file: {error}"))?;
    drop(tmp);
    if let Some(context) = context {
        context.checkpoint()?;
    }

    if context.is_some() {
        publish_private_path(&tmp_path, path)
            .map_err(|error| format!("Failed to publish {kind}: {error}"))?;
    } else {
        // The report already exists and intentionally changes in place after
        // its opened descriptor was validated above.
        fs::rename(&tmp_path, path)
            .map_err(|error| format!("Failed to replace {kind}: {error}"))?;
    }
    pending.published();
    let parent = path
        .parent()
        .ok_or_else(|| format!("{kind} path has no parent: '{}'", path.display()))?;
    if let Err(error) = sync_published_directory(parent) {
        let warning = format!(
            "{kind} '{}' was published but its directory could not be synced: {error}",
            path.display()
        );
        if let Some(transaction) = context.and_then(PluginContext::artifact_transaction) {
            transaction.record_durability_warning(warning);
        } else {
            eprintln!("[monitor] PNGConverter: {warning}");
        }
    }
    Ok(())
}

fn sync_private_directory(path: &Path) -> Result<(), String> {
    open_private_directory(path)?.sync_all().map_err(|error| {
        format!(
            "failed to sync private directory '{}': {error}",
            path.display()
        )
    })
}

fn load_report_for_conversion(path: &Path, context: &PluginContext) -> Result<CrashReport, String> {
    context.checkpoint()?;
    let mut file = open_private_file(path)
        .map_err(|error| format!("report '{}' is not private: {error}", path.display()))?;
    let metadata = file
        .metadata()
        .map_err(|error| format!("cannot inspect report '{}': {error}", path.display()))?;
    if !metadata.file_type().is_file() {
        return Err(format!("report '{}' is not a regular file", path.display()));
    }
    if metadata.len() > MAX_REPORT_JSON_BYTES as u64 {
        return Err(format!(
            "report '{}' exceeds {MAX_REPORT_JSON_BYTES} bytes",
            path.display()
        ));
    }

    let mut bytes = Vec::with_capacity(
        usize::try_from(metadata.len())
            .unwrap_or(MAX_REPORT_JSON_BYTES)
            .min(MAX_REPORT_JSON_BYTES),
    );
    let mut buffer = vec![0_u8; RGBA_READ_CHUNK_BYTES];
    loop {
        context.checkpoint()?;
        let remaining = (MAX_REPORT_JSON_BYTES + 1).saturating_sub(bytes.len());
        if remaining == 0 {
            return Err(format!(
                "report '{}' exceeds {MAX_REPORT_JSON_BYTES} bytes",
                path.display()
            ));
        }
        let read_len = remaining.min(buffer.len());
        match file.read(&mut buffer[..read_len]) {
            Ok(0) => break,
            Ok(read) => {
                bytes.extend_from_slice(&buffer[..read]);
                if bytes.len() > MAX_REPORT_JSON_BYTES {
                    return Err(format!(
                        "report '{}' exceeds {MAX_REPORT_JSON_BYTES} bytes",
                        path.display()
                    ));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => {
                return Err(format!("cannot read report '{}': {error}", path.display()));
            }
        }
    }
    serde_json::from_slice(&bytes)
        .map_err(|error| format!("invalid report JSON in '{}': {error}", path.display()))
}

fn remove_if_unchanged(path: &Path, expected: FileIdentity) -> Result<bool, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error.to_string()),
    };
    if metadata.file_type().is_file()
        && metadata.dev() == expected.device
        && metadata.ino() == expected.inode
    {
        match fs::remove_file(path) {
            Ok(()) => return Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(false)
}

/// Encode RGBA pixel data to PNG. Returns the PNG bytes.
///
/// # Errors
/// Returns an error when dimensions or input length violate the explicit RGBA
/// bounds, or when the `png` crate reports an I/O error.
#[allow(clippy::doc_markdown)]
pub fn encode_png(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
    let expected_len = checked_rgba_len(width, height)?;
    if rgba.len() != expected_len {
        return Err(format!(
            "RGBA size mismatch: expected {expected_len} bytes, found {}",
            rgba.len()
        ));
    }
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
