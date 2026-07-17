//! Safe wrappers for Mach port allocation and process spawning with exception ports.

use mach2::mach_port::{mach_port_allocate, mach_port_insert_right};
use mach2::message::MACH_MSG_TYPE_MAKE_SEND;
use mach2::port::{MACH_PORT_RIGHT_RECEIVE, mach_port_t};
use nix::libc;
use std::fmt;

use crate::platform::macos::types::{
    ARM_THREAD_STATE64, CRASH_EXCEPTION_MASK, EXCEPTION_STATE_IDENTITY, MACH_EXCEPTION_CODES_FLAG,
    MachError, mach_result,
};

use super::types::self_task;

/// The `posix_spawn` setup stage that failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpawnStage {
    /// Initializing the spawn attributes failed.
    AttrInit,
    /// Installing the Mach exception port in the spawn attributes failed.
    ExceptionPorts,
    /// Creating or setting up the child executable failed.
    PosixSpawn,
}

impl fmt::Display for SpawnStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::AttrInit => "posix_spawnattr_init",
            Self::ExceptionPorts => "posix_spawnattr_setexceptionports_np",
            Self::PosixSpawn => "posix_spawn",
        })
    }
}

/// A synchronous failure while configuring or starting a child process.
///
/// The APIs used here return an errno-style error number directly rather than
/// requiring the caller to read the thread-local `errno` value. That return
/// code is preserved verbatim in [`Self::rc`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SpawnError {
    /// The spawn operation that failed.
    pub stage: SpawnStage,
    /// The errno-style return code produced by the failed operation.
    pub rc: libc::c_int,
}

impl SpawnError {
    const fn new(stage: SpawnStage, rc: libc::c_int) -> Self {
        Self { stage, rc }
    }
}

impl fmt::Display for SpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} failed: rc={}", self.stage, self.rc)
    }
}

impl std::error::Error for SpawnError {}

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
/// On macOS, a non-zero `posix_spawn` return is a synchronous spawn or exec
/// setup failure. It is returned as [`SpawnStage::PosixSpawn`], not as a child
/// [`nix::sys::wait::WaitStatus`].
///
/// # Errors
/// Returns [`SpawnError`] with the failing stage and its errno-style return
/// code if spawn-attribute initialization, exception-port setup, or
/// `posix_spawn` fails.
pub fn spawn_with_exception_port(
    exc_port: mach_port_t,
    app_path: &std::ffi::CStr,
    argv: &[&std::ffi::CStr],
    envp: &[&std::ffi::CStr],
) -> Result<libc::pid_t, SpawnError> {
    use std::ptr;

    let behavior = EXCEPTION_STATE_IDENTITY | MACH_EXCEPTION_CODES_FLAG;

    // SAFETY: posix_spawnattr lifecycle is init → configure → spawn → destroy.
    unsafe {
        let mut attr: libc::posix_spawnattr_t = ptr::null_mut();
        let rc = libc::posix_spawnattr_init(&raw mut attr);
        if rc != 0 {
            return Err(SpawnError::new(SpawnStage::AttrInit, rc));
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
            return Err(SpawnError::new(SpawnStage::ExceptionPorts, rc));
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
            return Err(SpawnError::new(SpawnStage::PosixSpawn, rc));
        }

        Ok(pid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mach2::mach_port::mach_port_destroy;
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    fn assert_std_error<T: std::error::Error>() {}

    #[test]
    fn spawn_error_preserves_stage_and_return_code() {
        let error = SpawnError {
            stage: SpawnStage::ExceptionPorts,
            rc: libc::EINVAL,
        };

        assert_eq!(error.stage, SpawnStage::ExceptionPorts);
        assert_eq!(error.rc, libc::EINVAL);
        assert_eq!(
            error.to_string(),
            format!(
                "posix_spawnattr_setexceptionports_np failed: rc={}",
                libc::EINVAL
            )
        );

        assert_std_error::<SpawnError>();
    }

    #[test]
    fn nonexistent_executable_is_a_synchronous_posix_spawn_error() {
        let port = allocate_receive_port().expect("allocate exception receive port");
        insert_send_right(port).expect("insert exception port send right");

        let temp_dir = tempfile::tempdir().expect("create temporary directory");
        let missing_path = temp_dir.path().join("missing-child-executable");
        let app_path = CString::new(missing_path.as_os_str().as_bytes())
            .expect("temporary path contains no NUL");
        let argv = [app_path.as_c_str()];

        let result = spawn_with_exception_port(port, &app_path, &argv, &[]);

        // SAFETY: `port` is a receive right allocated by this test. Destroying
        // it releases both the receive right and the inserted send right.
        let destroy_result = unsafe { mach_port_destroy(self_task(), port) };
        assert_eq!(destroy_result, 0, "destroy temporary exception port");

        let error = result.expect_err("a missing executable must fail synchronously");
        assert_eq!(error.stage, SpawnStage::PosixSpawn);
        assert_eq!(error.rc, libc::ENOENT);
    }
}
