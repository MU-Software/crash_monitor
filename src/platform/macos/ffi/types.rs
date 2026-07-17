//! RAII wrapper for Mach port rights and low-level type helpers.

use mach2::port::{MACH_PORT_NULL, mach_port_t};

/// Owned Mach port that deallocates its send right on drop.
pub struct OwnedMachPort(mach_port_t);

impl OwnedMachPort {
    #[must_use]
    pub fn new(port: mach_port_t) -> Self {
        Self(port)
    }

    #[must_use]
    pub fn raw(&self) -> mach_port_t {
        self.0
    }
}

impl Drop for OwnedMachPort {
    fn drop(&mut self) {
        if self.0 != MACH_PORT_NULL {
            // SAFETY: deallocate our send right to this port.
            unsafe {
                mach2::mach_port::mach_port_deallocate(self_task(), self.0);
            }
        }
    }
}

pub(crate) fn self_task() -> mach_port_t {
    // SAFETY: mach_task_self() returns the current task port, always valid.
    unsafe { mach2::traps::mach_task_self() }
}
