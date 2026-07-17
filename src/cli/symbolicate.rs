//! `crash_monitor symbolicate <report.json> --dsym <path>` — resolve addresses to `source:line`.
//!
//! Uses `addr2line::Loader` to parse DWARF debug info from a dSYM bundle
//! and resolve backtrace addresses to source file, function, and line number.

use crate::pipeline::report::{self, CrashReport, LoadedImageReport};
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
    let json = serde_json::to_string_pretty(report)
        .map_err(|e| format!("JSON serialization failed: {e}"))?;
    std::fs::write(path, json).map_err(|e| format!("failed to write '{}': {e}", path.display()))
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
