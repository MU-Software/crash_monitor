//! Pre-processor: resolve backtrace addresses to function names.
//!
//! Reads Mach-O symbol tables (`LC_SYMTAB` → `nlist_64` + string table) from
//! on-disk binary files to resolve backtrace addresses to function symbols.
//! Uses image paths from `DylibCollector` + ASLR slide to map runtime addresses.

use crate::collectors::dylib::RawImageData;
use crate::collectors::thread::RawThreadData;
use crate::pipeline::{
    CollectedData, CrashEvent, Plugin, PluginContext, PluginExecution, PreProcessor, Priority,
};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;

// Mach-O constants
const MH_MAGIC_64: u32 = 0xFEED_FACF;
const LC_SYMTAB: u32 = 0x02;

/// `N_SECT`: symbol is defined in a section.
const N_SECT: u8 = 0x0E;
/// Mask for `n_type` symbol type bits.
const N_TYPE_MASK: u8 = 0x0E;

/// Bound all target-provided collections before symbol parsing work begins.
const MAX_THREADS_TO_SYMBOLICATE: usize = 4096;
const MAX_BACKTRACE_FRAMES_TO_SYMBOLICATE: usize = 128 * 1024;
const MAX_UNIQUE_ADDRESSES_TO_SYMBOLICATE: usize = 64 * 1024;
const MAX_IMAGES_TO_SYMBOLICATE: usize = 2000;

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
    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }
    fn priority(&self) -> Priority {
        Priority::Low
    }
}

fn collect_symbolication_addresses(
    threads: &[RawThreadData],
    context: &PluginContext,
    max_threads: usize,
    max_frames: usize,
    max_unique_addresses: usize,
) -> Result<Vec<u64>, String> {
    if max_unique_addresses == 0 {
        return Ok(Vec::new());
    }
    let mut addresses = BTreeSet::new();
    let mut frames_seen = 0_usize;

    // Preserve the faulting thread under global thread/frame/address caps even
    // when it appears late in the kernel-provided thread list.
    let prioritized_threads = threads
        .iter()
        .filter(|thread| thread.crashed)
        .chain(threads.iter().filter(|thread| !thread.crashed));

    'threads: for thread in prioritized_threads.take(max_threads) {
        context.checkpoint()?;
        for &address in &thread.backtrace {
            if frames_seen >= max_frames {
                break 'threads;
            }
            frames_seen += 1;
            context.checkpoint()?;
            if address != 0 {
                addresses.insert(address);
                if addresses.len() >= max_unique_addresses {
                    break 'threads;
                }
            }
        }
    }

    Ok(addresses.into_iter().collect())
}

impl PreProcessor for SymbolResolver {
    fn process(
        &self,
        _event: &CrashEvent,
        data: &mut CollectedData,
        context: &PluginContext,
    ) -> Result<(), String> {
        context.checkpoint()?;
        let addresses = collect_symbolication_addresses(
            &data.raw.threads,
            context,
            MAX_THREADS_TO_SYMBOLICATE,
            MAX_BACKTRACE_FRAMES_TO_SYMBOLICATE,
            MAX_UNIQUE_ADDRESSES_TO_SYMBOLICATE,
        )?;

        if addresses.is_empty() {
            return Ok(());
        }

        let mut symbols: BTreeMap<u64, String> = BTreeMap::new();

        for img in data.raw.images.iter().take(MAX_IMAGES_TO_SYMBOLICATE) {
            context.checkpoint()?;
            let img_addrs: Vec<u64> = addresses
                .iter()
                .copied()
                .filter(|&address| {
                    img.text_start
                        .zip(img.text_end)
                        .is_some_and(|(start, end)| address >= start && address < end)
                })
                .collect();

            if img_addrs.is_empty() {
                continue;
            }

            if let Some(resolved) = resolve_image_symbols_from_disk(img, &img_addrs, context) {
                symbols.extend(resolved);
            }
            context.checkpoint()?;
        }

        if !symbols.is_empty() {
            eprintln!(
                "[monitor] SymbolResolver: resolved {} symbols",
                symbols.len()
            );
        }

        data.raw.symbols = symbols;
        context.checkpoint()?;
        Ok(())
    }
}

/// Maximum file size to read (256 MB). Prevents OOM on unexpectedly large binaries.
const MAX_FILE_SIZE: u64 = 256 * 1024 * 1024;

/// Bound each cooperative read independently of the file size.
const FILE_READ_BUFFER_SIZE: usize = 64 * 1024;

/// Bound repeated `EINTR` results even when no plugin deadline was configured.
const MAX_FILE_READ_ATTEMPTS_PER_CHUNK: usize = 1024;

/// Corrupted Mach-O headers must not turn load-command parsing into an
/// effectively unbounded loop.
const MAX_LOAD_COMMANDS: usize = 64 * 1024;

/// Maximum number of nlist entries to parse. A typical large app has ~100K symbols;
/// 2M is generous enough for any real binary while preventing excessive memory/time
/// on corrupted symtab headers.
const MAX_NLIST_ENTRIES: u32 = 2_000_000;

/// Bound allocations caused by target-controlled symbol names.
const MAX_SYMBOL_NAME_BYTES: usize = 4096;
const MAX_TOTAL_SYMBOL_NAME_BYTES: usize = 64 * 1024 * 1024;
const MAX_FUNCTION_SYMBOLS: usize = 500_000;

fn read_bounded<R: Read>(
    reader: &mut R,
    initial_capacity: usize,
    max_size: u64,
    context: &PluginContext,
) -> Option<Vec<u8>> {
    let probe_limit = max_size.checked_add(1)?;
    let mut data = Vec::with_capacity(initial_capacity);
    let mut buffer = vec![0_u8; FILE_READ_BUFFER_SIZE];

    loop {
        context.checkpoint().ok()?;
        let current_size = u64::try_from(data.len()).ok()?;
        let remaining_probe = probe_limit.checked_sub(current_size)?;
        let read_len = usize::try_from(remaining_probe)
            .unwrap_or(buffer.len())
            .min(buffer.len());
        let mut attempts = 0_usize;
        let bytes_read = loop {
            context.checkpoint().ok()?;
            if attempts >= MAX_FILE_READ_ATTEMPTS_PER_CHUNK {
                return None;
            }
            attempts += 1;

            match reader.read(&mut buffer[..read_len]) {
                Ok(bytes_read) => break bytes_read,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return None,
            }
        };
        context.checkpoint().ok()?;

        if bytes_read == 0 {
            return Some(data);
        }

        let next_len = data.len().checked_add(bytes_read)?;
        if u64::try_from(next_len).ok()? > max_size {
            return None;
        }
        data.extend_from_slice(&buffer[..bytes_read]);
    }
}

fn read_regular_file_bounded(path: &str, context: &PluginContext) -> Option<Vec<u8>> {
    context.checkpoint().ok()?;
    // Framework binaries commonly use a facade such as
    // `Foo.framework/Foo -> Versions/Current/Foo`. Resolve that legitimate
    // chain first, then refuse a last-moment final-component symlink swap via
    // O_NOFOLLOW and admit only a regular file via fstat below.
    let canonical_path = std::fs::canonicalize(path).ok()?;
    context.checkpoint().ok()?;
    let mut file = OpenOptions::new()
        .read(true)
        // Avoid following a replaced canonical target and avoid blocking while
        // opening a FIFO. The fstat below then admits regular files only.
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(canonical_path)
        .ok()?;
    context.checkpoint().ok()?;

    let metadata = file.metadata().ok()?;
    if !metadata.file_type().is_file() || metadata.len() > MAX_FILE_SIZE {
        return None;
    }
    context.checkpoint().ok()?;

    let initial_capacity = usize::try_from(metadata.len()).ok()?;
    read_bounded(&mut file, initial_capacity, MAX_FILE_SIZE, context)
}

fn find_symtab_info(file_data: &[u8], context: &PluginContext) -> Option<SymtabInfo> {
    if file_data.len() < 32 || read_u32_le(file_data, 0)? != MH_MAGIC_64 {
        return None;
    }

    let ncmds = usize::try_from(read_u32_le(file_data, 16)?).ok()?;
    let sizeofcmds = usize::try_from(read_u32_le(file_data, 20)?).ok()?;
    let load_commands_end = 32_usize.checked_add(sizeofcmds)?;
    if ncmds > MAX_LOAD_COMMANDS
        || ncmds.checked_mul(8)? > sizeofcmds
        || sizeofcmds > 1024 * 1024
        || load_commands_end > file_data.len()
    {
        return None;
    }

    let mut offset = 32_usize;
    for _ in 0..ncmds {
        context.checkpoint().ok()?;
        if offset.checked_add(8)? > load_commands_end {
            return None;
        }
        let cmd = read_u32_le(file_data, offset)?;
        let cmdsize = usize::try_from(read_u32_le(file_data, offset + 4)?).ok()?;
        let next_offset = offset.checked_add(cmdsize)?;
        if cmdsize < 8 || next_offset > load_commands_end {
            return None;
        }

        if cmd == LC_SYMTAB && cmdsize >= 24 {
            return Some(SymtabInfo {
                sym_offset: read_u32_le(file_data, offset + 8)?,
                nsyms: read_u32_le(file_data, offset + 12)?,
                str_offset: read_u32_le(file_data, offset + 16)?,
                str_size: read_u32_le(file_data, offset + 20)?,
            });
        }
        offset = next_offset;
    }
    None
}

/// Read the symbol table from a Mach-O file on disk and resolve the given addresses.
fn resolve_image_symbols_from_disk(
    img: &RawImageData,
    addresses: &[u64],
    context: &PluginContext,
) -> Option<Vec<(u64, String)>> {
    context.checkpoint().ok()?;
    let file_data = read_regular_file_bounded(&img.path, context)?;
    let symtab = find_symtab_info(&file_data, context)?;

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
    let mut total_symbol_name_bytes = 0_usize;
    for i in 0..symtab.nsyms as usize {
        context.checkpoint().ok()?;
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

        let name_bytes = &strtab[n_strx..];
        let search_len = name_bytes.len().min(MAX_SYMBOL_NAME_BYTES + 1);
        let name_len = name_bytes[..search_len].iter().position(|&b| b == 0)?;
        if name_len > MAX_SYMBOL_NAME_BYTES {
            continue;
        }
        let name_end = n_strx.checked_add(name_len)?;
        let raw_name = String::from_utf8_lossy(&strtab[n_strx..name_end]);

        // Strip leading underscore (C symbol convention on macOS)
        let name = if let Some(stripped) = raw_name.strip_prefix('_') {
            stripped.to_string()
        } else {
            raw_name.into_owned()
        };

        total_symbol_name_bytes = total_symbol_name_bytes.checked_add(name.len())?;
        if total_symbol_name_bytes > MAX_TOTAL_SYMBOL_NAME_BYTES
            || func_syms.len() >= MAX_FUNCTION_SYMBOLS
        {
            return None;
        }

        func_syms.push(NlistEntry {
            address: n_value.wrapping_add(slide),
            name,
        });
    }

    func_syms.sort_by_key(|s| s.address);

    let mut result = Vec::new();
    for &addr in addresses {
        context.checkpoint().ok()?;
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

#[cfg(test)]
#[path = "../../tests/unit/preprocessors/symbolicate_tests.rs"]
mod tests;
