//! Safe wrappers for Mach task operations.

use mach2::kern_return::KERN_SUCCESS;
use mach2::port::{MACH_PORT_NULL, MACH_PORT_RIGHT_SEND, mach_port_t};
use mach2::task::{task_resume, task_suspend, task_terminate, task_threads};
use mach2::vm::mach_vm_deallocate;

use crate::platform::macos::types::{MachError, mach_result};

use super::types::self_task;

/// Get the task port for a given PID.
///
/// # Errors
/// Returns `MachError` if `task_for_pid` fails (requires debugger entitlement).
pub fn get_task_for_pid(pid: i32) -> Result<mach_port_t, MachError> {
    let mut task: mach_port_t = 0;
    // SAFETY: task_for_pid is a kernel syscall. Requires debugger entitlement.
    let kr = unsafe { mach2::traps::task_for_pid(self_task(), pid, &raw mut task) };
    mach_result("task_for_pid", kr)?;
    Ok(task)
}

/// Add one owned user reference to an existing task send right.
///
/// Capture workers use this to keep the Mach name alive independently of the
/// supervisor's `OwnedTaskPort`. A detached worker must never borrow the sole
/// user reference from a shorter-lived owner.
///
/// # Errors
/// Returns `MachError` when `task` is null/dead or the send right cannot be
/// retained in the current task's IPC namespace.
pub fn retain_task_port(task: mach_port_t) -> Result<(), MachError> {
    if task == MACH_PORT_NULL {
        return Err(MachError {
            function: "mach_port_mod_refs(task send right)",
            kern_return: mach2::kern_return::KERN_INVALID_ARGUMENT,
        });
    }
    // SAFETY: `task` names an existing send right in this task. Incrementing
    // its user-reference count creates an independently owned reference that
    // must later be balanced by `mach_port_deallocate`.
    let kr =
        unsafe { mach2::mach_port::mach_port_mod_refs(self_task(), task, MACH_PORT_RIGHT_SEND, 1) };
    mach_result("mach_port_mod_refs(task send right)", kr)
}

/// Release one task send-right user reference previously retained by
/// [`retain_task_port`].
pub fn deallocate_task_port(task: mach_port_t) {
    if task != MACH_PORT_NULL {
        // SAFETY: balances one owned send-right user reference.
        unsafe {
            mach2::mach_port::mach_port_deallocate(self_task(), task);
        }
    }
}

/// Suspend all threads in the target task.
///
/// # Errors
/// Returns `MachError` if the `task_suspend` kernel call fails.
pub fn suspend_task(task: mach_port_t) -> Result<(), MachError> {
    // SAFETY: task_suspend is a kernel syscall on a valid task port.
    let kr = unsafe { task_suspend(task) };
    mach_result("task_suspend", kr)
}

/// Resume all threads in the target task.
///
/// # Errors
/// Returns `MachError` if the `task_resume` kernel call fails.
pub fn resume_task(task: mach_port_t) -> Result<(), MachError> {
    // SAFETY: task_resume is a kernel syscall on a valid task port.
    let kr = unsafe { task_resume(task) };
    mach_result("task_resume", kr)
}

/// Terminate a task that could not be resumed safely.
///
/// # Errors
/// Returns `MachError` if the `task_terminate` kernel call fails.
pub fn terminate_task(task: mach_port_t) -> Result<(), MachError> {
    // SAFETY: task_terminate is a kernel syscall on a valid task port.
    let kr = unsafe { task_terminate(task) };
    mach_result("task_terminate", kr)
}

/// Get thread ports for all threads in the target task.
///
/// # Errors
/// Returns `MachError` if the `task_threads` kernel call fails.
pub fn get_task_threads(task: mach_port_t) -> Result<Vec<mach_port_t>, MachError> {
    let mut threads_ptr: *mut mach_port_t = std::ptr::null_mut();
    let mut thread_count: u32 = 0;

    // SAFETY: task_threads writes to the out-params; we deallocate the buffer after copying.
    let kr = unsafe { task_threads(task, &raw mut threads_ptr, &raw mut thread_count) };
    mach_result("task_threads", kr)?;

    if thread_count == 0 || threads_ptr.is_null() {
        return Ok(Vec::new());
    }

    // Copy thread ports into a Vec, then deallocate the kernel-allocated buffer
    let threads = unsafe {
        let slice = std::slice::from_raw_parts(threads_ptr, thread_count as usize);
        let vec = slice.to_vec();
        let kr = mach_vm_deallocate(
            self_task(),
            threads_ptr as u64,
            u64::from(thread_count) * (std::mem::size_of::<mach_port_t>() as u64),
        );
        if kr != KERN_SUCCESS {
            eprintln!("[monitor] warning: mach_vm_deallocate(threads) failed: kr={kr}");
        }
        vec
    };

    Ok(threads)
}

/// Deallocate a send right for a thread port obtained from `task_threads()`.
/// Call this after the thread port is no longer needed to prevent port right leaks.
pub fn deallocate_thread_port(thread: mach_port_t) {
    if thread != mach2::port::MACH_PORT_NULL {
        // SAFETY: deallocates our send right to the thread port.
        unsafe {
            mach2::mach_port::mach_port_deallocate(self_task(), thread);
        }
    }
}
