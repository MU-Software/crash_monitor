use super::*;

use std::io::{Cursor, Read};
use std::os::unix::fs::symlink;

use nix::sys::stat::Mode;
use nix::unistd::mkfifo;

fn raw_thread(backtrace: Vec<u64>) -> RawThreadData {
    RawThreadData {
        thread_port: 0,
        thread_id: 100,
        name: None,
        crashed: false,
        registers: None,
        backtrace,
        stack_capture: None,
    }
}

struct InterruptOnceReader {
    interrupted: bool,
    inner: Cursor<Vec<u8>>,
}

impl Read for InterruptOnceReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if !self.interrupted {
            self.interrupted = true;
            return Err(std::io::ErrorKind::Interrupted.into());
        }
        self.inner.read(buffer)
    }
}

fn write_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn write_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn minimal_macho() -> Vec<u8> {
    const LOAD_COMMAND_OFFSET: usize = 32;
    const SYMTAB_OFFSET: usize = 56;
    const STRING_TABLE_OFFSET: usize = 72;
    let mut bytes = vec![0_u8; STRING_TABLE_OFFSET + 6];

    write_u32(&mut bytes, 0, MH_MAGIC_64);
    write_u32(&mut bytes, 16, 1);
    write_u32(&mut bytes, 20, 24);

    write_u32(&mut bytes, LOAD_COMMAND_OFFSET, LC_SYMTAB);
    write_u32(&mut bytes, LOAD_COMMAND_OFFSET + 4, 24);
    write_u32(
        &mut bytes,
        LOAD_COMMAND_OFFSET + 8,
        u32::try_from(SYMTAB_OFFSET).unwrap(),
    );
    write_u32(&mut bytes, LOAD_COMMAND_OFFSET + 12, 1);
    write_u32(
        &mut bytes,
        LOAD_COMMAND_OFFSET + 16,
        u32::try_from(STRING_TABLE_OFFSET).unwrap(),
    );
    write_u32(&mut bytes, LOAD_COMMAND_OFFSET + 20, 6);

    write_u32(&mut bytes, SYMTAB_OFFSET, 0);
    bytes[SYMTAB_OFFSET + 4] = N_SECT;
    write_u64(&mut bytes, SYMTAB_OFFSET + 8, 0x1000);
    bytes[STRING_TABLE_OFFSET..].copy_from_slice(b"_func\0");
    bytes
}

#[test]
fn bounded_reader_accepts_limit_and_rejects_limit_plus_one() {
    let context = PluginContext::without_deadline();
    let accepted = read_bounded(&mut Cursor::new(vec![1_u8; 8]), 0, 8, &context).unwrap();
    assert_eq!(accepted.len(), 8);

    let rejected = read_bounded(&mut Cursor::new(vec![1_u8; 9]), 0, 8, &context);
    assert!(rejected.is_none());
}

#[test]
fn bounded_reader_retries_eintr() {
    let mut reader = InterruptOnceReader {
        interrupted: false,
        inner: Cursor::new(b"symbols".to_vec()),
    };

    let result = read_bounded(&mut reader, 0, 16, &PluginContext::without_deadline()).unwrap();

    assert_eq!(result, b"symbols");
}

#[test]
fn regular_file_reader_accepts_framework_facade_symlink() {
    let tempdir = tempfile::tempdir().unwrap();
    let versions = tempdir.path().join("Foo.framework/Versions");
    let version = versions.join("A");
    std::fs::create_dir_all(&version).unwrap();
    std::fs::write(version.join("Foo"), b"contents").unwrap();
    symlink("A", versions.join("Current")).unwrap();
    let facade = tempdir.path().join("Foo.framework/Foo");
    symlink("Versions/Current/Foo", &facade).unwrap();

    let result =
        read_regular_file_bounded(facade.to_str().unwrap(), &PluginContext::without_deadline());

    assert_eq!(result.unwrap(), b"contents");
}

#[test]
fn regular_file_reader_rejects_fifo_without_blocking() {
    let tempdir = tempfile::tempdir().unwrap();
    let fifo = tempdir.path().join("symbols.fifo");
    mkfifo(&fifo, Mode::S_IRUSR | Mode::S_IWUSR).unwrap();

    let result =
        read_regular_file_bounded(fifo.to_str().unwrap(), &PluginContext::without_deadline());

    assert!(result.is_none());
}

#[test]
fn address_collection_enforces_thread_frame_and_unique_caps() {
    let threads = vec![
        raw_thread(vec![5, 4, 0]),
        raw_thread(vec![3, 2, 1]),
        raw_thread(vec![99]),
    ];

    let unique_limited =
        collect_symbolication_addresses(&threads, &PluginContext::without_deadline(), 2, 5, 3)
            .unwrap();
    let frame_limited =
        collect_symbolication_addresses(&threads, &PluginContext::without_deadline(), 3, 2, 10)
            .unwrap();
    let thread_limited =
        collect_symbolication_addresses(&threads, &PluginContext::without_deadline(), 1, 10, 10)
            .unwrap();

    assert_eq!(unique_limited, vec![3, 4, 5]);
    assert_eq!(frame_limited, vec![4, 5]);
    assert_eq!(thread_limited, vec![4, 5]);
}

#[test]
fn address_collection_prioritizes_crashed_thread_before_global_caps() {
    let mut crashed = raw_thread(vec![0xCAFE, 0xCAFF]);
    crashed.crashed = true;
    let threads = vec![raw_thread(vec![1, 2, 3]), crashed];

    let addresses =
        collect_symbolication_addresses(&threads, &PluginContext::without_deadline(), 1, 1, 1)
            .unwrap();

    assert_eq!(addresses, vec![0xCAFE]);
}

#[test]
fn resolver_reads_and_parses_regular_file_from_open_descriptor() {
    let tempdir = tempfile::tempdir().unwrap();
    let path = tempdir.path().join("image");
    std::fs::write(&path, minimal_macho()).unwrap();
    let image = RawImageData {
        path: path.to_string_lossy().into_owned(),
        base_address: 0x1000,
        slide: None,
        uuid: None,
        architecture: None,
        text_start: Some(0x1000),
        text_end: Some(0x2000),
        segments: Vec::new(),
    };

    let symbols =
        resolve_image_symbols_from_disk(&image, &[0x1004], &PluginContext::without_deadline())
            .unwrap();

    assert_eq!(symbols, vec![(0x1004, "func".to_string())]);
}

#[test]
fn symbol_lookup_enforces_exact_and_one_mib_boundaries() {
    let symbols = vec![NlistEntry {
        address: 0x1000,
        name: "boundary".to_string(),
    }];

    assert_eq!(find_symbol(&symbols, 0x1000).as_deref(), Some("boundary"));
    assert_eq!(
        find_symbol(&symbols, 0x1000 + 1024 * 1024).as_deref(),
        Some("boundary")
    );
    assert_eq!(find_symbol(&symbols, 0x1000 + 1024 * 1024 + 1), None);
    assert_eq!(find_symbol(&[], 0x1000), None);
    assert_eq!(find_symbol(&symbols, 0x0fff), None);
}

#[test]
fn parser_strips_one_leading_underscore_and_rejects_truncated_tables() {
    let tempdir = tempfile::tempdir().unwrap();
    let path = tempdir.path().join("image");
    std::fs::write(&path, minimal_macho()).unwrap();
    let image = RawImageData {
        path: path.to_string_lossy().into_owned(),
        base_address: 0x1000,
        slide: None,
        uuid: None,
        architecture: None,
        text_start: Some(0x1000),
        text_end: Some(0x2000),
        segments: Vec::new(),
    };
    assert_eq!(
        resolve_image_symbols_from_disk(&image, &[0x1000], &PluginContext::without_deadline())
            .unwrap(),
        vec![(0x1000, "func".to_string())]
    );

    let mut truncated_commands = minimal_macho();
    truncated_commands.truncate(40);
    assert!(find_symtab_info(&truncated_commands, &PluginContext::without_deadline()).is_none());

    let mut invalid_string_range = minimal_macho();
    write_u32(&mut invalid_string_range, 32 + 16, u32::MAX);
    std::fs::write(&path, invalid_string_range).unwrap();
    assert!(
        resolve_image_symbols_from_disk(&image, &[0x1000], &PluginContext::without_deadline())
            .is_none()
    );
}

#[test]
fn arbitrary_macho_bytes_never_panic() {
    let context = PluginContext::without_deadline();
    let mut state = 0xa5a5_5a5a_u32;
    for len in 0..1024 {
        let mut bytes = vec![0_u8; len];
        for byte in &mut bytes {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            *byte = (state >> 16) as u8;
        }
        let _ = find_symtab_info(&bytes, &context);
    }
}
