//! Safe wrappers for Mach port allocation and process spawning with exception ports.

use mach2::mach_port::{mach_port_allocate, mach_port_insert_right};
use mach2::message::MACH_MSG_TYPE_MAKE_SEND;
use mach2::port::{MACH_PORT_RIGHT_RECEIVE, mach_port_t};
use nix::libc;

use crate::platform::macos::types::{
    ARM_THREAD_STATE64, CRASH_EXCEPTION_MASK, EXCEPTION_STATE_IDENTITY, MACH_EXCEPTION_CODES_FLAG,
    MachError, mach_result,
};

use super::types::self_task;

/// Allocate a Mach receive right.
///
/// # Errors
/// Returns `MachError` if `mach_port_allocate` fails.
pub fn allocate_receive_port() -> Result<mach_port_t, MachError> {
    let mut port: mach_port_t = 0;
    // SAFETY: mach_port_allocate creates a new port right.
    let kr = unsafe { mach_port_allocate(self_task(), MACH_PORT_RIGHT_RECEIVE, &raw mut port) };
    mach_result("mach_port_allocate", kr)?;
    Ok(port)
}

/// Insert a send right for an existing receive port.
///
/// # Errors
/// Returns `MachError` if `mach_port_insert_right` fails.
pub fn insert_send_right(port: mach_port_t) -> Result<(), MachError> {
    // SAFETY: mach_port_insert_right adds a send right to an existing port.
    let kr = unsafe { mach_port_insert_right(self_task(), port, port, MACH_MSG_TYPE_MAKE_SEND) };
    mach_result("mach_port_insert_right", kr)
}

// posix_spawnattr_setexceptionports_np is a macOS-specific extension not in libc crate
unsafe extern "C" {
    fn posix_spawnattr_setexceptionports_np(
        attr: *mut libc::posix_spawnattr_t,
        mask: u32,
        new_port: mach_port_t,
        behavior: i32,
        flavor: i32,
    ) -> libc::c_int;
}

/// Spawn a child process via `posix_spawn` with exception ports pre-configured.
/// The exception port survives exec (via `posix_spawnattr_setexceptionports_np`).
/// Returns the child PID.
///
/// # Errors
/// Returns an error string if `posix_spawnattr_init`, exception port setup, or `posix_spawn` fails.
pub fn spawn_with_exception_port(
    exc_port: mach_port_t,
    app_path: &std::ffi::CStr,
    argv: &[&std::ffi::CStr],
    envp: &[&std::ffi::CStr],
) -> Result<libc::pid_t, String> {
    use std::ptr;

    let behavior = EXCEPTION_STATE_IDENTITY | MACH_EXCEPTION_CODES_FLAG;

    // SAFETY: posix_spawnattr lifecycle is init → configure → spawn → destroy.
    unsafe {
        let mut attr: libc::posix_spawnattr_t = ptr::null_mut();
        if libc::posix_spawnattr_init(&raw mut attr) != 0 {
            return Err("posix_spawnattr_init failed".into());
        }

        // Set exception port on the spawned child (survives exec)
        let rc = posix_spawnattr_setexceptionports_np(
            &raw mut attr,
            CRASH_EXCEPTION_MASK,
            exc_port,
            behavior,
            ARM_THREAD_STATE64,
        );
        if rc != 0 {
            libc::posix_spawnattr_destroy(&raw mut attr);
            return Err(format!(
                "posix_spawnattr_setexceptionports_np failed: rc={rc}"
            ));
        }

        // Build null-terminated argv/envp arrays
        let mut c_argv: Vec<*mut libc::c_char> =
            argv.iter().map(|s| s.as_ptr().cast_mut()).collect();
        c_argv.push(ptr::null_mut());

        let mut c_envp: Vec<*mut libc::c_char> =
            envp.iter().map(|s| s.as_ptr().cast_mut()).collect();
        c_envp.push(ptr::null_mut());

        let mut pid: libc::pid_t = 0;
        let rc = libc::posix_spawn(
            &raw mut pid,
            app_path.as_ptr(),
            ptr::null(), // file_actions
            &raw const attr,
            c_argv.as_ptr(),
            c_envp.as_ptr(),
        );

        libc::posix_spawnattr_destroy(&raw mut attr);

        if rc != 0 {
            return Err(format!("posix_spawn failed: rc={rc}"));
        }

        Ok(pid)
    }
}
