use super::*;

#[test]
fn test_extract_thread_name_normal() {
    let mut buf = [0u8; 64];
    buf[..4].copy_from_slice(b"main");
    assert_eq!(extract_thread_name(&buf), Some("main".to_string()));
}

#[test]
fn test_extract_thread_name_empty() {
    let buf = [0u8; 64];
    assert_eq!(extract_thread_name(&buf), None);
}

#[test]
fn test_extract_thread_name_no_null() {
    // 64 non-zero ASCII bytes, no null terminator
    let buf = [b'A'; 64];
    let expected: String = "A".repeat(64);
    assert_eq!(extract_thread_name(&buf), Some(expected));
}

#[test]
fn test_extract_thread_name_invalid_utf8() {
    let mut buf = [0xFFu8; 64];
    // No null terminator, all 0xFF — invalid UTF-8
    buf[63] = 0xFF; // ensure no accidental null
    assert_eq!(extract_thread_name(&buf), None);
}
