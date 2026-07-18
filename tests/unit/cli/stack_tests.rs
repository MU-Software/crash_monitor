use super::*;

#[test]
fn test_hexdump_single_line() {
    let data = b"Hello World!\x00\x01\x02\x03";
    let mut bytes = Vec::new();
    format_hexdump(data, &mut bytes).unwrap();
    let out = String::from_utf8(bytes).unwrap();
    assert!(out.contains("48 65 6c 6c 6f 20 57 6f"));
    assert!(out.contains("|Hello World!....|"));
}

#[test]
fn test_hexdump_partial_line() {
    let data = b"ABC";
    let mut bytes = Vec::new();
    format_hexdump(data, &mut bytes).unwrap();
    let out = String::from_utf8(bytes).unwrap();
    assert!(out.contains("41 42 43"));
    assert!(out.contains("|ABC|"));
    // Should have proper padding
    assert!(out.contains("00000000"));
}

#[test]
fn test_hexdump_empty() {
    let data: &[u8] = &[];
    let mut out = Vec::new();
    format_hexdump(data, &mut out).unwrap();
    assert!(out.is_empty());
}

#[test]
fn test_hexdump_ascii_sidebar_printable_range() {
    // All printable ASCII
    let data: Vec<u8> = (0x20..=0x7E).collect();
    let mut bytes = Vec::new();
    format_hexdump(&data, &mut bytes).unwrap();
    let out = String::from_utf8(bytes).unwrap();
    // First line should contain ' ' through '/'
    assert!(out.contains("| !\"#$%&'()*+,-./|"));
}

#[test]
fn test_hexdump_non_printable_as_dot() {
    let data = [0x00u8, 0x01, 0x7F, 0xFF, 0x41]; // A at the end
    let mut bytes = Vec::new();
    format_hexdump(&data, &mut bytes).unwrap();
    let out = String::from_utf8(bytes).unwrap();
    assert!(out.contains("|....A|"));
}

#[test]
fn test_hexdump_multi_line_offset() {
    let data = [0u8; 48]; // 3 lines
    let mut bytes = Vec::new();
    format_hexdump(&data, &mut bytes).unwrap();
    let out = String::from_utf8(bytes).unwrap();
    assert!(out.contains("00000000"));
    assert!(out.contains("00000010"));
    assert!(out.contains("00000020"));
}

#[test]
fn no_threads_has_a_dedicated_error() {
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(
        file.path(),
        serde_json::json!({
            "header": {
                "version": 1,
                "timestamp": "2026-04-05T12:00:00+09:00",
                "pid": 1,
                "process": "test",
                "collector": "crash_monitor",
                "type": "snapshot"
            },
            "threads": []
        })
        .to_string(),
    )
    .unwrap();
    let mut output = Vec::new();
    let mut errors = Vec::new();

    assert_eq!(
        run_with_writers(file.path().to_str().unwrap(), 0, &mut output, &mut errors),
        1
    );
    assert_eq!(
        String::from_utf8(errors).unwrap(),
        "error: report contains no threads\n"
    );
}

#[test]
fn stack_header_uses_decoded_length_and_warns_on_mismatch() {
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(
        file.path(),
        serde_json::json!({
            "header": {
                "version": 1,
                "timestamp": "2026-04-05T12:00:00+09:00",
                "pid": 1,
                "process": "test",
                "collector": "crash_monitor",
                "type": "snapshot"
            },
            "threads": [{
                "index": 0,
                "id": 1,
                "crashed": false,
                "registers": {},
                "backtrace": [],
                "stack_memory": {
                    "sp": "0x1000",
                    "size": 999,
                    "hex_dump": base64::engine::general_purpose::STANDARD.encode(b"abc")
                }
            }]
        })
        .to_string(),
    )
    .unwrap();
    let mut output = Vec::new();

    assert_eq!(
        run_with_writers(
            file.path().to_str().unwrap(),
            0,
            &mut output,
            &mut Vec::new()
        ),
        0
    );
    let rendered = String::from_utf8(output).unwrap();
    assert!(rendered.contains("3 decoded bytes"), "{rendered}");
    assert!(rendered.contains("declared stack size 999"), "{rendered}");
}
