use super::*;

fn make_image(path: &str, base: u64) -> RawImageData {
    RawImageData {
        path: path.to_string(),
        base_address: base,
        slide: None,
    }
}

#[test]
fn test_resolve_address_found() {
    let images = vec![
        make_image("/path/to/img1", 0x1000),
        make_image("/path/to/img2", 0x2000),
    ];
    let (name, offset) = resolve_address(&images, 0x1500);
    assert_eq!(name.as_deref(), Some("img1"));
    assert_eq!(offset, Some(0x500));
}

#[test]
fn test_resolve_address_before_first() {
    let images = vec![
        make_image("/path/to/img1", 0x1000),
        make_image("/path/to/img2", 0x2000),
    ];
    let (name, offset) = resolve_address(&images, 0x500);
    assert_eq!(name, None);
    assert_eq!(offset, None);
}

#[test]
fn test_resolve_address_between_images() {
    let images = vec![
        make_image("/path/to/img1", 0x1000),
        make_image("/path/to/img2", 0x3000),
    ];
    // Address 0x2500 is between img1 (0x1000) and img2 (0x3000) → resolves to img1
    let (name, offset) = resolve_address(&images, 0x2500);
    assert_eq!(name.as_deref(), Some("img1"));
    assert_eq!(offset, Some(0x1500));
}

#[test]
fn test_resolve_address_empty_list() {
    let images: Vec<RawImageData> = vec![];
    let (name, offset) = resolve_address(&images, 0x1000);
    assert_eq!(name, None);
    assert_eq!(offset, None);
}
