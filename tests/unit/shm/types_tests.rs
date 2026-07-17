use super::*;
use std::mem::MaybeUninit;
use std::os::raw::c_char;

/// A zeroed annotation array of the schema-defined length.
fn zeroed_annotations() -> [SutCrashAnnotation; MAX_ANNOTATIONS] {
    // SAFETY: SutCrashAnnotation is repr(C) plain char arrays; all-zero is valid.
    unsafe { MaybeUninit::zeroed().assume_init() }
}

/// Build a `Vec<c_char>` from bytes (shm char arrays are `c_char`/i8).
fn cc(bytes: &[u8]) -> Vec<c_char> {
    bytes.iter().map(|&b| b.cast_signed()).collect()
}

/// Copy bytes into a fixed `c_char` slice (leaves the rest untouched).
fn set(dst: &mut [c_char], s: &[u8]) {
    for (d, &b) in dst.iter_mut().zip(s) {
        *d = b.cast_signed();
    }
}

#[test]
fn test_c_array_to_string_with_nul() {
    assert_eq!(c_array_to_string(&cc(&[b'h', b'i', 0, b'X'])), "hi");
}

#[test]
fn test_c_array_to_string_no_nul() {
    assert_eq!(c_array_to_string(&cc(b"ABC")), "ABC");
}

#[test]
fn test_c_array_to_string_empty() {
    assert_eq!(c_array_to_string(&cc(&[0, b'X'])), "");
}

#[test]
fn test_c_array_to_string_all_zeros() {
    assert_eq!(c_array_to_string(&cc(&[0, 0, 0])), "");
}

#[test]
fn test_read_annotations_normal() {
    let mut anns = zeroed_annotations();
    set(&mut anns[0].key, b"key");
    set(&mut anns[0].value, b"val");
    set(&mut anns[1].key, b"k2");
    set(&mut anns[1].value, b"v2");
    let result = read_annotations(&anns, 2);
    assert_eq!(result.len(), 2);
    assert_eq!(result[0], ("key".to_string(), "val".to_string()));
    assert_eq!(result[1], ("k2".to_string(), "v2".to_string()));
}

#[test]
fn test_read_annotations_clamps_negative() {
    let anns = zeroed_annotations();
    assert!(read_annotations(&anns, -1).is_empty());
}

#[test]
fn test_read_annotations_clamps_above_max() {
    let anns = zeroed_annotations();
    let result = read_annotations(&anns, 9999);
    assert_eq!(result.len(), MAX_ANNOTATIONS);
}
