//! Safe wrappers for Mach port allocation and process spawning with exception ports.

use mach2::mach_port::{mach_port_allocate, mach_port_insert_right};
use mach2::message::MACH_MSG_TYPE_MAKE_SEND;
use mach2::port::{MACH_PORT_RIGHT_RECEIVE, mach_port_t};
use nix::libc;
use std::fmt;
use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::Arc;

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
    /// Creating or configuring stdout/stderr capture pipes failed.
    OutputPipe,
    /// Configuring `posix_spawn` file actions failed.
    FileActions,
    /// Creating a dedicated process group for lifecycle ownership failed.
    ProcessGroup,
    /// Installing the child's explicit signal mask/default policy failed.
    SignalState,
    /// Creating or setting up the child executable failed.
    PosixSpawn,
}

impl fmt::Display for SpawnStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::AttrInit => "posix_spawnattr_init",
            Self::ExceptionPorts => "posix_spawnattr_setexceptionports_np",
            Self::OutputPipe => "pipe/fcntl",
            Self::FileActions => "posix_spawn_file_actions",
            Self::ProcessGroup => "posix_spawn process-group attributes",
            Self::SignalState => "posix_spawn signal attributes",
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
        write!(
            f,
            "{} failed: rc={} ({})",
            self.stage,
            self.rc,
            std::io::Error::from_raw_os_error(self.rc)
        )
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

unsafe fn configure_process_group(attr: *mut libc::posix_spawnattr_t) -> Result<(), SpawnError> {
    // SAFETY: caller owns an initialized spawn attribute object.
    let rc = unsafe { libc::posix_spawnattr_setpgroup(attr, 0) };
    if rc != 0 {
        return Err(SpawnError::new(SpawnStage::ProcessGroup, rc));
    }
    // SAFETY: same initialized attribute object; this selects the pgroup value
    // installed immediately above.
    let rc = unsafe {
        libc::posix_spawnattr_setflags(attr, libc::POSIX_SPAWN_SETPGROUP as libc::c_short)
    };
    if rc != 0 {
        return Err(SpawnError::new(SpawnStage::ProcessGroup, rc));
    }
    Ok(())
}

unsafe fn configure_signal_state(attr: *mut libc::posix_spawnattr_t) -> Result<(), SpawnError> {
    let mut mask = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
    // SAFETY: mask points to writable sigset storage.
    if unsafe { libc::sigemptyset(mask.as_mut_ptr()) } != 0 {
        return Err(SpawnError::new(SpawnStage::SignalState, last_errno()));
    }
    // SAFETY: mask was initialized above and attr is owned by the caller.
    let rc = unsafe { libc::posix_spawnattr_setsigmask(attr, mask.as_ptr()) };
    if rc != 0 {
        return Err(SpawnError::new(SpawnStage::SignalState, rc));
    }

    let mut defaults = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
    if unsafe { libc::sigemptyset(defaults.as_mut_ptr()) } != 0
        || unsafe { libc::sigaddset(defaults.as_mut_ptr(), libc::SIGPIPE) } != 0
    {
        return Err(SpawnError::new(SpawnStage::SignalState, last_errno()));
    }
    // Rust ignores SIGPIPE in the monitor. Restore only that inherited ignored
    // disposition; caught SIGTERM/SIGINT/SIGUSR1 handlers reset on exec.
    let rc = unsafe { libc::posix_spawnattr_setsigdefault(attr, defaults.as_ptr()) };
    if rc != 0 {
        return Err(SpawnError::new(SpawnStage::SignalState, rc));
    }

    let mut flags = 0 as libc::c_short;
    let rc = unsafe { libc::posix_spawnattr_getflags(attr, &raw mut flags) };
    if rc != 0 {
        return Err(SpawnError::new(SpawnStage::SignalState, rc));
    }
    flags |= (libc::POSIX_SPAWN_SETSIGMASK | libc::POSIX_SPAWN_SETSIGDEF) as libc::c_short;
    let rc = unsafe { libc::posix_spawnattr_setflags(attr, flags) };
    if rc != 0 {
        return Err(SpawnError::new(SpawnStage::SignalState, rc));
    }
    Ok(())
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
#[allow(dead_code)] // Compatibility wrapper used by focused spawn tests.
pub fn spawn_with_exception_port(
    exc_port: mach_port_t,
    app_path: &std::ffi::CStr,
    argv: &[&std::ffi::CStr],
    envp: &[&std::ffi::CStr],
) -> Result<libc::pid_t, SpawnError> {
    spawn_with_exception_port_impl(exc_port, app_path, argv, envp, None)
}

/// Spawn while redirecting stdout and stderr to continuously-drained bounded
/// tails. Pipe read ends remain private to the monitor and all original pipe
/// descriptors are closed by the child's file actions after `dup2`.
///
/// # Errors
/// Returns a [`SpawnError`] when pipe setup, file actions, spawn attributes, or
/// the synchronous `posix_spawn` operation fails.
pub fn spawn_with_exception_port_and_output(
    exc_port: mach_port_t,
    app_path: &std::ffi::CStr,
    argv: &[&std::ffi::CStr],
    envp: &[&std::ffi::CStr],
    output: Arc<crate::platform::macos::ChildOutputCapture>,
) -> Result<libc::pid_t, SpawnError> {
    spawn_with_exception_port_impl(exc_port, app_path, argv, envp, Some(output))
}

struct OutputPipes {
    stdout_read: OwnedFd,
    stdout_write: OwnedFd,
    stderr_read: OwnedFd,
    stderr_write: OwnedFd,
}

fn last_errno() -> libc::c_int {
    std::io::Error::last_os_error()
        .raw_os_error()
        .unwrap_or(libc::EIO)
}

fn move_above_stdio(fd: OwnedFd) -> Result<OwnedFd, SpawnError> {
    if fd.as_raw_fd() > libc::STDERR_FILENO {
        return Ok(fd);
    }

    // Child file actions close every original pipe descriptor after dup2.
    // Keeping those originals above stderr prevents a close action from
    // accidentally closing the newly-installed stdout/stderr descriptor when
    // the monitor itself was launched with fd 1 or 2 closed.
    // SAFETY: `fd` is valid and `F_DUPFD_CLOEXEC` returns a newly-owned fd.
    let duplicated = unsafe {
        libc::fcntl(
            fd.as_raw_fd(),
            libc::F_DUPFD_CLOEXEC,
            libc::STDERR_FILENO + 1,
        )
    };
    if duplicated == -1 {
        return Err(SpawnError::new(SpawnStage::OutputPipe, last_errno()));
    }
    // SAFETY: successful F_DUPFD_CLOEXEC transfers ownership of a new fd.
    Ok(unsafe { OwnedFd::from_raw_fd(duplicated) })
}

fn cloexec_pipe() -> Result<(OwnedFd, OwnedFd), SpawnError> {
    let mut fds = [-1; 2];
    // SAFETY: `fds` points to space for exactly two descriptors.
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        return Err(SpawnError::new(SpawnStage::OutputPipe, last_errno()));
    }
    // SAFETY: pipe returned two newly-owned descriptors.
    let read = move_above_stdio(unsafe { OwnedFd::from_raw_fd(fds[0]) })?;
    let write = move_above_stdio(unsafe { OwnedFd::from_raw_fd(fds[1]) })?;
    for fd in [&read, &write] {
        // SAFETY: descriptor is owned and valid for the duration of the call.
        if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
            return Err(SpawnError::new(SpawnStage::OutputPipe, last_errno()));
        }
    }
    Ok((read, write))
}

fn output_pipes() -> Result<OutputPipes, SpawnError> {
    let (stdout_read, stdout_write) = cloexec_pipe()?;
    let (stderr_read, stderr_write) = cloexec_pipe()?;
    Ok(OutputPipes {
        stdout_read,
        stdout_write,
        stderr_read,
        stderr_write,
    })
}

#[allow(clippy::too_many_lines)] // spawn attributes and file actions share one cleanup scope
fn spawn_with_exception_port_impl(
    exc_port: mach_port_t,
    app_path: &std::ffi::CStr,
    argv: &[&std::ffi::CStr],
    envp: &[&std::ffi::CStr],
    output: Option<Arc<crate::platform::macos::ChildOutputCapture>>,
) -> Result<libc::pid_t, SpawnError> {
    use std::ptr;

    let behavior = EXCEPTION_STATE_IDENTITY | MACH_EXCEPTION_CODES_FLAG;
    let pipes = output.as_ref().map(|_| output_pipes()).transpose()?;

    // SAFETY: posix_spawnattr lifecycle is init → configure → spawn → destroy.
    let pid = unsafe {
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

        // Make the spawned process the leader of a new process group before
        // applying stdout/stderr file actions.
        if let Err(error) = configure_process_group(&raw mut attr) {
            libc::posix_spawnattr_destroy(&raw mut attr);
            return Err(error);
        }
        if let Err(error) = configure_signal_state(&raw mut attr) {
            libc::posix_spawnattr_destroy(&raw mut attr);
            return Err(error);
        }

        let mut file_actions: libc::posix_spawn_file_actions_t = ptr::null_mut();
        let file_actions_ptr = if let Some(pipes) = &pipes {
            let rc = libc::posix_spawn_file_actions_init(&raw mut file_actions);
            if rc != 0 {
                libc::posix_spawnattr_destroy(&raw mut attr);
                return Err(SpawnError::new(SpawnStage::FileActions, rc));
            }
            let actions = [
                libc::posix_spawn_file_actions_adddup2(
                    &raw mut file_actions,
                    pipes.stdout_write.as_raw_fd(),
                    libc::STDOUT_FILENO,
                ),
                libc::posix_spawn_file_actions_adddup2(
                    &raw mut file_actions,
                    pipes.stderr_write.as_raw_fd(),
                    libc::STDERR_FILENO,
                ),
                libc::posix_spawn_file_actions_addclose(
                    &raw mut file_actions,
                    pipes.stdout_read.as_raw_fd(),
                ),
                libc::posix_spawn_file_actions_addclose(
                    &raw mut file_actions,
                    pipes.stdout_write.as_raw_fd(),
                ),
                libc::posix_spawn_file_actions_addclose(
                    &raw mut file_actions,
                    pipes.stderr_read.as_raw_fd(),
                ),
                libc::posix_spawn_file_actions_addclose(
                    &raw mut file_actions,
                    pipes.stderr_write.as_raw_fd(),
                ),
            ];
            if let Some(rc) = actions.into_iter().find(|rc| *rc != 0) {
                libc::posix_spawn_file_actions_destroy(&raw mut file_actions);
                libc::posix_spawnattr_destroy(&raw mut attr);
                return Err(SpawnError::new(SpawnStage::FileActions, rc));
            }
            &raw const file_actions
        } else {
            ptr::null()
        };

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
            file_actions_ptr,
            &raw const attr,
            c_argv.as_ptr(),
            c_envp.as_ptr(),
        );

        if pipes.is_some() {
            libc::posix_spawn_file_actions_destroy(&raw mut file_actions);
        }
        libc::posix_spawnattr_destroy(&raw mut attr);

        if rc != 0 {
            return Err(SpawnError::new(SpawnStage::PosixSpawn, rc));
        }

        pid
    };

    if let (Some(output), Some(pipes)) = (output, pipes) {
        drop(pipes.stdout_write);
        drop(pipes.stderr_write);
        output.attach_stdout(File::from(pipes.stdout_read));
        output.attach_stderr(File::from(pipes.stderr_read));
    }

    Ok(pid)
}

/// A tiny process that outlives the monitor long enough to kill the monitored
/// process group if the monitor disappears without running destructors.
///
/// The guardian blocks in async-signal-safe `read(2)`. Normal shutdown writes
/// one disarm byte; monitor death closes the CLOEXEC pipe and produces EOF,
/// causing a group-wide SIGKILL. It owns no Rust heap state in the fork child.
pub struct ParentDeathGuard {
    write_fd: libc::c_int,
    guardian_pid: libc::pid_t,
    armed: bool,
}

impl ParentDeathGuard {
    /// Install a parent-death guard for `process_group`.
    ///
    /// # Errors
    /// Returns an error if the CLOEXEC pipe or guardian process cannot be
    /// created.
    pub fn install(process_group: libc::pid_t) -> Result<Self, String> {
        if process_group <= 1 {
            return Err(format!(
                "parent-death guard requires a positive child PGID, got {process_group}"
            ));
        }
        let mut pipe_fds = [-1; 2];
        // SAFETY: `pipe_fds` provides storage for exactly two descriptors.
        if unsafe { libc::pipe(pipe_fds.as_mut_ptr()) } != 0 {
            return Err(format!(
                "parent-death guard pipe failed: {}",
                std::io::Error::last_os_error()
            ));
        }

        for fd in pipe_fds {
            // SAFETY: both descriptors were returned by `pipe` above.
            if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
                let error = std::io::Error::last_os_error();
                // SAFETY: close each descriptor exactly once on setup failure.
                unsafe {
                    libc::close(pipe_fds[0]);
                    libc::close(pipe_fds[1]);
                }
                return Err(format!("parent-death guard CLOEXEC failed: {error}"));
            }
        }

        // SAFETY: the fork child calls only async-signal-safe libc functions
        // before `_exit`, avoiding inherited Rust runtime state.
        let guardian_pid = unsafe { libc::fork() };
        if guardian_pid < 0 {
            let error = std::io::Error::last_os_error();
            unsafe {
                libc::close(pipe_fds[0]);
                libc::close(pipe_fds[1]);
            }
            return Err(format!("parent-death guard fork failed: {error}"));
        }
        if guardian_pid == 0 {
            unsafe {
                libc::close(pipe_fds[1]);
                let mut disarm = 0_u8;
                loop {
                    let read_result = libc::read(
                        pipe_fds[0],
                        std::ptr::from_mut(&mut disarm).cast::<libc::c_void>(),
                        1,
                    );
                    if read_result > 0 {
                        libc::_exit(0);
                    }
                    if read_result == 0 {
                        libc::kill(-process_group, libc::SIGKILL);
                        libc::_exit(0);
                    }
                    if *libc::__error() != libc::EINTR {
                        libc::kill(-process_group, libc::SIGKILL);
                        libc::_exit(1);
                    }
                }
            }
        }

        // SAFETY: the parent needs only the write end.
        unsafe {
            libc::close(pipe_fds[0]);
        }
        Ok(Self {
            write_fd: pipe_fds[1],
            guardian_pid,
            armed: true,
        })
    }

    /// Tell the guardian that normal supervisor cleanup has completed.
    pub fn disarm(&mut self) {
        if !self.armed {
            return;
        }
        self.armed = false;
        let byte = 1_u8;
        // SAFETY: the descriptor is uniquely owned here. Write failure means
        // the guardian has already exited, so cleanup can continue.
        unsafe {
            libc::write(
                self.write_fd,
                std::ptr::from_ref(&byte).cast::<libc::c_void>(),
                1,
            );
            libc::close(self.write_fd);
        }
        self.write_fd = -1;

        loop {
            // SAFETY: wait only for the dedicated guardian child.
            let result = unsafe { libc::waitpid(self.guardian_pid, std::ptr::null_mut(), 0) };
            if result == self.guardian_pid
                || result < 0 && nix::errno::Errno::last() != nix::errno::Errno::EINTR
            {
                break;
            }
        }
    }
}

impl Drop for ParentDeathGuard {
    fn drop(&mut self) {
        self.disarm();
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
                "posix_spawnattr_setexceptionports_np failed: rc={} ({})",
                libc::EINVAL,
                std::io::Error::from_raw_os_error(libc::EINVAL)
            )
        );

        assert_std_error::<SpawnError>();
    }

    #[test]
    fn output_pipe_descriptors_never_overlap_stdio() {
        let pipes = output_pipes().expect("create capture pipes");

        for fd in [
            &pipes.stdout_read,
            &pipes.stdout_write,
            &pipes.stderr_read,
            &pipes.stderr_write,
        ] {
            assert!(fd.as_raw_fd() > libc::STDERR_FILENO);
        }
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

    #[test]
    fn captured_output_is_drained_without_exceeding_the_tail_limit() {
        let port = allocate_receive_port().expect("allocate exception receive port");
        insert_send_right(port).expect("insert exception port send right");
        let app_path = CString::new("/bin/sh").unwrap();
        let script = CString::new("head -c 262144 /dev/zero; printf 'stderr-tail' >&2").unwrap();
        let dash_c = CString::new("-c").unwrap();
        let argv = [app_path.as_c_str(), dash_c.as_c_str(), script.as_c_str()];
        let output = Arc::new(crate::platform::macos::ChildOutputCapture::new(1024));

        let pid = spawn_with_exception_port_and_output(port, &app_path, &argv, &[], output.clone())
            .expect("spawn output fixture");
        // SAFETY: `pid` is the direct child returned above and status is valid.
        let mut status = 0;
        assert_eq!(unsafe { libc::waitpid(pid, &raw mut status, 0) }, pid);

        let snapshot = output.snapshot_after_eof(std::time::Duration::from_secs(1));
        assert_eq!(snapshot.stdout.tail.len(), 1024);
        assert!(snapshot.stdout.truncated);
        assert_eq!(snapshot.stderr.tail, "stderr-tail");
        assert!(!snapshot.stderr.truncated);
        assert!(snapshot.stdout.read_error.is_none());
        assert!(snapshot.stderr.read_error.is_none());

        // SAFETY: `port` is a receive right allocated by this test.
        let destroy_result = unsafe { mach_port_destroy(self_task(), port) };
        assert_eq!(destroy_result, 0, "destroy temporary exception port");
    }

    #[test]
    fn spawn_attributes_create_a_dedicated_process_group() {
        unsafe {
            let mut attr: libc::posix_spawnattr_t = std::ptr::null_mut();
            assert_eq!(libc::posix_spawnattr_init(&raw mut attr), 0);
            configure_process_group(&raw mut attr).expect("configure child process group");

            let mut flags = 0 as libc::c_short;
            let mut pgroup = -1 as libc::pid_t;
            assert_eq!(
                libc::posix_spawnattr_getflags(&raw const attr, &raw mut flags),
                0
            );
            assert_eq!(
                libc::posix_spawnattr_getpgroup(&raw const attr, &raw mut pgroup),
                0
            );
            assert_ne!(flags & libc::POSIX_SPAWN_SETPGROUP as libc::c_short, 0);
            assert_eq!(pgroup, 0, "zero requests child PID as its new PGID");
            libc::posix_spawnattr_destroy(&raw mut attr);
        }
    }

    #[test]
    fn spawn_attributes_set_an_empty_mask_and_restore_only_sigpipe() {
        unsafe {
            let mut attr: libc::posix_spawnattr_t = std::ptr::null_mut();
            assert_eq!(libc::posix_spawnattr_init(&raw mut attr), 0);
            configure_signal_state(&raw mut attr).expect("configure signal policy");

            let mut flags = 0 as libc::c_short;
            let mut mask = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
            let mut defaults = std::mem::MaybeUninit::<libc::sigset_t>::uninit();
            assert_eq!(
                libc::posix_spawnattr_getflags(&raw const attr, &raw mut flags),
                0
            );
            assert_eq!(
                libc::posix_spawnattr_getsigmask(&raw const attr, mask.as_mut_ptr()),
                0
            );
            assert_eq!(
                libc::posix_spawnattr_getsigdefault(&raw const attr, defaults.as_mut_ptr()),
                0
            );
            let mask = mask.assume_init();
            let defaults = defaults.assume_init();
            assert_ne!(flags & libc::POSIX_SPAWN_SETSIGMASK as libc::c_short, 0);
            assert_ne!(flags & libc::POSIX_SPAWN_SETSIGDEF as libc::c_short, 0);
            assert_eq!(libc::sigismember(&raw const mask, libc::SIGTERM), 0);
            assert_eq!(libc::sigismember(&raw const defaults, libc::SIGPIPE), 1);
            assert_eq!(libc::sigismember(&raw const defaults, libc::SIGTERM), 0);
            libc::posix_spawnattr_destroy(&raw mut attr);
        }
    }

    #[test]
    fn parent_death_guard_kills_the_owned_process_group_on_pipe_eof() {
        let mut ready_pipe = [-1; 2];
        assert_eq!(unsafe { libc::pipe(ready_pipe.as_mut_ptr()) }, 0);
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            unsafe {
                libc::close(ready_pipe[0]);
                if libc::setpgid(0, 0) != 0 {
                    libc::_exit(2);
                }
                let ready = 1_u8;
                libc::write(
                    ready_pipe[1],
                    std::ptr::from_ref(&ready).cast::<libc::c_void>(),
                    1,
                );
                libc::close(ready_pipe[1]);
                loop {
                    libc::pause();
                }
            }
        }
        assert!(pid > 0, "fork test child");
        unsafe {
            libc::close(ready_pipe[1]);
            let mut ready = 0_u8;
            assert_eq!(
                libc::read(
                    ready_pipe[0],
                    std::ptr::from_mut(&mut ready).cast::<libc::c_void>(),
                    1,
                ),
                1
            );
            libc::close(ready_pipe[0]);
        }
        assert_eq!(unsafe { libc::getpgid(pid) }, pid);
        let mut guard = ParentDeathGuard::install(pid).expect("install parent-death guard");

        // Simulate abrupt monitor death by closing the only parent-side pipe
        // descriptor without sending the normal disarm byte.
        guard.armed = false;
        unsafe {
            libc::close(guard.write_fd);
        }
        guard.write_fd = -1;

        let mut guardian_status = 0;
        // SAFETY: wait for the dedicated guardian and monitored child only.
        unsafe {
            assert_eq!(
                libc::waitpid(guard.guardian_pid, &raw mut guardian_status, 0),
                guard.guardian_pid
            );
        }
        assert!(libc::WIFEXITED(guardian_status));

        let mut child_status = 0;
        unsafe {
            assert_eq!(libc::waitpid(pid, &raw mut child_status, 0), pid);
        }
        assert!(libc::WIFSIGNALED(child_status));
        assert_eq!(libc::WTERMSIG(child_status), libc::SIGKILL);
    }
}
