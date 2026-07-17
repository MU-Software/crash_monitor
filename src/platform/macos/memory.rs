//! Pure logic for VM memory region enumeration.

use mach2::kern_return::KERN_INVALID_ADDRESS;

/// Maximum number of VM regions to enumerate (safety cap).
pub(crate) const MAX_VM_REGIONS: usize = 2000;

/// Result of processing one VM region query in the enumeration loop.
pub(crate) enum VmEnumAction {
    /// Add region and advance address.
    AddRegion { next_address: u64 },
    /// End of address space reached.
    Done,
    /// Query failed, skip this page.
    SkipPage { next_address: u64 },
    /// Maximum regions reached.
    Truncated,
}

/// Pure logic for deciding the next action in VM region enumeration.
///
/// `query_result`: `Ok((region_size, query_addr))` on success,
///                 `Err((kern_return, current_address))` on failure.
pub(crate) fn vm_enum_action(
    region_count: usize,
    query_result: Result<(u64, u64), (i32, u64)>,
) -> VmEnumAction {
    if region_count >= MAX_VM_REGIONS {
        return VmEnumAction::Truncated;
    }
    match query_result {
        Ok((size, query_addr)) => VmEnumAction::AddRegion {
            next_address: query_addr.saturating_add(size),
        },
        Err((kr, _)) if kr == KERN_INVALID_ADDRESS => VmEnumAction::Done,
        Err((_, address)) => VmEnumAction::SkipPage {
            next_address: address.saturating_add(4096),
        },
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/platform/macos/memory_tests.rs"]
mod tests;
