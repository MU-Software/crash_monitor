//! Pure logic for VM memory region enumeration.

use mach2::kern_return::KERN_INVALID_ADDRESS;

/// Maximum number of VM regions to enumerate (safety cap).
pub(crate) const MAX_VM_REGIONS: usize = 2000;

/// Maximum kernel queries, including failed queries that do not add a region.
pub(crate) const MAX_VM_REGION_QUERY_ATTEMPTS: usize = MAX_VM_REGIONS * 4;

/// Stop after this many consecutive non-terminal query failures.
pub(crate) const MAX_CONSECUTIVE_VM_REGION_FAILURES: usize = 64;

/// Completeness of a VM-region enumeration result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmRegionEnumerationQuality {
    Complete,
    RegionLimit,
    QueryAttemptLimit,
    ConsecutiveErrorLimit,
    SkippedQueryErrors { count: usize },
    NoAddressProgress,
    AddressOverflow,
    CaptureDeadline,
}

impl VmRegionEnumerationQuality {
    #[must_use]
    pub const fn is_complete(self) -> bool {
        matches!(self, Self::Complete)
    }
}

impl std::fmt::Display for VmRegionEnumerationQuality {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Complete => f.write_str("complete"),
            Self::RegionLimit => f.write_str("region limit reached"),
            Self::QueryAttemptLimit => f.write_str("query attempt limit reached"),
            Self::ConsecutiveErrorLimit => f.write_str("consecutive query error limit reached"),
            Self::SkippedQueryErrors { count } => {
                write!(f, "completed with {count} skipped query error(s)")
            }
            Self::NoAddressProgress => f.write_str("address made no forward progress"),
            Self::AddressOverflow => f.write_str("address overflow reached the terminal address"),
            Self::CaptureDeadline => f.write_str("capture deadline reached"),
        }
    }
}

/// Result of processing one VM region query in the enumeration loop.
pub(crate) enum VmEnumAction {
    /// Add the returned region and continue at the state's next address.
    AddRegion,
    /// Add the returned region and then stop with partial quality.
    AddTerminalRegion(VmRegionEnumerationQuality),
    /// End of address space reached normally.
    Done(VmRegionEnumerationQuality),
    /// Query failed, skip one host page.
    SkipPage,
    /// Stop without adding the current query result.
    Stop(VmRegionEnumerationQuality),
}

/// Stateful, pure accounting for the Mach VM-region query loop.
pub(crate) struct VmEnumerationState {
    address: u64,
    query_attempts: usize,
    consecutive_failures: usize,
    total_query_failures: usize,
    region_count: usize,
    page_size: u64,
}

impl VmEnumerationState {
    #[must_use]
    pub(crate) const fn new(page_size: u64) -> Self {
        Self {
            address: 0,
            query_attempts: 0,
            consecutive_failures: 0,
            total_query_failures: 0,
            region_count: 0,
            page_size,
        }
    }

    #[must_use]
    pub(crate) const fn address(&self) -> u64 {
        self.address
    }

    /// Return a limit reached before issuing another kernel query.
    #[must_use]
    pub(crate) const fn pre_query_stop(&self) -> Option<VmRegionEnumerationQuality> {
        if self.region_count >= MAX_VM_REGIONS {
            Some(VmRegionEnumerationQuality::RegionLimit)
        } else if self.query_attempts >= MAX_VM_REGION_QUERY_ATTEMPTS {
            Some(VmRegionEnumerationQuality::QueryAttemptLimit)
        } else {
            None
        }
    }

    /// Account for exactly one kernel result and choose the next action.
    pub(crate) fn process_query(&mut self, query_result: Result<(u64, u64), i32>) -> VmEnumAction {
        self.query_attempts = self.query_attempts.saturating_add(1);

        match query_result {
            Ok((size, query_address)) => {
                self.consecutive_failures = 0;
                let Some(next_address) = query_address.checked_add(size) else {
                    self.address = u64::MAX;
                    self.region_count = self.region_count.saturating_add(1);
                    return VmEnumAction::AddTerminalRegion(
                        VmRegionEnumerationQuality::AddressOverflow,
                    );
                };
                if next_address == u64::MAX {
                    self.address = u64::MAX;
                    self.region_count = self.region_count.saturating_add(1);
                    return VmEnumAction::AddTerminalRegion(
                        VmRegionEnumerationQuality::AddressOverflow,
                    );
                }
                if next_address <= self.address {
                    return VmEnumAction::Stop(VmRegionEnumerationQuality::NoAddressProgress);
                }

                self.address = next_address;
                self.region_count = self.region_count.saturating_add(1);
                VmEnumAction::AddRegion
            }
            Err(kr) if kr == KERN_INVALID_ADDRESS => {
                let quality = if self.total_query_failures == 0 {
                    VmRegionEnumerationQuality::Complete
                } else {
                    VmRegionEnumerationQuality::SkippedQueryErrors {
                        count: self.total_query_failures,
                    }
                };
                VmEnumAction::Done(quality)
            }
            Err(_) => {
                self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                self.total_query_failures = self.total_query_failures.saturating_add(1);
                if self.consecutive_failures >= MAX_CONSECUTIVE_VM_REGION_FAILURES {
                    return VmEnumAction::Stop(VmRegionEnumerationQuality::ConsecutiveErrorLimit);
                }

                let Some(next_address) = self.address.checked_add(self.page_size) else {
                    self.address = u64::MAX;
                    return VmEnumAction::Stop(VmRegionEnumerationQuality::AddressOverflow);
                };
                if next_address == u64::MAX {
                    self.address = u64::MAX;
                    return VmEnumAction::Stop(VmRegionEnumerationQuality::AddressOverflow);
                }
                if next_address <= self.address {
                    return VmEnumAction::Stop(VmRegionEnumerationQuality::NoAddressProgress);
                }

                self.address = next_address;
                VmEnumAction::SkipPage
            }
        }
    }

    #[cfg(test)]
    pub(crate) const fn query_attempts(&self) -> usize {
        self.query_attempts
    }

    #[cfg(test)]
    pub(crate) const fn region_count(&self) -> usize {
        self.region_count
    }
}

#[cfg(test)]
#[path = "../../../tests/unit/platform/macos/memory_tests.rs"]
mod tests;
