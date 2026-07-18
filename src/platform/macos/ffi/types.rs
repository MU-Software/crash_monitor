//! RAII wrappers for distinct Mach port right kinds and low-level helpers.

use std::marker::PhantomData;

use mach2::port::{MACH_PORT_NULL, MACH_PORT_RIGHT_RECEIVE, mach_port_t};

pub enum GenericSendRight {}
pub enum TaskSendRight {}
pub enum ThreadSendRight {}

/// One owned user reference to a Mach send right of kind `Kind`.
pub struct SendRight<Kind> {
    port: mach_port_t,
    kind: PhantomData<fn() -> Kind>,
}

pub type OwnedSendRight = SendRight<GenericSendRight>;
pub type OwnedTaskPort = SendRight<TaskSendRight>;
pub type OwnedThreadPort = SendRight<ThreadSendRight>;

impl<Kind> SendRight<Kind> {
    #[must_use]
    pub fn new(port: mach_port_t) -> Self {
        Self {
            port,
            kind: PhantomData,
        }
    }

    #[must_use]
    pub fn raw(&self) -> mach_port_t {
        self.port
    }
}

impl<Kind> Drop for SendRight<Kind> {
    fn drop(&mut self) {
        if self.port != MACH_PORT_NULL {
            // SAFETY: deallocate our send right to this port.
            unsafe {
                mach2::mach_port::mach_port_deallocate(self_task(), self.port);
            }
        }
    }
}

/// One owned Mach receive right.
///
/// Dropping the final receive reference destroys the port, releases any
/// process-local send rights, and wakes blocked receivers. It must never use
/// `mach_port_deallocate`, which only releases send/send-once user refs.
pub struct OwnedReceiveRight(mach_port_t);

impl OwnedReceiveRight {
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
            // SAFETY: this owner uniquely owns one receive-right reference.
            // Decrementing it to zero destroys the receive right and the port.
            let kr = unsafe {
                mach2::mach_port::mach_port_mod_refs(
                    self_task(),
                    self.0,
                    MACH_PORT_RIGHT_RECEIVE,
                    -1,
                )
            };
            if kr != 0 {
                eprintln!("[monitor] mach_port_mod_refs(receive right) failed: kr={kr}");
            }
            self.0 = MACH_PORT_NULL;
        }
    }
}

impl Drop for OwnedReceiveRight {
    fn drop(&mut self) {
        self.destroy();
    }
}

/// Receive right dedicated to the exception listener.
pub type OwnedExceptionPort = OwnedReceiveRight;

pub(crate) fn self_task() -> mach_port_t {
    // SAFETY: mach_task_self() returns the current task port, always valid.
    unsafe { mach2::traps::mach_task_self() }
}
