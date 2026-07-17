use super::*;
use std::os::raw::c_char;

/// Build a `Vec<c_char>` from bytes (shm char arrays are `c_char`/i8).
fn cc(bytes: &[u8]) -> Vec<c_char> {
    bytes.iter().map(|&b| b as c_char).collect()
}

/// Copy bytes into a fixed `c_char` slice (leaves the rest untouched).
fn set(dst: &mut [c_char], s: &[u8]) {
    for (d, &b) in dst.iter_mut().zip(s) {
        *d = b as c_char;
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
fn test_read_tags_normal() {
    let mut tags = [[[0 as c_char; 64]; 2]; 4];
    set(&mut tags[0][0], b"key");
    set(&mut tags[0][1], b"val");
    set(&mut tags[1][0], b"k2");
    set(&mut tags[1][1], b"v2");
    let result = read_tags(&tags, 2);
    assert_eq!(result.len(), 2);
    assert_eq!(result[0], ("key".to_string(), "val".to_string()));
    assert_eq!(result[1], ("k2".to_string(), "v2".to_string()));
}

#[test]
fn test_read_tags_clamps_negative() {
    let tags = [[[0 as c_char; 64]; 2]; 4];
    assert!(read_tags(&tags, -1).is_empty());
}

#[test]
fn test_read_tags_clamps_above_four() {
    let mut tags = [[[0 as c_char; 64]; 2]; 4];
    let keys = [b'a', b'b', b'c', b'd'];
    let vals = [b'A', b'B', b'C', b'D'];
    for (i, (&k, &v)) in keys.iter().zip(vals.iter()).enumerate() {
        tags[i][0][0] = k as c_char;
        tags[i][1][0] = v as c_char;
    }
    let result = read_tags(&tags, 10);
    assert_eq!(result.len(), 4);
}
