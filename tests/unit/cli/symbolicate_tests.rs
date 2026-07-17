use super::*;
use crate::pipeline::report::LoadedImageReport;

#[test]
fn test_parse_hex_address_with_prefix() {
    assert_eq!(parse_hex_address("0x0000000100a3b1a8"), Some(0x1_00a3_b1a8));
    assert_eq!(parse_hex_address("0X1234"), Some(0x1234));
}

#[test]
fn test_parse_hex_address_without_prefix() {
    assert_eq!(parse_hex_address("100a3b1a8"), Some(0x1_00a3_b1a8));
}

#[test]
fn test_parse_hex_address_invalid() {
    assert_eq!(parse_hex_address("not_hex"), None);
    assert_eq!(parse_hex_address(""), None);
}

#[test]
fn test_build_slide_map_empty() {
    let slides = build_slide_map(&[]);
    assert!(slides.is_empty());
}

#[test]
fn test_build_slide_map_sorted() {
    let images = vec![
        LoadedImageReport {
            path: "/b".into(),
            base: "0x200000000".into(),
            slide: Some("0x2000".into()),
        },
        LoadedImageReport {
            path: "/a".into(),
            base: "0x100000000".into(),
            slide: Some("0x1000".into()),
        },
    ];
    let slides = build_slide_map(&images);
    assert_eq!(slides.len(), 2);
    assert!(slides[0].base < slides[1].base);
    assert_eq!(slides[0].slide, 0x1000);
    assert_eq!(slides[1].slide, 0x2000);
}

#[test]
fn test_find_slide_for_address() {
    let slides = vec![
        ImageSlide {
            base: 0x1_0000_0000,
            slide: 0x1000,
        },
        ImageSlide {
            base: 0x2_0000_0000,
            slide: 0x2000,
        },
    ];

    // Address in first image range
    assert_eq!(find_slide_for_address(&slides, 0x1_0000_5000), 0x1000);
    // Address in second image range
    assert_eq!(find_slide_for_address(&slides, 0x2_0000_1000), 0x2000);
    // Address before any image
    assert_eq!(find_slide_for_address(&slides, 0x0000_1000), 0);
}

#[test]
fn test_find_slide_for_address_empty() {
    assert_eq!(find_slide_for_address(&[], 0x1234), 0);
}

#[test]
fn test_find_dwarf_binary_nonexistent() {
    let result = find_dwarf_binary(Path::new("/nonexistent/path.dSYM"));
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("not found"));
}

#[test]
fn test_find_dwarf_binary_invalid_bundle() {
    // A directory that isn't a valid dSYM bundle
    let tmp = std::env::temp_dir().join("test_invalid_dsym");
    let _ = std::fs::create_dir_all(&tmp);
    let result = find_dwarf_binary(&tmp);
    assert!(result.is_err());
    assert!(result.unwrap_err().contains("not a valid dSYM"));
    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn test_find_dwarf_binary_file_path() {
    // If given a file path directly, should return it
    let tmp = std::env::temp_dir().join("test_dwarf_direct");
    std::fs::write(&tmp, b"fake").unwrap();
    let result = find_dwarf_binary(&tmp);
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), tmp);
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn test_build_slide_map_no_slide() {
    let images = vec![LoadedImageReport {
        path: "/a".into(),
        base: "0x100000000".into(),
        slide: None,
    }];
    let slides = build_slide_map(&images);
    assert_eq!(slides.len(), 1);
    assert_eq!(slides[0].slide, 0); // defaults to 0
}
