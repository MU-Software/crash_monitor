use super::*;
use crate::pipeline::report::LoadedImageReport;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

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
            uuid: None,
            architecture: None,
            text_start: None,
            text_end: None,
            segments: Vec::new(),
        },
        LoadedImageReport {
            path: "/a".into(),
            base: "0x100000000".into(),
            slide: Some("0x1000".into()),
            uuid: None,
            architecture: None,
            text_start: None,
            text_end: None,
            segments: Vec::new(),
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
        uuid: None,
        architecture: None,
        text_start: None,
        text_end: None,
        segments: Vec::new(),
    }];
    let slides = build_slide_map(&images);
    assert_eq!(slides.len(), 1);
    assert_eq!(slides[0].slide, 0); // defaults to 0
}

fn thin_macho_identity(cpu: u32, uuid: [u8; 16]) -> Vec<u8> {
    let mut bytes = vec![0_u8; 56];
    bytes[0..4].copy_from_slice(&0xfeed_facf_u32.to_le_bytes());
    bytes[4..8].copy_from_slice(&cpu.to_le_bytes());
    bytes[16..20].copy_from_slice(&1_u32.to_le_bytes());
    bytes[20..24].copy_from_slice(&24_u32.to_le_bytes());
    bytes[32..36].copy_from_slice(&0x1b_u32.to_le_bytes());
    bytes[36..40].copy_from_slice(&24_u32.to_le_bytes());
    bytes[40..56].copy_from_slice(&uuid);
    bytes
}

#[test]
fn thin_and_fat_macho_identity_selection_is_bounds_checked() {
    let arm = thin_macho_identity(0x0100_000c, [1; 16]);
    let x86 = thin_macho_identity(0x0100_0007, [2; 16]);
    assert_eq!(
        parse_macho_identities(&arm).unwrap()[0].architecture,
        "arm64"
    );

    let first_offset = 8 + 2 * 20;
    let second_offset = first_offset + arm.len();
    let mut fat = vec![0_u8; second_offset + x86.len()];
    fat[0..4].copy_from_slice(&0xcafe_babe_u32.to_be_bytes());
    fat[4..8].copy_from_slice(&2_u32.to_be_bytes());
    fat[16..20].copy_from_slice(
        &u32::try_from(first_offset)
            .expect("test offset fits u32")
            .to_be_bytes(),
    );
    fat[20..24].copy_from_slice(
        &u32::try_from(arm.len())
            .expect("test image fits u32")
            .to_be_bytes(),
    );
    fat[36..40].copy_from_slice(
        &u32::try_from(second_offset)
            .expect("test offset fits u32")
            .to_be_bytes(),
    );
    fat[40..44].copy_from_slice(
        &u32::try_from(x86.len())
            .expect("test image fits u32")
            .to_be_bytes(),
    );
    fat[first_offset..second_offset].copy_from_slice(&arm);
    fat[second_offset..].copy_from_slice(&x86);
    let identities = parse_macho_identities(&fat).unwrap();
    assert_eq!(identities.len(), 2);
    assert_eq!(identities[0].architecture, "arm64");
    assert_eq!(identities[1].architecture, "x86_64");

    fat[40..44].copy_from_slice(&u32::MAX.to_be_bytes());
    assert!(parse_macho_identities(&fat).is_none());
}

#[test]
fn image_address_matching_uses_exact_text_range() {
    let image = LoadedImageReport {
        path: "/bin/app".into(),
        base: "0x1000".into(),
        slide: Some("0x100".into()),
        uuid: Some("id".into()),
        architecture: Some("arm64".into()),
        text_start: Some("0x1000".into()),
        text_end: Some("0x2000".into()),
        segments: Vec::new(),
    };
    assert!(image_contains(&image, 0x1000));
    assert!(image_contains(&image, 0x1fff));
    assert!(!image_contains(&image, 0x2000));
}

#[cfg(target_os = "macos")]
#[test]
fn private_output_is_0600_without_changing_parent_mode() {
    let root = tempfile::tempdir().unwrap();
    let output_dir = root.path().join("output");
    std::fs::create_dir(&output_dir).unwrap();
    std::fs::set_permissions(&output_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let parent_inode = std::fs::metadata(&output_dir).unwrap().ino();
    let output = output_dir.join("symbolicated.json");

    write_private_output(&output, b"private").unwrap();

    assert_eq!(std::fs::read(&output).unwrap(), b"private");
    assert_eq!(
        std::fs::metadata(&output).unwrap().permissions().mode() & 0o7777,
        crate::utils::paths::PRIVATE_FILE_MODE
    );
    assert_eq!(
        std::fs::metadata(&output_dir).unwrap().permissions().mode() & 0o7777,
        0o755
    );
    assert_eq!(std::fs::metadata(&output_dir).unwrap().ino(), parent_inode);
}

#[test]
fn private_output_validates_replacement_and_rejects_symlink() {
    let root = tempfile::tempdir().unwrap();
    let output_dir = root.path().join("output");
    std::fs::create_dir(&output_dir).unwrap();
    std::fs::set_permissions(&output_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let output = output_dir.join("symbolicated.json");
    std::fs::write(&output, b"old").unwrap();
    std::fs::set_permissions(&output, std::fs::Permissions::from_mode(0o644)).unwrap();

    write_private_output(&output, b"new").unwrap();
    assert_eq!(std::fs::read(&output).unwrap(), b"new");
    assert_eq!(
        std::fs::metadata(&output).unwrap().permissions().mode() & 0o7777,
        crate::utils::paths::PRIVATE_FILE_MODE
    );

    std::fs::remove_file(&output).unwrap();
    let outside = output_dir.join("outside.json");
    std::fs::write(&outside, b"outside").unwrap();
    symlink(&outside, &output).unwrap();
    let error = write_private_output(&output, b"blocked").unwrap_err();
    assert!(error.contains("safely open existing output"));
    assert_eq!(std::fs::read(&outside).unwrap(), b"outside");
    assert!(
        std::fs::read_dir(&output_dir)
            .unwrap()
            .flatten()
            .all(|entry| !entry
                .file_name()
                .to_string_lossy()
                .contains(".symbolicate-"))
    );
}

#[test]
fn replacement_in_other_writable_directory_fails_closed() {
    let root = tempfile::tempdir().unwrap();
    let output_dir = root.path().join("shared");
    std::fs::create_dir(&output_dir).unwrap();
    std::fs::set_permissions(&output_dir, std::fs::Permissions::from_mode(0o777)).unwrap();
    let output = output_dir.join("symbolicated.json");
    std::fs::write(&output, b"old").unwrap();

    let error = write_private_output(&output, b"new").unwrap_err();

    assert!(error.contains("unsafe mode"), "{error}");
    assert_eq!(std::fs::read(&output).unwrap(), b"old");
}

#[test]
fn new_output_in_other_writable_directory_fails_closed() {
    let root = tempfile::tempdir().unwrap();
    let output_dir = root.path().join("shared");
    std::fs::create_dir(&output_dir).unwrap();
    std::fs::set_permissions(&output_dir, std::fs::Permissions::from_mode(0o777)).unwrap();
    let output = output_dir.join("symbolicated.json");

    let error = write_private_output(&output, b"new").unwrap_err();

    assert!(error.contains("unsafe mode"), "{error}");
    assert!(!output.exists());
}

#[test]
fn output_parent_symlink_is_rejected_without_writing_target() {
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("target");
    let linked = root.path().join("linked");
    std::fs::create_dir(&target).unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
    symlink(&target, &linked).unwrap();

    let error = write_private_output(&linked.join("symbolicated.json"), b"new").unwrap_err();

    assert!(error.contains("safely open"), "{error}");
    assert!(!target.join("symbolicated.json").exists());
    assert!(
        std::fs::symlink_metadata(linked)
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[cfg(target_os = "macos")]
#[test]
fn output_parent_allow_acl_is_rejected() {
    let root = tempfile::tempdir().unwrap();
    let output_dir = root.path().join("acl-output");
    std::fs::create_dir(&output_dir).unwrap();
    std::fs::set_permissions(&output_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let status = std::process::Command::new("/bin/chmod")
        .args(["+a", "everyone allow read"])
        .arg(&output_dir)
        .status()
        .unwrap();
    assert!(status.success());

    let output = output_dir.join("symbolicated.json");
    let error = write_private_output(&output, b"new").unwrap_err();

    assert!(error.contains("grants access"), "{error}");
    assert!(!output.exists());
    let _ = std::process::Command::new("/bin/chmod")
        .arg("-N")
        .arg(output_dir)
        .status();
}
