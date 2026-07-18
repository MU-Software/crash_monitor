//! Killable capture-helper process setup and inherited Mach capability access.

use mach2::exception_types::{EXC_MASK_RPC_ALERT, EXCEPTION_DEFAULT, exception_mask_t};
use mach2::mach_port::{mach_port_allocate, mach_port_destroy, mach_port_insert_right};
use mach2::mach_types::exception_handler_t;
use mach2::message::{
    MACH_MSG_PORT_DESCRIPTOR, MACH_MSG_SUCCESS, MACH_MSG_TYPE_COPY_SEND, MACH_MSG_TYPE_MAKE_SEND,
    MACH_MSGH_BITS, MACH_MSGH_BITS_COMPLEX, MACH_RCV_MSG, MACH_RCV_TIMEOUT, MACH_SEND_MSG,
    MACH_SEND_TIMEOUT, mach_msg, mach_msg_body_t, mach_msg_destroy, mach_msg_header_t,
    mach_msg_port_descriptor_t,
};
use mach2::port::{MACH_PORT_NULL, MACH_PORT_RIGHT_RECEIVE, mach_port_t};
use mach2::task::{task_get_exception_ports, task_set_exception_ports};
use nix::libc;
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;
use std::ffi::{CString, OsStr};
use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::types::{OwnedReceiveRight, OwnedSendRight, OwnedTaskPort, OwnedThreadPort, self_task};

/// Descriptor on which the capture helper writes its framed result.
///
/// The source descriptor is always duplicated before spawn. This matters when
/// the caller's file already happens to be descriptor 3: a no-op `dup2(3, 3)`
/// would otherwise preserve `FD_CLOEXEC` and silently close the result channel
/// during `exec`.
pub const CAPTURE_HELPER_RESULT_FD: RawFd = 3;

const MAX_CAPTURE_HELPER_REQUEST_BYTES: usize = 64 * 1024;
const FIRST_TEMPORARY_FD: RawFd = CAPTURE_HELPER_RESULT_FD + 1;
const CAPABILITY_HANDOFF_TIMEOUT_MS: u32 = 1_000;
const FAILED_HANDOFF_REAP_GRACE: Duration = Duration::from_secs(2);
const FAILED_HANDOFF_POLL_INTERVAL: Duration = Duration::from_millis(5);
const CAPABILITY_HANDSHAKE_MESSAGE_ID: i32 = 0x4348_5001;
const CAPABILITY_TRANSFER_MESSAGE_ID: i32 = 0x4348_5002;
const CAPABILITY_WIRE_VERSION: u32 = 1;

static CAPTURE_HELPER_RESULT_TAKEN: AtomicBool = AtomicBool::new(false);

#[derive(Debug)]
pub struct CaptureHelperSpawnError {
    message: String,
    cleanup_unproven: bool,
}

impl CaptureHelperSpawnError {
    fn new(message: String, cleanup_unproven: bool) -> Self {
        Self {
            message,
            cleanup_unproven,
        }
    }

    #[must_use]
    pub const fn cleanup_unproven(&self) -> bool {
        self.cleanup_unproven
    }
}

impl std::fmt::Display for CaptureHelperSpawnError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for CaptureHelperSpawnError {}

impl From<String> for CaptureHelperSpawnError {
    fn from(message: String) -> Self {
        Self::new(message, false)
    }
}

/// Typed result of the capture helper's single authoritative `waitpid` owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureHelperReap {
    StillRunning,
    Exited(i32),
    Signaled {
        signal: i32,
        core_dumped: bool,
    },
    /// `ECHILD` or a second poll after terminal consumption means this owner
    /// can no longer prove that the helper was reaped.
    OwnershipLost,
}

/// Unique wait/reap capability for one spawned capture helper.
///
/// The value is created only after `posix_spawn` returns a valid PID. On a
/// successful handoff it moves to the capture supervisor; on handoff failure
/// it remains in the spawn layer for kill/reap cleanup. No PID-only late/global
/// reaper is allowed to compete for this child.
pub struct CaptureHelperProcess {
    pid: Pid,
    terminal_consumed: bool,
}

impl CaptureHelperProcess {
    const fn new(pid: Pid) -> Self {
        Self {
            pid,
            terminal_consumed: false,
        }
    }

    #[must_use]
    pub const fn pid(&self) -> Pid {
        self.pid
    }

    /// Poll and, when terminal, reap this helper exactly once.
    ///
    /// `ECHILD` is returned as [`CaptureHelperReap::OwnershipLost`], never as
    /// successful cleanup or evidence of timeout.
    ///
    /// # Errors
    /// Returns an error for wait failures other than `EINTR`/`ECHILD`.
    pub fn poll_reap(&mut self) -> Result<CaptureHelperReap, String> {
        self.poll_reap_with(|pid| waitpid(pid, Some(WaitPidFlag::WNOHANG)))
    }

    fn poll_reap_with(
        &mut self,
        mut wait: impl FnMut(Pid) -> Result<WaitStatus, nix::errno::Errno>,
    ) -> Result<CaptureHelperReap, String> {
        if self.terminal_consumed {
            return Ok(CaptureHelperReap::OwnershipLost);
        }
        loop {
            match wait(self.pid) {
                Ok(WaitStatus::Exited(_, status)) => {
                    self.terminal_consumed = true;
                    return Ok(CaptureHelperReap::Exited(status));
                }
                Ok(WaitStatus::Signaled(_, signal, core_dumped)) => {
                    self.terminal_consumed = true;
                    return Ok(CaptureHelperReap::Signaled {
                        signal: signal as i32,
                        core_dumped,
                    });
                }
                Ok(
                    WaitStatus::StillAlive | WaitStatus::Stopped(_, _) | WaitStatus::Continued(_),
                ) => {
                    return Ok(CaptureHelperReap::StillRunning);
                }
                Err(nix::errno::Errno::EINTR) => {}
                Err(nix::errno::Errno::ECHILD) => {
                    self.terminal_consumed = true;
                    return Ok(CaptureHelperReap::OwnershipLost);
                }
                Err(error) => {
                    return Err(format!(
                        "cannot wait for capture helper {}: {error}",
                        self.pid
                    ));
                }
            }
        }
    }
}

// macOS-specific extensions declared by <spawn.h> but not exposed by libc.
unsafe extern "C" {
    fn posix_spawnattr_setexceptionports_np(
        attr: *mut libc::posix_spawnattr_t,
        mask: exception_mask_t,
        port: mach_port_t,
        behavior: libc::c_int,
        flavor: libc::c_int,
    ) -> libc::c_int;
}

struct SpawnFileActions(libc::posix_spawn_file_actions_t);

impl SpawnFileActions {
    fn new() -> Result<Self, String> {
        let mut actions = std::ptr::null_mut();
        // SAFETY: `actions` is an out-parameter initialized by libc. The
        // successful value is destroyed exactly once by `Drop`.
        let rc = unsafe { libc::posix_spawn_file_actions_init(&raw mut actions) };
        if rc != 0 {
            return Err(errno_style_error("posix_spawn_file_actions_init", rc));
        }
        Ok(Self(actions))
    }

    fn add_dup2(&mut self, source: RawFd, target: RawFd) -> Result<(), String> {
        // SAFETY: the file-actions object is initialized and both descriptors
        // were validated as non-negative. libc copies only their integer IDs.
        let rc = unsafe { libc::posix_spawn_file_actions_adddup2(&raw mut self.0, source, target) };
        if rc != 0 {
            return Err(errno_style_error("posix_spawn_file_actions_adddup2", rc));
        }
        Ok(())
    }

    fn add_close(&mut self, fd: RawFd) -> Result<(), String> {
        // SAFETY: the initialized action list accepts a validated descriptor;
        // the close itself occurs only in the spawned child.
        let rc = unsafe { libc::posix_spawn_file_actions_addclose(&raw mut self.0, fd) };
        if rc != 0 {
            return Err(errno_style_error("posix_spawn_file_actions_addclose", rc));
        }
        Ok(())
    }

    const fn as_ptr(&self) -> *const libc::posix_spawn_file_actions_t {
        &raw const self.0
    }
}

impl Drop for SpawnFileActions {
    fn drop(&mut self) {
        // SAFETY: construction succeeded, and this is the unique owner.
        unsafe {
            libc::posix_spawn_file_actions_destroy(&raw mut self.0);
        }
    }
}

struct SpawnAttributes(libc::posix_spawnattr_t);

impl SpawnAttributes {
    fn new() -> Result<Self, String> {
        let mut attributes = std::ptr::null_mut();
        // SAFETY: `attributes` is an out-parameter initialized by libc. The
        // successful value is destroyed exactly once by `Drop`.
        let rc = unsafe { libc::posix_spawnattr_init(&raw mut attributes) };
        if rc != 0 {
            return Err(errno_style_error("posix_spawnattr_init", rc));
        }
        Ok(Self(attributes))
    }

    fn set_transport_port(
        &mut self,
        mask: exception_mask_t,
        port: mach_port_t,
    ) -> Result<(), String> {
        // SAFETY: `port` is a send right in the spawning task. The helper uses
        // an otherwise-unused exception-handler slot purely as an exec-safe
        // transport and retrieves the copied right immediately at entry.
        let rc = unsafe {
            posix_spawnattr_setexceptionports_np(
                &raw mut self.0,
                mask,
                port,
                EXCEPTION_DEFAULT as libc::c_int,
                0,
            )
        };
        if rc != 0 {
            return Err(errno_style_error(
                "posix_spawnattr_setexceptionports_np",
                rc,
            ));
        }
        Ok(())
    }

    fn set_flags(&mut self, flags: libc::c_short) -> Result<(), String> {
        // SAFETY: the attributes object is initialized and `flags` contains
        // only Darwin posix_spawn flags supported by this process.
        let rc = unsafe { libc::posix_spawnattr_setflags(&raw mut self.0, flags) };
        if rc != 0 {
            return Err(errno_style_error("posix_spawnattr_setflags", rc));
        }
        Ok(())
    }

    const fn as_ptr(&self) -> *const libc::posix_spawnattr_t {
        &raw const self.0
    }
}

impl Drop for SpawnAttributes {
    fn drop(&mut self) {
        // SAFETY: construction succeeded, and this is the unique owner.
        unsafe {
            libc::posix_spawnattr_destroy(&raw mut self.0);
        }
    }
}

fn errno_style_error(operation: &str, rc: libc::c_int) -> String {
    format!("{operation} failed: rc={rc}")
}

#[repr(C)]
struct OnePortMessage {
    header: mach_msg_header_t,
    body: mach_msg_body_t,
    port: mach_msg_port_descriptor_t,
    version: u32,
    port_count: u32,
}

#[repr(C)]
struct TwoPortMessage {
    header: mach_msg_header_t,
    body: mach_msg_body_t,
    ports: [mach_msg_port_descriptor_t; 2],
    version: u32,
    port_count: u32,
}

#[repr(C, align(8))]
struct ReceiveBuffer([u64; 64]);

impl Default for ReceiveBuffer {
    fn default() -> Self {
        Self([0; 64])
    }
}

fn allocate_receive_right() -> Result<OwnedReceiveRight, String> {
    let mut port = MACH_PORT_NULL;
    // SAFETY: `port` is a valid out-parameter for a new receive right.
    let allocate =
        unsafe { mach_port_allocate(self_task(), MACH_PORT_RIGHT_RECEIVE, &raw mut port) };
    if allocate != 0 {
        return Err(format!(
            "mach_port_allocate(capture handoff) failed: kr={allocate}"
        ));
    }
    // SAFETY: the newly allocated receive right is valid; MAKE_SEND adds
    // the send right copied through the exec transport.
    let insert =
        unsafe { mach_port_insert_right(self_task(), port, port, MACH_MSG_TYPE_MAKE_SEND) };
    if insert != 0 {
        // SAFETY: `port` is still owned exclusively by this function.
        unsafe {
            mach_port_destroy(self_task(), port);
        }
        return Err(format!(
            "mach_port_insert_right(capture handoff) failed: kr={insert}"
        ));
    }
    Ok(OwnedReceiveRight::new(port))
}

fn message_size<T>() -> u32 {
    u32::try_from(std::mem::size_of::<T>()).expect("Mach handoff message fits mach_msg_size_t")
}

fn send_one_port(remote: mach_port_t, port: mach_port_t) -> Result<(), String> {
    let mut message = OnePortMessage {
        header: mach_msg_header_t {
            msgh_bits: MACH_MSGH_BITS(MACH_MSG_TYPE_COPY_SEND, 0) | MACH_MSGH_BITS_COMPLEX,
            msgh_size: message_size::<OnePortMessage>(),
            msgh_remote_port: remote,
            msgh_local_port: MACH_PORT_NULL,
            msgh_voucher_port: MACH_PORT_NULL,
            msgh_id: CAPABILITY_HANDSHAKE_MESSAGE_ID,
        },
        body: mach_msg_body_t {
            msgh_descriptor_count: 1,
        },
        port: mach_msg_port_descriptor_t::new(port, MACH_MSG_TYPE_COPY_SEND),
        version: CAPABILITY_WIRE_VERSION,
        port_count: 1,
    };
    // SAFETY: `message` is a fully initialized complex Mach message and all
    // embedded rights are borrowed with COPY_SEND disposition.
    let kr = unsafe {
        mach_msg(
            &raw mut message.header,
            MACH_SEND_MSG | MACH_SEND_TIMEOUT,
            message.header.msgh_size,
            0,
            MACH_PORT_NULL,
            CAPABILITY_HANDOFF_TIMEOUT_MS,
            MACH_PORT_NULL,
        )
    };
    if kr == MACH_MSG_SUCCESS {
        Ok(())
    } else {
        Err(format!(
            "capture capability handshake send failed: kr={kr:#x}"
        ))
    }
}

fn send_capabilities(
    remote: mach_port_t,
    task: mach_port_t,
    crashed_thread: Option<mach_port_t>,
    timeout_ms: u32,
) -> Result<(), String> {
    let mut ports = [
        mach_msg_port_descriptor_t::new(task, MACH_MSG_TYPE_COPY_SEND),
        mach_msg_port_descriptor_t::default(),
    ];
    let port_count = if let Some(thread) = crashed_thread {
        ports[1] = mach_msg_port_descriptor_t::new(thread, MACH_MSG_TYPE_COPY_SEND);
        2
    } else {
        1
    };
    let mut message = TwoPortMessage {
        header: mach_msg_header_t {
            msgh_bits: MACH_MSGH_BITS(MACH_MSG_TYPE_COPY_SEND, 0) | MACH_MSGH_BITS_COMPLEX,
            msgh_size: message_size::<TwoPortMessage>(),
            msgh_remote_port: remote,
            msgh_local_port: MACH_PORT_NULL,
            msgh_voucher_port: MACH_PORT_NULL,
            msgh_id: CAPABILITY_TRANSFER_MESSAGE_ID,
        },
        body: mach_msg_body_t {
            msgh_descriptor_count: port_count,
        },
        ports,
        version: CAPABILITY_WIRE_VERSION,
        port_count,
    };
    // SAFETY: the initialized descriptor count matches the number of valid
    // COPY_SEND descriptors. The task/thread rights remain owned by the parent.
    let kr = unsafe {
        mach_msg(
            &raw mut message.header,
            MACH_SEND_MSG | MACH_SEND_TIMEOUT,
            message.header.msgh_size,
            0,
            MACH_PORT_NULL,
            timeout_ms,
            MACH_PORT_NULL,
        )
    };
    if kr == MACH_MSG_SUCCESS {
        Ok(())
    } else {
        Err(format!("capture capability transfer failed: kr={kr:#x}"))
    }
}

fn receive_message(receive_port: mach_port_t, timeout_ms: u32) -> Result<ReceiveBuffer, String> {
    let mut buffer = ReceiveBuffer::default();
    let receive_size = message_size::<ReceiveBuffer>();
    let header = buffer.0.as_mut_ptr().cast::<mach_msg_header_t>();
    // SAFETY: the zeroed, 8-byte-aligned buffer is writable for `receive_size`
    // bytes and remains live for the duration of the bounded receive.
    let kr = unsafe {
        mach_msg(
            header,
            MACH_RCV_MSG | MACH_RCV_TIMEOUT,
            0,
            receive_size,
            receive_port,
            timeout_ms,
            MACH_PORT_NULL,
        )
    };
    if kr == MACH_MSG_SUCCESS {
        Ok(buffer)
    } else {
        Err(format!("capture capability receive failed: kr={kr:#x}"))
    }
}

fn destroy_received_message(buffer: &mut ReceiveBuffer) {
    // SAFETY: this is called only after a successful receive. The kernel wrote
    // a valid message header and descriptors into the buffer.
    unsafe {
        mach_msg_destroy(buffer.0.as_mut_ptr().cast::<mach_msg_header_t>());
    }
}

fn receive_one_port(receive_port: mach_port_t, timeout_ms: u32) -> Result<OwnedSendRight, String> {
    let mut buffer = receive_message(receive_port, timeout_ms)?;
    // SAFETY: the buffer is aligned and fully initialized. Validation below
    // precedes taking ownership of the received descriptor.
    let message = unsafe { &mut *buffer.0.as_mut_ptr().cast::<OnePortMessage>() };
    let valid = message.header.msgh_id == CAPABILITY_HANDSHAKE_MESSAGE_ID
        && message.header.msgh_size >= message_size::<OnePortMessage>()
        && message.header.msgh_bits & MACH_MSGH_BITS_COMPLEX != 0
        && message.body.msgh_descriptor_count == 1
        && message.port.type_ == MACH_MSG_PORT_DESCRIPTOR as u8
        && message.version == CAPABILITY_WIRE_VERSION
        && message.port_count == 1
        && message.port.name != MACH_PORT_NULL;
    if !valid {
        destroy_received_message(&mut buffer);
        return Err("invalid capture capability handshake message".into());
    }
    Ok(OwnedSendRight::new(message.port.name))
}

fn receive_capabilities(
    receive_port: mach_port_t,
    expect_crashed_thread: bool,
) -> Result<(OwnedTaskPort, Option<OwnedThreadPort>), String> {
    let mut buffer = receive_message(receive_port, CAPABILITY_HANDOFF_TIMEOUT_MS)?;
    // SAFETY: the buffer is aligned and fully initialized. Descriptor count,
    // message size, and port names are validated before ownership is created.
    let message = unsafe { &mut *buffer.0.as_mut_ptr().cast::<TwoPortMessage>() };
    let expected_count = if expect_crashed_thread { 2 } else { 1 };
    let valid = message.header.msgh_id == CAPABILITY_TRANSFER_MESSAGE_ID
        && message.header.msgh_size >= message_size::<TwoPortMessage>()
        && message.header.msgh_bits & MACH_MSGH_BITS_COMPLEX != 0
        && message.body.msgh_descriptor_count == expected_count
        && message.port_count == expected_count
        && message.version == CAPABILITY_WIRE_VERSION
        && message.ports[0].type_ == MACH_MSG_PORT_DESCRIPTOR as u8
        && message.ports[0].name != MACH_PORT_NULL
        && (!expect_crashed_thread
            || (message.ports[1].type_ == MACH_MSG_PORT_DESCRIPTOR as u8
                && message.ports[1].name != MACH_PORT_NULL));
    if !valid {
        destroy_received_message(&mut buffer);
        return Err("invalid capture capability transfer message".into());
    }
    let task = OwnedTaskPort::new(message.ports[0].name);
    let crashed_thread = expect_crashed_thread.then(|| OwnedThreadPort::new(message.ports[1].name));
    Ok((task, crashed_thread))
}

fn validate_capture_helper_spawn(
    executable: &Path,
    request_json: &str,
    result_fd: RawFd,
    task: mach_port_t,
    crashed_thread: Option<mach_port_t>,
) -> Result<(), String> {
    if executable.as_os_str().is_empty() {
        return Err("capture-helper executable path is empty".to_string());
    }
    if executable.as_os_str().as_bytes().contains(&0) {
        return Err("capture-helper executable path contains a null byte".to_string());
    }
    if request_json.is_empty() {
        return Err("capture-helper request JSON is empty".to_string());
    }
    if request_json.len() > MAX_CAPTURE_HELPER_REQUEST_BYTES {
        return Err(format!(
            "capture-helper request exceeds {MAX_CAPTURE_HELPER_REQUEST_BYTES} bytes"
        ));
    }
    if request_json.as_bytes().contains(&0) {
        return Err("capture-helper request JSON contains a null byte".to_string());
    }
    if result_fd < 0 {
        return Err("capture-helper result descriptor is invalid".to_string());
    }
    if task == MACH_PORT_NULL {
        return Err("capture-helper target task port is null".to_string());
    }
    if crashed_thread == Some(MACH_PORT_NULL) {
        return Err("capture-helper crashed-thread port is null".to_string());
    }
    Ok(())
}

fn cstring_from_os_str(value: &OsStr, description: &str) -> Result<CString, String> {
    CString::new(value.as_bytes()).map_err(|_| format!("{description} contains a null byte"))
}

fn inherited_environment() -> Result<(Vec<CString>, Vec<*mut libc::c_char>), String> {
    let mut entries = Vec::new();
    for (key, value) in std::env::vars_os() {
        let mut entry = Vec::with_capacity(key.as_bytes().len() + value.as_bytes().len() + 1);
        entry.extend_from_slice(key.as_bytes());
        entry.push(b'=');
        entry.extend_from_slice(value.as_bytes());
        entries.push(
            CString::new(entry)
                .map_err(|_| "capture-helper environment contains a null byte".to_string())?,
        );
    }
    let mut pointers: Vec<*mut libc::c_char> = entries
        .iter()
        .map(|entry| entry.as_ptr().cast_mut())
        .collect();
    pointers.push(std::ptr::null_mut());
    Ok((entries, pointers))
}

fn duplicate_result_fd(result_fd: RawFd) -> Result<OwnedFd, String> {
    // SAFETY: `result_fd` is borrowed and validated. F_DUPFD_CLOEXEC creates a
    // distinct owned descriptor, which is immediately placed under RAII.
    let duplicated = unsafe { libc::fcntl(result_fd, libc::F_DUPFD_CLOEXEC, FIRST_TEMPORARY_FD) };
    if duplicated < 0 {
        return Err(format!(
            "failed to duplicate capture-helper result descriptor: {}",
            std::io::Error::last_os_error()
        ));
    }
    // SAFETY: `fcntl(F_DUPFD_CLOEXEC)` returned a new uniquely owned fd.
    Ok(unsafe { OwnedFd::from_raw_fd(duplicated) })
}

/// Spawn the current executable's hidden `capture-helper` command.
///
/// An ordinary handoff send right is installed in an otherwise-unused
/// exception-handler slot. After `exec`, the helper exchanges a private receive
/// port over that channel and the parent transfers the protected task/thread
/// rights in a bounded Mach message. The result file is made available as
/// descriptor 3 across `exec`. `POSIX_SPAWN_CLOEXEC_DEFAULT` closes every
/// descriptor not explicitly produced by the file-action list, so descriptor 3
/// is the helper's only inherited file descriptor.
///
/// # Errors
/// Returns an error before a child exists when validation, descriptor setup,
/// spawn-attribute setup, or `posix_spawn` fails.
pub fn spawn_capture_helper(
    executable: &Path,
    request_json: &str,
    result_file: &File,
    task: mach_port_t,
    crashed_thread: Option<mach_port_t>,
    handoff_timeout: Duration,
) -> Result<CaptureHelperProcess, CaptureHelperSpawnError> {
    let result_fd = result_file.as_raw_fd();
    validate_capture_helper_spawn(executable, request_json, result_fd, task, crashed_thread)?;

    let executable = cstring_from_os_str(executable.as_os_str(), "capture-helper executable path")?;
    let subcommand = c"capture-helper";
    let request_arg = c"--request-json";
    let request = CString::new(request_json)
        .map_err(|_| "capture-helper request JSON contains a null byte".to_string())?;
    let argv = [
        executable.as_c_str(),
        subcommand,
        request_arg,
        request.as_c_str(),
    ];
    let mut argv_pointers: Vec<*mut libc::c_char> = argv
        .iter()
        .map(|argument| argument.as_ptr().cast_mut())
        .collect();
    argv_pointers.push(std::ptr::null_mut());

    let (_environment, environment_pointers) = inherited_environment()?;
    let duplicated_result = duplicate_result_fd(result_fd)?;
    let duplicated_result_fd = duplicated_result.as_raw_fd();
    let handoff = allocate_receive_right()?;

    let mut file_actions = SpawnFileActions::new()?;
    file_actions.add_dup2(duplicated_result_fd, CAPTURE_HELPER_RESULT_FD)?;
    file_actions.add_close(duplicated_result_fd)?;

    let mut attributes = SpawnAttributes::new()?;
    let close_by_default = libc::c_short::try_from(libc::POSIX_SPAWN_CLOEXEC_DEFAULT)
        .map_err(|_| "Darwin POSIX_SPAWN_CLOEXEC_DEFAULT does not fit c_short".to_string())?;
    attributes.set_flags(close_by_default)?;
    attributes.set_transport_port(EXC_MASK_RPC_ALERT, handoff.raw())?;

    let mut child_pid = 0;
    // SAFETY: every C string and pointer array remains live and terminated for
    // the duration of the call. File actions and attributes are initialized,
    // and `child_pid` is a valid out-parameter.
    let rc = unsafe {
        libc::posix_spawn(
            &raw mut child_pid,
            executable.as_ptr(),
            file_actions.as_ptr(),
            attributes.as_ptr(),
            argv_pointers.as_ptr(),
            environment_pointers.as_ptr(),
        )
    };
    if rc != 0 {
        return Err(errno_style_error("posix_spawn(capture-helper)", rc).into());
    }
    if child_pid <= 0 {
        return Err(CaptureHelperSpawnError::new(
            format!("posix_spawn(capture-helper) returned invalid child pid {child_pid}"),
            true,
        ));
    }
    let mut child = CaptureHelperProcess::new(Pid::from_raw(child_pid));
    let handoff_deadline = Instant::now() + handoff_timeout.min(Duration::from_millis(1_000));
    let transfer = receive_one_port(handoff.raw(), timeout_millis_until(handoff_deadline))
        .and_then(|control| {
            send_capabilities(
                control.raw(),
                task,
                crashed_thread,
                timeout_millis_until(handoff_deadline),
            )
        });
    if let Err(error) = transfer {
        let cleanup = kill_and_reap_failed_handoff(&mut child);
        return Err(match cleanup {
            Ok(()) => CaptureHelperSpawnError::new(
                format!("capture-helper capability handoff failed: {error}"),
                false,
            ),
            Err(cleanup_error) => CaptureHelperSpawnError::new(
                format!(
                    "capture-helper capability handoff failed: {error}; cleanup failed: {cleanup_error}"
                ),
                true,
            ),
        });
    }
    Ok(child)
}

fn timeout_millis_until(deadline: Instant) -> u32 {
    let millis = deadline
        .saturating_duration_since(Instant::now())
        .as_millis()
        .clamp(1, u128::from(u32::MAX));
    u32::try_from(millis).expect("clamped handoff timeout fits u32")
}

fn kill_and_reap_failed_handoff(child: &mut CaptureHelperProcess) -> Result<(), String> {
    if let Err(error) = nix::sys::signal::kill(child.pid(), nix::sys::signal::Signal::SIGKILL)
        && error != nix::errno::Errno::ESRCH
    {
        return Err(format!("cannot kill helper {}: {error}", child.pid()));
    }
    let deadline = Instant::now() + FAILED_HANDOFF_REAP_GRACE;
    loop {
        match child.poll_reap()? {
            CaptureHelperReap::StillRunning => {}
            CaptureHelperReap::Exited(_) | CaptureHelperReap::Signaled { .. } => return Ok(()),
            CaptureHelperReap::OwnershipLost => {
                return Err(format!(
                    "capture helper {} wait ownership was lost (ECHILD)",
                    child.pid()
                ));
            }
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "helper {} was not reaped after SIGKILL",
                child.pid()
            ));
        }
        std::thread::sleep(FAILED_HANDOFF_POLL_INTERVAL);
    }
}

fn inherited_transport_port(
    mask: exception_mask_t,
    description: &str,
) -> Result<OwnedSendRight, String> {
    let mut masks = [0 as exception_mask_t; 1];
    let mut handlers = [MACH_PORT_NULL as exception_handler_t; 1];
    let mut behaviors = [0; 1];
    let mut flavors = [0; 1];
    let mut count = 1;
    // SAFETY: every output array has capacity `count == 1`. Requesting one
    // exact mask returns a copied send right in `handlers[0]` on success.
    let kr = unsafe {
        task_get_exception_ports(
            self_task(),
            mask,
            masks.as_mut_ptr(),
            &raw mut count,
            handlers.as_mut_ptr(),
            behaviors.as_mut_ptr(),
            flavors.as_mut_ptr(),
        )
    };
    if kr != 0 {
        return Err(format!(
            "task_get_exception_ports({description}) failed: kr={kr}"
        ));
    }
    if count != 1 || masks[0] & mask == 0 {
        return Err(format!(
            "task_get_exception_ports({description}) returned no matching handler"
        ));
    }
    let port = handlers[0];
    if port == MACH_PORT_NULL {
        return Err(format!(
            "task_get_exception_ports({description}) returned a null port"
        ));
    }
    let port = OwnedSendRight::new(port);
    // Clear the temporary exception transport before doing collector work, so
    // a later helper exception cannot be routed to the short-lived handoff
    // channel.
    // SAFETY: this mutates only the current helper's selected exception slot.
    let clear = unsafe {
        task_set_exception_ports(
            self_task(),
            mask,
            MACH_PORT_NULL,
            EXCEPTION_DEFAULT as libc::c_int,
            0,
        )
    };
    if clear != 0 {
        return Err(format!(
            "task_set_exception_ports({description}, clear) failed: kr={clear}"
        ));
    }
    Ok(port)
}

/// Recover the target capabilities installed by [`spawn_capture_helper`].
///
/// The returned RAII owners release the received send rights when the helper
/// exits or finishes capture.
///
/// # Errors
/// Returns an error if the handoff channel or a required transferred right is
/// unavailable, malformed, or times out.
pub fn inherited_capture_ports(
    expect_crashed_thread: bool,
) -> Result<(OwnedTaskPort, Option<OwnedThreadPort>), String> {
    let parent_handoff = inherited_transport_port(EXC_MASK_RPC_ALERT, "EXC_MASK_RPC_ALERT")?;
    let control = allocate_receive_right()?;
    send_one_port(parent_handoff.raw(), control.raw())?;
    receive_capabilities(control.raw(), expect_crashed_thread)
}

/// Take unique ownership of the helper result descriptor installed by
/// [`spawn_capture_helper`].
///
/// This function is intended to be called exactly once by the hidden helper
/// command. The returned [`File`] closes descriptor 3 on drop. Repeated or
/// concurrent calls fail rather than constructing multiple Rust owners for the
/// same descriptor.
///
/// # Errors
/// Returns an error if descriptor 3 is absent, read-only, or was already taken.
pub fn capture_result_file() -> Result<File, String> {
    // SAFETY: F_GETFL only inspects the descriptor table and does not consume
    // or mutate ownership of the fixed descriptor.
    let flags = unsafe { libc::fcntl(CAPTURE_HELPER_RESULT_FD, libc::F_GETFL) };
    if flags < 0 {
        return Err(format!(
            "capture-helper result descriptor {} is unavailable: {}",
            CAPTURE_HELPER_RESULT_FD,
            std::io::Error::last_os_error()
        ));
    }
    if flags & libc::O_ACCMODE == libc::O_RDONLY {
        return Err(format!(
            "capture-helper result descriptor {CAPTURE_HELPER_RESULT_FD} is read-only"
        ));
    }
    CAPTURE_HELPER_RESULT_TAKEN
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map_err(|_| "capture-helper result descriptor was already taken".to_string())?;

    // SAFETY: posix_spawn installed this raw descriptor without creating a
    // Rust owner. The atomic transition above makes this the unique owner.
    Ok(unsafe { File::from_raw_fd(CAPTURE_HELPER_RESULT_FD) })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_validation_accepts_descriptor_three() {
        assert!(
            validate_capture_helper_spawn(
                Path::new("/tmp/crash-monitor"),
                "{}",
                CAPTURE_HELPER_RESULT_FD,
                42,
                Some(43),
            )
            .is_ok()
        );
    }

    #[test]
    fn spawn_validation_rejects_null_capabilities() {
        let null_task = validate_capture_helper_spawn(
            Path::new("/tmp/crash-monitor"),
            "{}",
            4,
            MACH_PORT_NULL,
            None,
        );
        assert!(null_task.unwrap_err().contains("target task port is null"));

        let null_thread = validate_capture_helper_spawn(
            Path::new("/tmp/crash-monitor"),
            "{}",
            4,
            42,
            Some(MACH_PORT_NULL),
        );
        assert!(
            null_thread
                .unwrap_err()
                .contains("crashed-thread port is null")
        );
    }

    #[test]
    fn spawn_validation_bounds_request_and_rejects_bad_inputs() {
        let path = Path::new("/tmp/crash-monitor");
        assert!(
            validate_capture_helper_spawn(path, "", 4, 42, None)
                .unwrap_err()
                .contains("request JSON is empty")
        );
        assert!(
            validate_capture_helper_spawn(
                path,
                &"x".repeat(MAX_CAPTURE_HELPER_REQUEST_BYTES + 1),
                4,
                42,
                None,
            )
            .unwrap_err()
            .contains("request exceeds")
        );
        assert!(
            validate_capture_helper_spawn(path, "{}", -1, 42, None)
                .unwrap_err()
                .contains("descriptor is invalid")
        );
    }

    #[test]
    fn echild_is_ownership_loss_for_every_reap_owner_path() {
        for path in ["normal completion", "timeout cleanup", "handoff cleanup"] {
            let mut helper = CaptureHelperProcess::new(Pid::from_raw(41));
            let result = helper
                .poll_reap_with(|_| Err(nix::errno::Errno::ECHILD))
                .unwrap();
            assert_eq!(
                result,
                CaptureHelperReap::OwnershipLost,
                "{path} must fail closed when wait ownership is lost"
            );
            assert_eq!(
                helper
                    .poll_reap_with(|_| panic!("terminal ownership must not be polled twice"))
                    .unwrap(),
                CaptureHelperReap::OwnershipLost
            );
        }
    }

    #[test]
    fn terminal_status_is_consumed_exactly_once() {
        let pid = Pid::from_raw(42);
        let mut helper = CaptureHelperProcess::new(pid);
        assert_eq!(
            helper
                .poll_reap_with(|_| Ok(WaitStatus::Exited(pid, 0)))
                .unwrap(),
            CaptureHelperReap::Exited(0)
        );
        assert_eq!(
            helper
                .poll_reap_with(|_| panic!("waitpid must not run after terminal consumption"))
                .unwrap(),
            CaptureHelperReap::OwnershipLost
        );
    }
}
