//! Pre-processor: compute a crash fingerprint hash from the crashed thread's backtrace.
//!
//! The fingerprint groups identical crashes together by hashing the top N
//! application-frame symbols (excluding system libraries). Reports with the
//! same fingerprint likely stem from the same root cause.

use crate::collectors::dylib;
use crate::pipeline::{CollectedData, CrashEvent, Plugin, PreProcessor, Priority};
use sha2::{Digest, Sha256};

/// System library path prefixes — frames from these images are excluded
/// from the fingerprint to focus on application code.
const SYSTEM_PREFIXES: &[&str] = &[
    "/usr/lib/",
    "/System/Library/",
    "/Library/Apple/",
    "/AppleInternal/",
];

pub struct Fingerprinter {
    top_n: usize,
}

impl Fingerprinter {
    #[must_use]
    pub fn new(top_n: usize) -> Self {
        Self { top_n }
    }
}

impl Plugin for Fingerprinter {
    fn name(&self) -> &'static str {
        "Fingerprinter"
    }

    fn priority(&self) -> Priority {
        Priority::Low
    }

    fn depends_on(&self) -> &'static [&'static str] {
        &["SymbolResolver"]
    }

    fn is_available(&self) -> bool {
        true
    }
}

impl PreProcessor for Fingerprinter {
    fn process(&self, _event: &CrashEvent, data: &mut CollectedData) -> Result<(), String> {
        // Find the crashed thread, or fall back to thread 0
        let thread = data
            .raw
            .threads
            .iter()
            .find(|t| t.crashed)
            .or_else(|| data.raw.threads.first());

        let Some(thread) = thread else {
            return Ok(());
        };

        let images = &data.raw.images;
        let symbols = &data.raw.symbols;

        // Collect up to top_n app-frame identifiers
        let mut frame_ids: Vec<String> = Vec::with_capacity(self.top_n);

        for &addr in &thread.backtrace {
            if addr == 0 {
                continue;
            }
            if frame_ids.len() >= self.top_n {
                break;
            }

            let (image_name, offset) = dylib::resolve_address(images, addr);

            // Skip system frames
            if let Some(ref img) = image_name {
                if is_system_image_name(images, addr) {
                    continue;
                }
                // Build the frame identifier: prefer symbol, fall back to image+offset
                let id = if let Some(sym) = symbols.get(&addr) {
                    sym.clone()
                } else {
                    // No symbol — use image name + offset for distinctiveness
                    let off = offset.unwrap_or(0);
                    format!("{img}+{off:#x}")
                };
                frame_ids.push(id);
            }
        }

        // Compute SHA-256 of concatenated frame identifiers
        let mut hasher = Sha256::new();
        for (i, id) in frame_ids.iter().enumerate() {
            if i > 0 {
                hasher.update(b"\n");
            }
            hasher.update(id.as_bytes());
        }
        let hash = hasher.finalize();

        // Truncate to 16 hex chars (64 bits)
        let fingerprint = format!(
            "{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            hash[0], hash[1], hash[2], hash[3], hash[4], hash[5], hash[6], hash[7]
        );

        data.fingerprint = Some(fingerprint);
        Ok(())
    }
}

/// Check if a backtrace address belongs to a system image by examining
/// the full path of the image containing that address.
fn is_system_image_name(images: &[dylib::RawImageData], addr: u64) -> bool {
    // Find the image containing this address (same lookup as resolve_address)
    let idx = images.partition_point(|img| img.base_address <= addr);
    if idx == 0 {
        return false;
    }
    let img = images.get(idx - 1);
    match img {
        Some(img) => SYSTEM_PREFIXES.iter().any(|p| img.path.starts_with(p)),
        None => false,
    }
}

/// Check if a path string is a system image path.
#[cfg(test)]
pub fn is_system_image(path: &str) -> bool {
    SYSTEM_PREFIXES.iter().any(|p| path.starts_with(p))
}

#[cfg(test)]
#[path = "../../tests/unit/preprocessors/fingerprint_tests.rs"]
mod tests;
