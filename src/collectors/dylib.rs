//! Collector: loaded images + ASLR slide computation.
//!
//! Self-contained — absorbs all logic from `dylib_info` and `memory_reader::read_c_string`.
//! Slide is computed within the collector (during suspension) to avoid
//! cross-category dependency with a pre-processor.

use std::sync::Arc;

use crate::pipeline::{CollectedData, Collector, CrashEvent, Plugin, Priority};
use crate::platform::PlatformOps;
use mach2::port::mach_port_t;

// ═══════════════════════════════════════════════════
//  Raw data types
// ═══════════════════════════════════════════════════

/// Information about a loaded dynamic library / image.
#[derive(Debug, Clone)]
pub struct RawImageData {
    pub path: String,
    pub base_address: u64,
    pub slide: Option<u64>,
}

// ═══════════════════════════════════════════════════
//  Plugin + Collector implementation
// ═══════════════════════════════════════════════════

pub struct DylibCollector {
    platform: Arc<dyn PlatformOps>,
}

impl DylibCollector {
    pub fn new(platform: Arc<dyn PlatformOps>) -> Self {
        Self { platform }
    }
}

impl Plugin for DylibCollector {
    fn name(&self) -> &'static str {
        "DylibCollector"
    }
    fn priority(&self) -> Priority {
        Priority::Medium
    }
}

impl Collector for DylibCollector {
    fn collect(
        &self,
        _event: &CrashEvent,
        task: mach_port_t,
        data: &mut CollectedData,
    ) -> Result<(), String> {
        let platform = self.platform.as_ref();
        data.raw.images = enumerate_loaded_images(platform, task).unwrap_or_else(|e| {
            eprintln!("[monitor] dylib collection failed: {e}");
            vec![]
        });
        compute_all_slides(platform, task, &mut data.raw.images);
        Ok(())
    }
}

// ═══════════════════════════════════════════════════
//  dyld C struct definitions (mirror C layout)
// ═══════════════════════════════════════════════════

const TASK_DYLD_INFO: u32 = 17;

/// Size of `TaskDyldInfo` C struct: u64 + u64 + i32 = 20 bytes.
const TASK_DYLD_INFO_SIZE: usize = 20;

#[repr(C)]
struct DyldAllImageInfos64 {
    _version: u32,
    info_array_count: u32,
    info_array: u64,
}

#[repr(C)]
#[allow(clippy::struct_field_names)] // matches dyld C header naming
struct DyldImageInfo64 {
    image_load_address: u64,
    image_file_path: u64,
    _image_file_mod_date: u64,
}

// ═══════════════════════════════════════════════════
//  Image enumeration (absorbed from dylib_info.rs)
// ═══════════════════════════════════════════════════

/// Enumerate all loaded images in the target process.
fn enumerate_loaded_images(
    platform: &dyn PlatformOps,
    task: mach_port_t,
) -> Result<Vec<RawImageData>, String> {
    // Step 1: Get dyld_all_image_infos address via task_info
    let mut buf = [0u8; TASK_DYLD_INFO_SIZE];
    platform
        .get_task_info_bytes(task, TASK_DYLD_INFO, &mut buf)
        .map_err(|e| format!("task_info(TASK_DYLD_INFO) failed: {e}"))?;

    // Parse TaskDyldInfo fields: u64 all_image_info_addr (offset 0),
    // u64 all_image_info_size (offset 8), i32 all_image_info_format (offset 16)
    let infos_addr = read_u64_le(&buf, 0).ok_or("failed to parse all_image_info_addr")?;
    if infos_addr == 0 {
        return Err("dyld_all_image_infos address is NULL".into());
    }

    // Step 2: Read dyld_all_image_infos to get array pointer and count
    let infos_bytes = platform
        .vm_read(task, infos_addr, std::mem::size_of::<DyldAllImageInfos64>())
        .map_err(|e| format!("Failed to read dyld_all_image_infos: {e}"))?;

    let (info_array, info_count) = parse_all_image_infos(&infos_bytes)?;
    if info_array == 0 || info_count == 0 {
        return Ok(Vec::new());
    }

    // Safety cap: a process rarely has more than 2000 loaded images.
    // Corrupted dyld info could report millions, causing excessive vm_read.
    let info_count = info_count.min(2000);

    // Step 3: Read the image info array
    let entry_size = std::mem::size_of::<DyldImageInfo64>();
    let array_bytes = platform
        .vm_read(task, info_array, info_count * entry_size)
        .map_err(|e| format!("Failed to read image info array: {e}"))?;

    let mut images = Vec::with_capacity(info_count);
    for i in 0..info_count {
        let (load_addr, path_addr) = parse_image_info(&array_bytes, i * entry_size)?;

        let path = read_c_string(platform, task, path_addr, 512)
            .unwrap_or_else(|_| format!("<unreadable@{path_addr:#x}>"));

        images.push(RawImageData {
            path,
            base_address: load_addr,
            slide: None, // computed post-enumeration by compute_all_slides()
        });
    }

    images.sort_by_key(|img| img.base_address);
    Ok(images)
}

// ═══════════════════════════════════════════════════
//  Byte parsing helpers
// ═══════════════════════════════════════════════════

fn read_u32_le(bytes: &[u8], offset: usize) -> Option<u32> {
    bytes
        .get(offset..offset + 4)?
        .try_into()
        .ok()
        .map(u32::from_le_bytes)
}

fn read_u64_le(bytes: &[u8], offset: usize) -> Option<u64> {
    bytes
        .get(offset..offset + 8)?
        .try_into()
        .ok()
        .map(u64::from_le_bytes)
}

/// Parse `DyldAllImageInfos64` from a byte buffer. Returns (`info_array_addr`, `info_count`).
fn parse_all_image_infos(bytes: &[u8]) -> Result<(u64, usize), String> {
    // Fields: version(u32), info_array_count(u32), info_array(u64) = 16 bytes minimum
    if bytes.len() < 16 {
        return Err(format!(
            "DyldAllImageInfos64 buffer too small: {}",
            bytes.len()
        ));
    }
    let count = read_u32_le(bytes, 4).ok_or("failed to parse info_array_count")? as usize;
    let array = read_u64_le(bytes, 8).ok_or("failed to parse info_array")?;
    Ok((array, count))
}

/// Parse a `DyldImageInfo64` from a byte buffer at the given offset.
/// Returns (`image_load_address`, `image_file_path`).
fn parse_image_info(bytes: &[u8], offset: usize) -> Result<(u64, u64), String> {
    if offset + 16 > bytes.len() {
        return Err(format!(
            "DyldImageInfo64 at offset {offset} exceeds buffer (len={})",
            bytes.len()
        ));
    }
    let load_addr = read_u64_le(bytes, offset).ok_or("failed to parse image_load_address")?;
    let path_addr = read_u64_le(bytes, offset + 8).ok_or("failed to parse image_file_path")?;
    Ok((load_addr, path_addr))
}

// ═══════════════════════════════════════════════════
//  ASLR slide computation (absorbed from dylib_info.rs)
// ═══════════════════════════════════════════════════

// Mach-O constants for ASLR slide computation
const MH_MAGIC_64: u32 = 0xFEED_FACF;
const LC_SEGMENT_64: u32 = 0x19;

/// Compute ASLR slides for all images. Call this post-resume — it does
/// 2 `vm_read` calls per image and doesn't need the child to be suspended
/// (Mach-O headers are file-backed and don't change).
fn compute_all_slides(platform: &dyn PlatformOps, task: mach_port_t, images: &mut [RawImageData]) {
    for img in images {
        img.slide = compute_slide(platform, task, img.base_address);
    }
}

/// Compute the ASLR slide for an image by reading its Mach-O header.
/// `slide` = `base_address` - `__TEXT` segment `vmaddr`.
fn compute_slide(platform: &dyn PlatformOps, task: mach_port_t, base_address: u64) -> Option<u64> {
    // Read mach_header_64: magic(4) + cputype(4) + cpusubtype(4) + filetype(4) +
    //                      ncmds(4) + sizeofcmds(4) + flags(4) + reserved(4) = 32 bytes
    let header = platform.vm_read(task, base_address, 32).ok()?;
    let magic = read_u32_le(&header, 0)?;
    if magic != MH_MAGIC_64 {
        return None;
    }

    let ncmds = read_u32_le(&header, 16)? as usize;
    let sizeofcmds = read_u32_le(&header, 20)? as usize;

    // Safety cap: refuse to read more than 1MB of load commands from remote memory.
    // A normal binary has ~50-200 load commands totaling ~10-50KB.
    if sizeofcmds > 1024 * 1024 {
        return None;
    }

    // Read all load commands (start at offset 32, right after the header)
    let cmds = platform.vm_read(task, base_address + 32, sizeofcmds).ok()?;

    let mut offset = 0;
    for _ in 0..ncmds {
        if offset + 8 > cmds.len() {
            break;
        }
        let cmd = read_u32_le(&cmds, offset)?;
        let cmdsize = read_u32_le(&cmds, offset + 4)? as usize;

        if cmd == LC_SEGMENT_64 && cmdsize >= 72 {
            // segment_command_64: cmd(4) + cmdsize(4) + segname(16) + vmaddr(8) + ...
            // segname at offset+8, vmaddr at offset+24
            let segname_bytes = cmds.get(offset + 8..offset + 24)?;
            let segname_end = segname_bytes.iter().position(|&b| b == 0).unwrap_or(16);
            let segname = std::str::from_utf8(&segname_bytes[..segname_end]).ok()?;

            if segname == "__TEXT" {
                let vmaddr = read_u64_le(&cmds, offset + 24)?;
                return Some(base_address.wrapping_sub(vmaddr));
            }
        }

        if cmdsize == 0 {
            break;
        }
        offset += cmdsize;
    }

    None
}

// ═══════════════════════════════════════════════════
//  Address resolution (public, used by report formatter)
// ═══════════════════════════════════════════════════

/// Resolve a backtrace address to an image name + offset.
pub fn resolve_address(images: &[RawImageData], address: u64) -> (Option<String>, Option<u64>) {
    let idx = images.partition_point(|img| img.base_address <= address);
    if idx == 0 {
        return (None, None);
    }
    let img = &images[idx - 1];
    let offset = address - img.base_address;

    let name = img.path.rsplit('/').next().unwrap_or(&img.path).to_string();
    (Some(name), Some(offset))
}

// ═══════════════════════════════════════════════════
//  C string reading (absorbed from memory_reader.rs)
// ═══════════════════════════════════════════════════

/// Read a C string (null-terminated) from the target task, up to `max_len` bytes.
/// Uses progressive reads (128 -> 512 -> `max_len`) to handle strings near page boundaries
/// where a full-length read would cross into an unmapped page and fail.
fn read_c_string(
    platform: &dyn PlatformOps,
    task: mach_port_t,
    address: u64,
    max_len: usize,
) -> Result<String, String> {
    // Try progressively larger reads — most paths are < 128 bytes,
    // so the first attempt usually succeeds even near page boundaries.
    for &chunk in &[128, 512, max_len] {
        let try_len = chunk.min(max_len);
        if let Ok(bytes) = platform.vm_read(task, address, try_len) {
            if let Some(end) = bytes.iter().position(|&b| b == 0) {
                return Ok(String::from_utf8_lossy(&bytes[..end]).into_owned());
            }
            // No null terminator found — try a larger read if available
            if try_len >= max_len {
                return Ok(String::from_utf8_lossy(&bytes).into_owned());
            }
        }
    }
    // All progressive reads failed
    Err(format!(
        "read_c_string at {address:#x}: all progressive reads failed"
    ))
}

#[cfg(test)]
#[path = "../../tests/unit/collectors/dylib_tests.rs"]
mod tests;
