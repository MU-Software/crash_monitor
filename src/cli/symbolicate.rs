//! `crash_monitor symbolicate <report.json> --dsym <path>` — resolve addresses to `source:line`.
//!
//! Uses `addr2line::Loader` to parse DWARF debug info from a dSYM bundle
//! and resolve backtrace addresses to source file, function, and line number.

use crate::pipeline::report::{self, CrashReport, LoadedImageReport};
use crate::utils::paths::{PRIVATE_FILE_MODE, open_trusted_directory, validate_private_file};
use nix::fcntl::{OFlag, openat, renameat};
use nix::sys::stat::Mode;
use std::fs::{self, File};
use std::io::Write;
#[cfg(target_os = "macos")]
use std::os::fd::AsRawFd;
#[cfg(target_os = "macos")]
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Maximum dSYM file size (1 GB).
const MAX_DSYM_SIZE: u64 = 1024 * 1024 * 1024;

/// Run the `symbolicate` subcommand. Returns exit code (0 = success, 1 = error).
#[must_use]
pub fn run(report_path: &str, dsym_path: &str, output: Option<&str>) -> i32 {
    let mut report = match report::load_report(Path::new(report_path)) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };

    let dwarf_path = match find_dwarf_binary(Path::new(dsym_path)) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };

    // Validate file size before loading
    match std::fs::metadata(&dwarf_path) {
        Ok(m) if m.len() > MAX_DSYM_SIZE => {
            eprintln!(
                "error: DWARF binary too large ({} bytes, max {} bytes)",
                m.len(),
                MAX_DSYM_SIZE
            );
            return 1;
        }
        Err(e) => {
            eprintln!("error: cannot stat '{}': {e}", dwarf_path.display());
            return 1;
        }
        _ => {}
    }

    let loader = match addr2line::Loader::new(&dwarf_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!(
                "error: failed to load DWARF from '{}': {e}",
                dwarf_path.display()
            );
            return 1;
        }
    };

    // Build a slide lookup from loaded_images
    let slides = build_slide_map(&report.loaded_images);

    // Symbolicate all frames in all threads
    let mut resolved_count = 0u32;
    for thread in &mut report.threads {
        for frame in &mut thread.backtrace {
            let Some(addr) = parse_hex_address(&frame.address) else {
                continue;
            };

            // Find the image this address belongs to and get its slide
            let slide = find_slide_for_address(&slides, addr);
            let file_addr = addr.wrapping_sub(slide);

            // Try to get source location
            if let Ok(Some(loc)) = loader.find_location(file_addr) {
                if let Some(file) = loc.file {
                    frame.file = Some(file.to_string());
                }
                if let Some(line) = loc.line {
                    frame.line = Some(line);
                }
                if loc.column.is_some() {
                    frame.column = loc.column;
                }
                resolved_count += 1;
            }

            // Also try to get function name if not already set
            if frame.symbol.is_none()
                && let Some(name) = loader.find_symbol(file_addr)
            {
                frame.symbol = Some(name.to_string());
            }
        }
    }

    eprintln!("[symbolicate] resolved {resolved_count} source locations");

    if let Some(out_path) = output {
        match write_report(&report, Path::new(out_path)) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("error: {e}");
                1
            }
        }
    } else {
        print_symbolicated_backtrace(&report);
        0
    }
}

/// Locate the DWARF binary inside a dSYM bundle.
///
/// Accepts either:
/// - A `.dSYM` directory: searches `Contents/Resources/DWARF/` for the first file
/// - A direct path to the DWARF binary itself
fn find_dwarf_binary(dsym_path: &Path) -> Result<PathBuf, String> {
    if !dsym_path.exists() {
        return Err(format!("dSYM path not found: '{}'", dsym_path.display()));
    }

    // If it's a file, assume it's the DWARF binary directly
    if dsym_path.is_file() {
        return Ok(dsym_path.to_path_buf());
    }

    // If it's a directory, look inside Contents/Resources/DWARF/
    let dwarf_dir = dsym_path.join("Contents/Resources/DWARF");
    if !dwarf_dir.is_dir() {
        return Err(format!(
            "not a valid dSYM bundle (missing Contents/Resources/DWARF): '{}'",
            dsym_path.display()
        ));
    }

    let entries = std::fs::read_dir(&dwarf_dir)
        .map_err(|e| format!("cannot read '{}': {e}", dwarf_dir.display()))?;

    for entry in entries {
        let Ok(entry) = entry else { continue };
        let path = entry.path();
        if path.is_file() {
            return Ok(path);
        }
    }

    Err(format!(
        "no DWARF binary found in '{}'",
        dwarf_dir.display()
    ))
}

/// Parsed image with base address and slide.
struct ImageSlide {
    base: u64,
    slide: u64,
}

/// Build a sorted slide lookup from the report's `loaded_images`.
fn build_slide_map(images: &[LoadedImageReport]) -> Vec<ImageSlide> {
    let mut slides: Vec<ImageSlide> = images
        .iter()
        .filter_map(|img| {
            let base = parse_hex_address(&img.base)?;
            let slide = img
                .slide
                .as_deref()
                .and_then(parse_hex_address)
                .unwrap_or(0);
            Some(ImageSlide { base, slide })
        })
        .collect();
    slides.sort_by_key(|s| s.base);
    slides
}

/// Find the ASLR slide for an address by finding its containing image.
fn find_slide_for_address(slides: &[ImageSlide], addr: u64) -> u64 {
    let idx = slides.partition_point(|s| s.base <= addr);
    if idx == 0 {
        return 0;
    }
    slides.get(idx - 1).map_or(0, |s| s.slide)
}

/// Parse a hex address string like `0x0000000100a3b1a8` to `u64`.
fn parse_hex_address(s: &str) -> Option<u64> {
    let s = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    u64::from_str_radix(s, 16).ok()
}

/// Write the modified report to disk.
fn write_report(report: &CrashReport, path: &Path) -> Result<(), String> {
    let json =
        serde_json::to_vec_pretty(report).map_err(|e| format!("JSON serialization failed: {e}"))?;
    write_private_output(path, &json)
        .map_err(|e| format!("failed to write '{}': {e}", path.display()))
}

fn write_private_output(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let requested_parent = path
        .parent()
        .ok_or_else(|| format!("output path has no parent: '{}'", path.display()))?;
    let parent_path = if requested_parent.as_os_str().is_empty() {
        std::env::current_dir()
            .map_err(|error| format!("cannot resolve current directory: {error}"))?
    } else if requested_parent.is_absolute() {
        requested_parent.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| format!("cannot resolve current directory: {error}"))?
            .join(requested_parent)
    };
    let destination_name = path
        .file_name()
        .ok_or_else(|| format!("output path has no filename: '{}'", path.display()))?;
    let parent = open_trusted_directory(&parent_path).map_err(|error| {
        format!(
            "cannot safely open output directory '{}': {error}",
            parent_path.display()
        )
    })?;

    let tmp_path = path.with_file_name(format!(
        ".{}.symbolicate-{}.tmp",
        destination_name.to_string_lossy(),
        uuid::Uuid::new_v4()
    ));
    let tmp_name = tmp_path
        .file_name()
        .ok_or_else(|| format!("temporary output has no filename: '{}'", tmp_path.display()))?;
    let descriptor = openat(
        &parent,
        tmp_name,
        OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::S_IRUSR | Mode::S_IWUSR,
    )
    .map_err(|error| format!("cannot create private temporary output: {error}"))?;
    let mut tmp = File::from(descriptor);
    let mut pending = match PendingOutput::new(&parent, tmp_name) {
        Ok(pending) => pending,
        Err(error) => {
            drop(tmp);
            let _ =
                nix::unistd::unlinkat(&parent, tmp_name, nix::unistd::UnlinkatFlags::NoRemoveDir);
            return Err(error);
        }
    };
    tmp.set_permissions(fs::Permissions::from_mode(PRIVATE_FILE_MODE))
        .map_err(|error| format!("cannot set private temporary output mode: {error}"))?;
    validate_private_file(&tmp, &tmp_path)?;
    let write_result = (|| -> Result<(), String> {
        tmp.write_all(bytes)
            .map_err(|error| format!("cannot write temporary output: {error}"))?;
        tmp.flush()
            .map_err(|error| format!("cannot flush temporary output: {error}"))?;
        tmp.sync_all()
            .map_err(|error| format!("cannot sync temporary output: {error}"))?;
        Ok(())
    })();
    drop(tmp);
    write_result?;

    if validate_existing_output(&parent, destination_name, path)? {
        renameat(&parent, tmp_name, &parent, destination_name)
            .map_err(|error| format!("cannot atomically replace output: {error}"))?;
    } else {
        publish_new_output(&parent, tmp_name, destination_name)
            .map_err(|error| format!("cannot exclusively publish output: {error}"))?;
    }
    pending.published();
    parent
        .sync_all()
        .map_err(|error| format!("cannot sync output directory: {error}"))?;
    Ok(())
}

fn validate_existing_output(
    parent: &File,
    name: &std::ffi::OsStr,
    path: &Path,
) -> Result<bool, String> {
    match openat(
        parent,
        name,
        OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
        Mode::empty(),
    ) {
        Ok(descriptor) => {
            let file = File::from(descriptor);
            validate_private_file(&file, path)?;
            Ok(true)
        }
        Err(nix::errno::Errno::ENOENT) => Ok(false),
        Err(error) => Err(format!(
            "cannot safely open existing output '{}': {error}",
            path.display()
        )),
    }
}

#[cfg(target_os = "macos")]
fn publish_new_output(
    parent: &File,
    source_name: &std::ffi::OsStr,
    destination_name: &std::ffi::OsStr,
) -> Result<(), std::io::Error> {
    const RENAME_EXCL: u32 = 0x0000_0004;

    unsafe extern "C" {
        fn renameatx_np(
            from_fd: nix::libc::c_int,
            from: *const nix::libc::c_char,
            to_fd: nix::libc::c_int,
            to: *const nix::libc::c_char,
            flags: u32,
        ) -> nix::libc::c_int;
    }

    let source = std::ffi::CString::new(source_name.as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let destination = std::ffi::CString::new(destination_name.as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    // SAFETY: `parent` is a live directory descriptor and both C strings are
    // NUL-terminated for the duration of the Darwin renameatx_np call.
    let status = unsafe {
        renameatx_np(
            parent.as_raw_fd(),
            source.as_ptr(),
            parent.as_raw_fd(),
            destination.as_ptr(),
            RENAME_EXCL,
        )
    };
    if status == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(target_os = "macos"))]
fn publish_new_output(
    _parent: &File,
    _source_name: &std::ffi::OsStr,
    _destination_name: &std::ffi::OsStr,
) -> Result<(), std::io::Error> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "exclusive output publication requires macOS",
    ))
}

struct PendingOutput {
    parent: File,
    name: std::ffi::OsString,
    published: bool,
}

impl PendingOutput {
    fn new(parent: &File, name: &std::ffi::OsStr) -> Result<Self, String> {
        Ok(Self {
            parent: parent
                .try_clone()
                .map_err(|error| format!("cannot retain output directory handle: {error}"))?,
            name: name.to_os_string(),
            published: false,
        })
    }

    fn published(&mut self) {
        self.published = true;
    }
}

impl Drop for PendingOutput {
    fn drop(&mut self) {
        if !self.published {
            let _ = nix::unistd::unlinkat(
                &self.parent,
                self.name.as_os_str(),
                nix::unistd::UnlinkatFlags::NoRemoveDir,
            );
        }
    }
}

/// Print a summary of symbolicated backtraces to stdout.
fn print_symbolicated_backtrace(report: &CrashReport) {
    for thread in &report.threads {
        let label = if thread.crashed { " [CRASHED]" } else { "" };
        let name = thread.name.as_deref().unwrap_or("unnamed");
        println!("Thread {} ({name}){label}:", thread.index);

        for (i, frame) in thread.backtrace.iter().enumerate() {
            let sym = frame.symbol.as_deref().unwrap_or("???");
            let offset_str = frame
                .offset
                .as_deref()
                .map_or(String::new(), |o| format!(" + {o}"));
            let source = match (&frame.file, frame.line) {
                (Some(f), Some(l)) => {
                    let short = f.rsplit('/').next().unwrap_or(f);
                    match frame.column {
                        Some(c) if c > 0 => format!("  {short}:{l}:{c}"),
                        _ => format!("  {short}:{l}"),
                    }
                }
                _ => String::new(),
            };
            println!("  #{i:<3} {}{sym}{offset_str}{source}", frame.address);
        }
        println!();
    }
}

#[cfg(test)]
#[path = "../../tests/unit/cli/symbolicate_tests.rs"]
mod tests;
