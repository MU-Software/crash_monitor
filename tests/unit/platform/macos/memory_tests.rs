use super::*;
use mach2::kern_return::KERN_INVALID_ADDRESS;

const QUERY_ERROR: i32 = 5;

#[test]
fn test_vm_enum_done_on_invalid_address() {
    let mut state = VmEnumerationState::new(16 * 1024);
    assert!(matches!(
        state.process_query(Err(KERN_INVALID_ADDRESS)),
        VmEnumAction::Done(VmRegionEnumerationQuality::Complete)
    ));
    assert_eq!(state.query_attempts(), 1);
}

#[test]
fn test_vm_enum_recovered_query_errors_still_report_partial_quality() {
    let mut state = VmEnumerationState::new(16 * 1024);
    assert!(matches!(
        state.process_query(Err(QUERY_ERROR)),
        VmEnumAction::SkipPage
    ));
    assert!(matches!(
        state.process_query(Ok((16 * 1024, state.address()))),
        VmEnumAction::AddRegion
    ));
    assert!(matches!(
        state.process_query(Err(KERN_INVALID_ADDRESS)),
        VmEnumAction::Done(VmRegionEnumerationQuality::SkippedQueryErrors { count: 1 })
    ));
}

#[test]
fn test_vm_enum_error_skips_actual_host_page_size() {
    let mut state = VmEnumerationState::new(16 * 1024);
    assert!(matches!(
        state.process_query(Err(QUERY_ERROR)),
        VmEnumAction::SkipPage
    ));
    assert_eq!(state.address(), 16_384);
}

#[test]
fn test_vm_enum_repeated_errors_stop_at_consecutive_cap() {
    let mut state = VmEnumerationState::new(16 * 1024);
    for _ in 1..MAX_CONSECUTIVE_VM_REGION_FAILURES {
        assert!(matches!(
            state.process_query(Err(QUERY_ERROR)),
            VmEnumAction::SkipPage
        ));
    }
    assert!(matches!(
        state.process_query(Err(QUERY_ERROR)),
        VmEnumAction::Stop(VmRegionEnumerationQuality::ConsecutiveErrorLimit)
    ));
    assert_eq!(state.query_attempts(), MAX_CONSECUTIVE_VM_REGION_FAILURES);
    assert_eq!(state.region_count(), 0);
}

#[test]
fn test_vm_enum_total_attempt_cap_is_independent_of_success_count() {
    let mut state = VmEnumerationState::new(1);
    while state.pre_query_stop().is_none() {
        let action = if state.query_attempts() % MAX_CONSECUTIVE_VM_REGION_FAILURES
            == MAX_CONSECUTIVE_VM_REGION_FAILURES - 1
        {
            state.process_query(Ok((1, state.address())))
        } else {
            state.process_query(Err(QUERY_ERROR))
        };
        assert!(!matches!(action, VmEnumAction::Stop(_)));
    }

    assert_eq!(state.query_attempts(), MAX_VM_REGION_QUERY_ATTEMPTS);
    assert!(state.region_count() < MAX_VM_REGIONS);
    assert_eq!(
        state.pre_query_stop(),
        Some(VmRegionEnumerationQuality::QueryAttemptLimit)
    );
}

#[test]
fn test_vm_enum_rejects_no_progress_but_accepts_overlapping_region_recovery() {
    let mut no_progress = VmEnumerationState::new(16 * 1024);
    assert!(matches!(
        no_progress.process_query(Ok((0, 0))),
        VmEnumAction::Stop(VmRegionEnumerationQuality::NoAddressProgress)
    ));

    // After a transient query error skips one page, mach_vm_region may return
    // the containing region whose start precedes the cursor. Its end still
    // advances the cursor, so this is valid recovery rather than regression.
    let mut overlapping = VmEnumerationState::new(16 * 1024);
    assert!(matches!(
        overlapping.process_query(Err(QUERY_ERROR)),
        VmEnumAction::SkipPage
    ));
    assert_eq!(overlapping.address(), 16 * 1024);
    assert!(matches!(
        overlapping.process_query(Ok((32 * 1024, 0))),
        VmEnumAction::AddRegion
    ));
    assert_eq!(overlapping.address(), 32 * 1024);

    // An overlapping result whose end does not advance remains invalid.
    assert!(matches!(
        overlapping.process_query(Ok((16 * 1024, 0))),
        VmEnumAction::Stop(VmRegionEnumerationQuality::NoAddressProgress)
    ));
}

#[test]
fn test_vm_enum_address_overflow_is_terminal_after_retaining_region() {
    let mut state = VmEnumerationState::new(16 * 1024);
    assert!(matches!(
        state.process_query(Ok((u64::MAX - 5, 0))),
        VmEnumAction::AddRegion
    ));
    assert!(matches!(
        state.process_query(Ok((10, u64::MAX - 5))),
        VmEnumAction::AddTerminalRegion(VmRegionEnumerationQuality::AddressOverflow)
    ));
    assert_eq!(state.address(), u64::MAX);
    assert_eq!(state.region_count(), 2);
}

#[test]
fn test_vm_enum_exact_terminal_address_does_not_issue_another_query() {
    let mut state = VmEnumerationState::new(16 * 1024);
    assert!(matches!(
        state.process_query(Ok((u64::MAX, 0))),
        VmEnumAction::AddTerminalRegion(VmRegionEnumerationQuality::AddressOverflow)
    ));
    assert_eq!(state.address(), u64::MAX);
    assert_eq!(state.query_attempts(), 1);
    assert_eq!(state.region_count(), 1);
}

#[test]
fn test_vm_enum_failed_query_overflow_stops_without_saturating_loop() {
    let mut state = VmEnumerationState::new(16 * 1024);
    assert!(matches!(
        state.process_query(Ok((u64::MAX - 5, 0))),
        VmEnumAction::AddRegion
    ));
    assert!(matches!(
        state.process_query(Err(QUERY_ERROR)),
        VmEnumAction::Stop(VmRegionEnumerationQuality::AddressOverflow)
    ));
    assert_eq!(state.address(), u64::MAX);
}

#[test]
fn test_vm_enum_huge_map_stops_without_an_extra_kernel_attempt() {
    let mut state = VmEnumerationState::new(16 * 1024);
    for _ in 0..MAX_VM_REGIONS {
        assert!(state.pre_query_stop().is_none());
        assert!(matches!(
            state.process_query(Ok((1, state.address()))),
            VmEnumAction::AddRegion
        ));
    }

    assert_eq!(state.region_count(), MAX_VM_REGIONS);
    assert_eq!(state.query_attempts(), MAX_VM_REGIONS);
    assert_eq!(
        state.pre_query_stop(),
        Some(VmRegionEnumerationQuality::RegionLimit)
    );
}

#[test]
fn test_vm_enum_quality_distinguishes_complete_from_partial() {
    assert!(VmRegionEnumerationQuality::Complete.is_complete());
    for partial in [
        VmRegionEnumerationQuality::RegionLimit,
        VmRegionEnumerationQuality::QueryAttemptLimit,
        VmRegionEnumerationQuality::ConsecutiveErrorLimit,
        VmRegionEnumerationQuality::SkippedQueryErrors { count: 1 },
        VmRegionEnumerationQuality::NoAddressProgress,
        VmRegionEnumerationQuality::AddressOverflow,
        VmRegionEnumerationQuality::CaptureDeadline,
    ] {
        assert!(!partial.is_complete());
        assert_ne!(partial.to_string(), "complete");
    }
}
