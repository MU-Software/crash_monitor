//! Pre-processor: resolve backtrace addresses to function names.
//!
//! Reads Mach-O symbol tables (`LC_SYMTAB` → `nlist_64` + string table) from
//! on-disk binary files to resolve backtrace addresses to function symbols.
//! Uses image paths from `DylibCollector` + ASLR slide to map runtime addresses.

use crate::collectors::dylib::RawImageData;
use crate::pipeline::{CollectedData, CrashEvent, Plugin, PreProcessor, Priority};
use std::collections::BTreeMap;
use std::fs;

// Mach-O constants
const MH_MAGIC_64: u32 = 0xFEED_FACF;
const LC_SYMTAB: u32 = 0x02;

/// `N_SECT`: symbol is defined in a section.
const N_SECT: u8 = 0x0E;
/// Mask for `n_type` symbol type bits.
const N_TYPE_MASK: u8 = 0x0E;

struct NlistEntry {
    address: u64,
    name: String,
}

struct SymtabInfo {
    sym_offset: u32,
    nsyms: u32,
    str_offset: u32,
    str_size: u32,
}

pub struct SymbolResolver;

impl SymbolResolver {
    pub fn new() -> Self {
        Self
    }
}

impl Plugin for SymbolResolver {
    fn name(&self) -> &'static str {
        "SymbolResolver"
    }
    fn priority(&self) -> Priority {
        Priority::Low
    }
}

impl PreProcessor for SymbolResolver {
    fn process(&self, _event: &CrashEvent, data: &mut CollectedData) -> Result<(), String> {
        let mut addresses: Vec<u64> = data
            .raw
            .threads
            .iter()
            .flat_map(|t| t.backtrace.iter().copied())
            .filter(|&a| a != 0)
            .collect();
        addresses.sort_unstable();
        addresses.dedup();

        if addresses.is_empty() {
            return Ok(());
        }

        let mut symbols: BTreeMap<u64, String> = BTreeMap::new();

        for img in &data.raw.images {
            let img_addrs: Vec<u64> = addresses
                .iter()
                .copied()
                .filter(|&a| {
                    a >= img.base_address && a < img.base_address.saturating_add(0x1000_0000)
                })
                .collect();

            if img_addrs.is_empty() {
                continue;
            }

            if let Some(resolved) = resolve_image_symbols_from_disk(img, &img_addrs) {
                symbols.extend(resolved);
            }
        }

        if !symbols.is_empty() {
            eprintln!(
                "[monitor] SymbolResolver: resolved {} symbols",
                symbols.len()
            );
        }

        data.raw.symbols = symbols;
        Ok(())
    }
}

/// Maximum file size to read (256 MB). Prevents OOM on unexpectedly large binaries.
const MAX_FILE_SIZE: u64 = 256 * 1024 * 1024;

/// Maximum number of nlist entries to parse. A typical large app has ~100K symbols;
/// 2M is generous enough for any real binary while preventing excessive memory/time
/// on corrupted symtab headers.
const MAX_NLIST_ENTRIES: u32 = 2_000_000;

/// Read the symbol table from a Mach-O file on disk and resolve the given addresses.
fn resolve_image_symbols_from_disk(
    img: &RawImageData,
    addresses: &[u64],
) -> Option<Vec<(u64, String)>> {
    // Check file size before reading to prevent OOM
    let metadata = fs::metadata(&img.path).ok()?;
    if metadata.len() > MAX_FILE_SIZE {
        return None;
    }

    let file_data = fs::read(&img.path).ok()?;
    if file_data.len() < 32 {
        return None;
    }

    let magic = read_u32_le(&file_data, 0)?;
    if magic != MH_MAGIC_64 {
        return None;
    }

    let ncmds = read_u32_le(&file_data, 16)? as usize;
    let sizeofcmds = read_u32_le(&file_data, 20)? as usize;
    if sizeofcmds > 1024 * 1024 || 32 + sizeofcmds > file_data.len() {
        return None;
    }

    // Parse LC_SYMTAB from load commands
    let mut symtab_info: Option<SymtabInfo> = None;
    let mut offset = 32;
    for _ in 0..ncmds {
        if offset + 8 > file_data.len() {
            break;
        }
        let cmd = read_u32_le(&file_data, offset)?;
        let cmdsize = read_u32_le(&file_data, offset + 4)? as usize;

        if cmd == LC_SYMTAB && cmdsize >= 24 {
            symtab_info = Some(SymtabInfo {
                sym_offset: read_u32_le(&file_data, offset + 8)?,
                nsyms: read_u32_le(&file_data, offset + 12)?,
                str_offset: read_u32_le(&file_data, offset + 16)?,
                str_size: read_u32_le(&file_data, offset + 20)?,
            });
            break;
        }

        if cmdsize == 0 {
            break;
        }
        offset = match offset.checked_add(cmdsize) {
            Some(next) => next,
            None => break, // overflow in corrupted binary
        };
    }

    let symtab = symtab_info?;

    // Validate before arithmetic to prevent overflow
    if symtab.nsyms > MAX_NLIST_ENTRIES {
        return None;
    }
    let nlist_bytes = (symtab.nsyms as usize).checked_mul(16)?;
    let sym_end = (symtab.sym_offset as usize).checked_add(nlist_bytes)?;
    let str_end = (symtab.str_offset as usize).checked_add(symtab.str_size as usize)?;
    if sym_end > file_data.len() || str_end > file_data.len() {
        return None;
    }

    let strtab = &file_data[symtab.str_offset as usize..str_end];
    let nlist_data = &file_data[symtab.sym_offset as usize..sym_end];
    let slide = img.slide.unwrap_or(0);

    // Parse nlist entries
    let mut func_syms: Vec<NlistEntry> = Vec::new();
    for i in 0..symtab.nsyms as usize {
        let off = i * 16;
        let n_strx = read_u32_le(nlist_data, off)? as usize;
        let n_type = *nlist_data.get(off + 4)?;
        let n_value = read_u64_le(nlist_data, off + 8)?;

        if (n_type & N_TYPE_MASK) != N_SECT || n_value == 0 {
            continue;
        }
        if n_strx >= strtab.len() {
            continue;
        }

        let name_end = strtab[n_strx..]
            .iter()
            .position(|&b| b == 0)
            .map_or(strtab.len(), |p| n_strx + p);
        let raw_name = String::from_utf8_lossy(&strtab[n_strx..name_end]);

        // Strip leading underscore (C symbol convention on macOS)
        let name = if let Some(stripped) = raw_name.strip_prefix('_') {
            stripped.to_string()
        } else {
            raw_name.into_owned()
        };

        func_syms.push(NlistEntry {
            address: n_value.wrapping_add(slide),
            name,
        });
    }

    func_syms.sort_by_key(|s| s.address);

    let mut result = Vec::new();
    for &addr in addresses {
        if let Some(sym) = find_symbol(&func_syms, addr) {
            result.push((addr, sym));
        }
    }

    Some(result)
}

/// Find the function symbol that contains the given address (nearest lower bound).
fn find_symbol(sorted_syms: &[NlistEntry], address: u64) -> Option<String> {
    if sorted_syms.is_empty() {
        return None;
    }
    let idx = sorted_syms.partition_point(|s| s.address <= address);
    if idx == 0 {
        return None;
    }
    let sym = &sorted_syms[idx - 1];
    // Sanity: symbol shouldn't be more than 1MB away
    if address - sym.address > 1024 * 1024 {
        return None;
    }
    Some(sym.name.clone())
}

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
