use super::*;
use crate::platform::mock::MockPlatform;

fn make_image(path: &str, base: u64) -> RawImageData {
    RawImageData {
        path: path.to_string(),
        base_address: base,
        slide: None,
        uuid: None,
        architecture: None,
        text_start: Some(base),
        text_end: base.checked_add(0x1000),
        segments: vec![RawImageSegment {
            name: "__TEXT".into(),
            start: base,
            end: base + 0x1000,
        }],
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
fn test_resolve_address_rejects_gap_between_image_ranges() {
    let images = vec![
        make_image("/path/to/img1", 0x1000),
        make_image("/path/to/img2", 0x3000),
    ];
    // Address 0x2500 is outside both actual __TEXT ranges.
    let (name, offset) = resolve_address(&images, 0x2500);
    assert_eq!(name, None);
    assert_eq!(offset, None);
}

#[test]
fn test_resolve_address_empty_list() {
    let images: Vec<RawImageData> = vec![];
    let (name, offset) = resolve_address(&images, 0x1000);
    assert_eq!(name, None);
    assert_eq!(offset, None);
}

#[test]
fn macho_metadata_collects_uuid_architecture_slide_and_text_range() {
    let mut platform = crate::platform::mock::MockPlatform::default();
    let base = 0x1_0000;
    let mut header = vec![0_u8; 32];
    header[0..4].copy_from_slice(&MH_MAGIC_64.to_le_bytes());
    header[4..8].copy_from_slice(&0x0100_000c_u32.to_le_bytes());
    header[16..20].copy_from_slice(&3_u32.to_le_bytes());
    header[20..24].copy_from_slice(&168_u32.to_le_bytes());
    platform.memory.insert(base, header);

    let mut commands = vec![0_u8; 168];
    commands[0..4].copy_from_slice(&LC_SEGMENT_64.to_le_bytes());
    commands[4..8].copy_from_slice(&72_u32.to_le_bytes());
    commands[8..14].copy_from_slice(b"__TEXT");
    commands[24..32].copy_from_slice(&0x4_000_u64.to_le_bytes());
    commands[32..40].copy_from_slice(&0x3_000_u64.to_le_bytes());
    commands[72..76].copy_from_slice(&LC_SEGMENT_64.to_le_bytes());
    commands[76..80].copy_from_slice(&72_u32.to_le_bytes());
    commands[80..86].copy_from_slice(b"__DATA");
    commands[96..104].copy_from_slice(&0x7_000_u64.to_le_bytes());
    commands[104..112].copy_from_slice(&0x1_000_u64.to_le_bytes());
    commands[144..148].copy_from_slice(&LC_UUID.to_le_bytes());
    commands[148..152].copy_from_slice(&24_u32.to_le_bytes());
    commands[152..168].copy_from_slice(&[0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15]);
    platform.memory.insert(base + 32, commands);

    let mut image = make_image("/bin/app", base);
    compute_image_metadata(&platform, 0, &mut image, &PluginContext::without_deadline());
    assert_eq!(image.architecture.as_deref(), Some("arm64"));
    assert_eq!(image.slide, Some(0xc_000));
    assert_eq!(image.text_start, Some(base));
    assert_eq!(image.text_end, Some(base + 0x3_000));
    assert_eq!(
        image.segments,
        vec![
            RawImageSegment {
                name: "__TEXT".into(),
                start: base,
                end: base + 0x3_000,
            },
            RawImageSegment {
                name: "__DATA".into(),
                start: base + 0x3_000,
                end: base + 0x4_000,
            },
        ]
    );
    assert_eq!(
        image.uuid.as_deref(),
        Some("00010203-0405-0607-0809-0a0b0c0d0e0f")
    );
}

#[test]
fn macho_metadata_preserves_complete_commands_from_partial_vm_read() {
    let mut platform = crate::platform::mock::MockPlatform::default();
    let base = 0x2_0000;
    let mut header = vec![0_u8; 32];
    header[0..4].copy_from_slice(&MH_MAGIC_64.to_le_bytes());
    header[16..20].copy_from_slice(&2_u32.to_le_bytes());
    header[20..24].copy_from_slice(&144_u32.to_le_bytes());
    platform.memory.insert(base, header);

    let mut partial_commands = vec![0_u8; 80];
    partial_commands[0..4].copy_from_slice(&LC_SEGMENT_64.to_le_bytes());
    partial_commands[4..8].copy_from_slice(&72_u32.to_le_bytes());
    partial_commands[8..14].copy_from_slice(b"__TEXT");
    partial_commands[24..32].copy_from_slice(&0x8_000_u64.to_le_bytes());
    partial_commands[32..40].copy_from_slice(&0x2_000_u64.to_le_bytes());
    partial_commands[72..76].copy_from_slice(&LC_SEGMENT_64.to_le_bytes());
    partial_commands[76..80].copy_from_slice(&72_u32.to_le_bytes());
    platform.memory.insert(base + 32, partial_commands);

    let mut image = make_image("/bin/partial", base);
    image.text_start = None;
    image.text_end = None;
    image.segments.clear();
    compute_image_metadata(&platform, 0, &mut image, &PluginContext::without_deadline());

    assert_eq!(image.slide, Some(base - 0x8_000));
    assert_eq!(image.text_start, Some(base));
    assert_eq!(image.text_end, Some(base + 0x2_000));
    assert_eq!(
        image.segments,
        vec![RawImageSegment {
            name: "__TEXT".into(),
            start: base,
            end: base + 0x2_000,
        }]
    );
}

#[test]
fn c_string_keeps_last_successful_prefix_when_larger_read_fails() {
    let mut platform = crate::platform::mock::MockPlatform::default();
    platform.memory.insert(0x5000, vec![b'a'; 128]);
    let value = read_c_string(
        &platform,
        0,
        0x5000,
        512,
        &PluginContext::without_deadline(),
    )
    .unwrap();
    assert_eq!(value, "a".repeat(128));
}

#[test]
fn test_loaded_image_enumeration_rejects_short_task_info_count() {
    let mut platform = MockPlatform::default();
    platform
        .task_info_responses
        .insert(TASK_DYLD_INFO, vec![0; TASK_DYLD_INFO_WORDS - 1]);

    let error = enumerate_loaded_images(
        &platform,
        0,
        &crate::pipeline::PluginContext::without_deadline(),
    )
    .unwrap_err();
    assert!(error.contains("returned 4 words; expected 5"));
}

#[test]
fn test_c_string_reader_can_accept_useful_partial_read() {
    let mut platform = MockPlatform::default();
    platform.memory.insert(0x1000, b"short\0".to_vec());

    let value = read_c_string(
        &platform,
        0,
        0x1000,
        512,
        &crate::pipeline::PluginContext::without_deadline(),
    )
    .unwrap();
    assert_eq!(value, "short");
}
