use super::*;

#[test]
fn test_hexdump_single_line() {
    let data = b"Hello World!\x00\x01\x02\x03";
    let mut out = String::new();
    format_hexdump(data, &mut out);
    assert!(out.contains("48 65 6c 6c 6f 20 57 6f"));
    assert!(out.contains("|Hello World!....|"));
}

#[test]
fn test_hexdump_partial_line() {
    let data = b"ABC";
    let mut out = String::new();
    format_hexdump(data, &mut out);
    assert!(out.contains("41 42 43"));
    assert!(out.contains("|ABC|"));
    // Should have proper padding
    assert!(out.contains("00000000"));
}

#[test]
fn test_hexdump_empty() {
    let data: &[u8] = &[];
    let mut out = String::new();
    format_hexdump(data, &mut out);
    assert!(out.is_empty());
}

#[test]
fn test_hexdump_ascii_sidebar_printable_range() {
    // All printable ASCII
    let data: Vec<u8> = (0x20..=0x7E).collect();
    let mut out = String::new();
    format_hexdump(&data, &mut out);
    // First line should contain ' ' through '/'
    assert!(out.contains("| !\"#$%&'()*+,-./|"));
}

#[test]
fn test_hexdump_non_printable_as_dot() {
    let data = [0x00u8, 0x01, 0x7F, 0xFF, 0x41]; // A at the end
    let mut out = String::new();
    format_hexdump(&data, &mut out);
    assert!(out.contains("|....A|"));
}

#[test]
fn test_hexdump_multi_line_offset() {
    let data = [0u8; 48]; // 3 lines
    let mut out = String::new();
    format_hexdump(&data, &mut out);
    assert!(out.contains("00000000"));
    assert!(out.contains("00000010"));
    assert!(out.contains("00000020"));
}
