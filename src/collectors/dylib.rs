//! Collector: loaded images + ASLR slide computation.
//!
//! Self-contained — absorbs all logic from `dylib_info` and `memory_reader::read_c_string`.
//! Slide is computed within the collector (during suspension) to avoid
//! cross-category dependency with a pre-processor.

use std::sync::Arc;

use crate::pipeline::{
    CollectedData, Collector, CollectorAccess, CrashEvent, Plugin, PluginContext, PluginExecution,
    Priority,
};
use crate::platform::PlatformOps;
use mach2::port::mach_port_t;
use serde::{Deserialize, Serialize};

// ═══════════════════════════════════════════════════
//  Raw data types
// ═══════════════════════════════════════════════════

/// Information about a loaded dynamic library / image.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawImageData {
    pub path: String,
    pub base_address: u64,
    pub slide: Option<u64>,
    #[serde(default)]
    pub uuid: Option<String>,
    #[serde(default)]
    pub architecture: Option<String>,
    #[serde(default)]
    pub text_start: Option<u64>,
    #[serde(default)]
    pub text_end: Option<u64>,
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
    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }
    fn priority(&self) -> Priority {
        Priority::Medium
    }
}

impl Collector for DylibCollector {
    fn access(&self) -> CollectorAccess {
        CollectorAccess::IsolatedTask
    }

    fn collect(
        &self,
        _event: &CrashEvent,
        task: mach_port_t,
        data: &mut CollectedData,
        context: &PluginContext,
    ) -> Result<(), String> {
        context.checkpoint()?;
        let platform = self.platform.as_ref();
        data.raw.images = enumerate_loaded_images(platform, task, context)
            .map_err(|error| format!("dylib image enumeration failed: {error}"))?;
        context.checkpoint()?;
        compute_all_slides(platform, task, &mut data.raw.images, context)?;
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
    context: &PluginContext,
) -> Result<Vec<RawImageData>, String> {
    context.checkpoint()?;
    // Step 1: Get dyld_all_image_infos address via task_info
    let mut buf = [0u8; TASK_DYLD_INFO_SIZE];
    platform
        .get_task_info_bytes(task, TASK_DYLD_INFO, &mut buf)
        .map_err(|e| format!("task_info(TASK_DYLD_INFO) failed: {e}"))?;
    context.checkpoint()?;

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
    context.checkpoint()?;

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
    context.checkpoint()?;

    let mut images = Vec::with_capacity(info_count);
    for i in 0..info_count {
        context.checkpoint()?;
        let (load_addr, path_addr) = parse_image_info(&array_bytes, i * entry_size)?;

        let path = read_c_string(platform, task, path_addr, 512, context)
            .unwrap_or_else(|_| format!("<unreadable@{path_addr:#x}>"));
        context.checkpoint()?;

        images.push(RawImageData {
            path,
            base_address: load_addr,
            slide: None, // computed post-enumeration by compute_all_slides()
            uuid: None,
            architecture: None,
            text_start: None,
            text_end: None,
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
const LC_UUID: u32 = 0x1b;

/// Compute ASLR slides for all images. Call this post-resume — it does
/// 2 `vm_read` calls per image and doesn't need the child to be suspended
/// (Mach-O headers are file-backed and don't change).
fn compute_all_slides(
    platform: &dyn PlatformOps,
    task: mach_port_t,
    images: &mut [RawImageData],
    context: &PluginContext,
) -> Result<(), String> {
    for img in images {
        context.checkpoint()?;
        compute_image_metadata(platform, task, img, context);
        context.checkpoint()?;
    }
    Ok(())
}

/// Compute the ASLR slide for an image by reading its Mach-O header.
/// `slide` = `base_address` - `__TEXT` segment `vmaddr`.
fn compute_image_metadata(
    platform: &dyn PlatformOps,
    task: mach_port_t,
    image: &mut RawImageData,
    context: &PluginContext,
) {
    if context.checkpoint().is_err() {
        return;
    }
    let base_address = image.base_address;
    // Read mach_header_64: magic(4) + cputype(4) + cpusubtype(4) + filetype(4) +
    //                      ncmds(4) + sizeofcmds(4) + flags(4) + reserved(4) = 32 bytes
    let Ok(header) = platform.vm_read(task, base_address, 32) else {
        return;
    };
    if context.checkpoint().is_err() {
        return;
    }
    let Some(magic) = read_u32_le(&header, 0) else {
        return;
    };
    if magic != MH_MAGIC_64 {
        return;
    }
    image.architecture = read_u32_le(&header, 4).map(|cpu| match cpu {
        0x0100_000c => "arm64".to_string(),
        0x0100_0007 => "x86_64".to_string(),
        other => format!("cpu_{other:#x}"),
    });

    let Some(ncmds) = read_u32_le(&header, 16).map(|value| value as usize) else {
        return;
    };
    let Some(sizeofcmds) = read_u32_le(&header, 20).map(|value| value as usize) else {
        return;
    };

    // Safety cap: refuse to read more than 1MB of load commands from remote memory.
    // A normal binary has ~50-200 load commands totaling ~10-50KB.
    if sizeofcmds > 1024 * 1024 {
        return;
    }

    // Read all load commands (start at offset 32, right after the header)
    let Some(commands_address) = base_address.checked_add(32) else {
        return;
    };
    let Ok(cmds) = platform.vm_read(task, commands_address, sizeofcmds) else {
        return;
    };
    if context.checkpoint().is_err() {
        return;
    }

    let mut offset = 0;
    for _ in 0..ncmds {
        if context.checkpoint().is_err() {
            return;
        }
        if offset + 8 > cmds.len() {
            break;
        }
        let Some(cmd) = read_u32_le(&cmds, offset) else {
            break;
        };
        let Some(cmdsize) = read_u32_le(&cmds, offset + 4).map(|value| value as usize) else {
            break;
        };

        if cmd == LC_SEGMENT_64 && cmdsize >= 72 {
            // segment_command_64: cmd(4) + cmdsize(4) + segname(16) + vmaddr(8) + ...
            // segname at offset+8, vmaddr at offset+24
            let Some(segname_bytes) = cmds.get(offset + 8..offset + 24) else {
                break;
            };
            let segname_end = segname_bytes.iter().position(|&b| b == 0).unwrap_or(16);
            let Ok(segname) = std::str::from_utf8(&segname_bytes[..segname_end]) else {
                break;
            };

            if segname == "__TEXT" {
                let Some(vmaddr) = read_u64_le(&cmds, offset + 24) else {
                    break;
                };
                let Some(vmsize) = read_u64_le(&cmds, offset + 32) else {
                    break;
                };
                image.slide = base_address.checked_sub(vmaddr);
                image.text_start = Some(base_address);
                image.text_end = base_address.checked_add(vmsize);
            }
        } else if cmd == LC_UUID && cmdsize >= 24 {
            if let Some(uuid) = cmds.get(offset + 8..offset + 24) {
                image.uuid = Some(format_uuid(uuid));
            }
        }

        if cmdsize == 0 {
            break;
        }
        let Some(next) = offset.checked_add(cmdsize) else {
            break;
        };
        if next > cmds.len() {
            break;
        }
        offset = next;
    }
}

fn format_uuid(bytes: &[u8]) -> String {
    bytes
        .iter()
        .enumerate()
        .map(|(index, byte)| {
            let separator = if matches!(index, 4 | 6 | 8 | 10) {
                "-"
            } else {
                ""
            };
            format!("{separator}{byte:02x}")
        })
        .collect()
}

// ═══════════════════════════════════════════════════
//  Address resolution (public, used by report formatter)
// ═══════════════════════════════════════════════════

/// Resolve a backtrace address to an image name + offset.
pub fn resolve_address(images: &[RawImageData], address: u64) -> (Option<String>, Option<u64>) {
    let Some(img) = images.iter().find(|image| {
        image
            .text_start
            .zip(image.text_end)
            .is_some_and(|(start, end)| address >= start && address < end)
    }) else {
        return (None, None);
    };
    let offset = address - img.text_start.unwrap_or(img.base_address);

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
    context: &PluginContext,
) -> Result<String, String> {
    // Try progressively larger reads — most paths are < 128 bytes,
    // so the first attempt usually succeeds even near page boundaries.
    let mut last_prefix = None;
    for &chunk in &[128, 512, max_len] {
        context.checkpoint()?;
        let try_len = chunk.min(max_len);
        if let Ok(bytes) = platform.vm_read(task, address, try_len) {
            context.checkpoint()?;
            if let Some(end) = bytes.iter().position(|&b| b == 0) {
                return Ok(String::from_utf8_lossy(&bytes[..end]).into_owned());
            }
            last_prefix = Some(String::from_utf8_lossy(&bytes).into_owned());
            // No null terminator found — try a larger read if available
            if try_len >= max_len {
                return Ok(String::from_utf8_lossy(&bytes).into_owned());
            }
        }
    }
    if let Some(prefix) = last_prefix {
        return Ok(prefix);
    }
    // All progressive reads failed
    Err(format!(
        "read_c_string at {address:#x}: all progressive reads failed"
    ))
}

#[cfg(test)]
#[path = "../../tests/unit/collectors/dylib_tests.rs"]
mod tests;
