use super::*;
use mach2::kern_return::KERN_INVALID_ADDRESS;

#[test]
fn test_vm_enum_truncated_at_max() {
    // At exactly MAX_VM_REGIONS, should truncate regardless of query result
    let result = vm_enum_action(MAX_VM_REGIONS, Ok((0x1000, 0x5000)));
    assert!(matches!(result, VmEnumAction::Truncated));

    // Above MAX_VM_REGIONS too
    let result = vm_enum_action(MAX_VM_REGIONS + 1, Ok((0x1000, 0x5000)));
    assert!(matches!(result, VmEnumAction::Truncated));
}

#[test]
fn test_vm_enum_add_region() {
    let result = vm_enum_action(0, Ok((0x2000, 0x1000)));
    match result {
        VmEnumAction::AddRegion { next_address } => {
            assert_eq!(next_address, 0x1000 + 0x2000);
        }
        _ => panic!("expected AddRegion"),
    }
}

#[test]
fn test_vm_enum_add_region_saturating() {
    // Verify saturating_add doesn't overflow
    let result = vm_enum_action(0, Ok((1, u64::MAX)));
    match result {
        VmEnumAction::AddRegion { next_address } => {
            assert_eq!(next_address, u64::MAX);
        }
        _ => panic!("expected AddRegion"),
    }
}

#[test]
fn test_vm_enum_done_on_invalid_address() {
    let result = vm_enum_action(0, Err((KERN_INVALID_ADDRESS, 0x5000)));
    assert!(matches!(result, VmEnumAction::Done));
}

#[test]
fn test_vm_enum_skip_on_error() {
    // kern_return = 5 (some other error, not KERN_INVALID_ADDRESS)
    let result = vm_enum_action(0, Err((5, 0x8000)));
    match result {
        VmEnumAction::SkipPage { next_address } => {
            assert_eq!(next_address, 0x8000 + 4096);
        }
        _ => panic!("expected SkipPage"),
    }
}
