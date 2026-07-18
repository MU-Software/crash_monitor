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

/// Owned exception receive right. Destroying the receive right also releases
/// the inserted send right and wakes a blocking exception listener.
pub struct OwnedExceptionPort(mach_port_t);

impl OwnedExceptionPort {
    #[must_use]
    pub fn new(port: mach_port_t) -> Self {
        Self(port)
    }

    #[must_use]
    pub fn raw(&self) -> mach_port_t {
        self.0
    }

    /// Destroy the port now. Repeated calls and the later `Drop` are no-ops.
    pub fn destroy(&mut self) {
        if self.0 != MACH_PORT_NULL {
            // SAFETY: this owner uniquely controls destruction of the receive
            // right allocated for the monitored child.
            unsafe {
                mach2::mach_port::mach_port_destroy(self_task(), self.0);
            }
            self.0 = MACH_PORT_NULL;
        }
    }
}

impl Drop for OwnedExceptionPort {
    fn drop(&mut self) {
        self.destroy();
    }
}

pub(crate) fn self_task() -> mach_port_t {
    // SAFETY: mach_task_self() returns the current task port, always valid.
    unsafe { mach2::traps::mach_task_self() }
}
