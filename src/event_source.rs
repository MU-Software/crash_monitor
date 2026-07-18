//! macOS event sources ("triggers") that feed the monitor's event loop.
//!
//! `MacEventSource` multiplexes the three OS-level trigger channels into the
//! platform-agnostic `MonitorEvent` stream consumed by `event_loop`:
//! - **Mach exception port** → `Crash`
//! - **SIGUSR1 pipe** (F8 manual snapshot) → `Snapshot`
//! - **`waitpid`** → one lossless `ChildTerminated(TerminationReason)` event
//!
//! The pure `MonitorEvent` → `CrashEvent` mapping (plus ANR/OOM decisions) lives
//! in `event_loop` and is unit-tested via `TestEventSource`; this module owns the
//! untestable OS wiring (signal handler, pipe, `waitpid`).

use nix::fcntl::{FcntlArg, FdFlag, OFlag, fcntl};
use nix::libc;
use nix::sys::event::{EvFlags, EventFilter, FilterFlag, KEvent, Kqueue};
use nix::sys::signal::{self, SaFlags, SigAction, SigHandler, SigSet};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::event_loop::{EventSource, MonitorEvent};
use crate::pipeline::TerminationReason;
use crate::platform;

// ═══════════════════════════════════════════════════
//  SIGUSR1 signal pipe for manual snapshots
// ═══════════════════════════════════════════════════

static SIGNAL_PIPE_WRITE: AtomicI32 = AtomicI32::new(-1);

extern "C" fn sigusr1_handler(_sig: libc::c_int) {
    // SAFETY: libc::write is async-signal-safe (POSIX requirement).
    // No safe alternative exists for writes inside signal handlers.
    let fd = SIGNAL_PIPE_WRITE.load(Ordering::Acquire);
    if fd >= 0 {
        unsafe {
            libc::write(fd, std::ptr::from_ref::<u8>(&1u8).cast::<libc::c_void>(), 1);
        }
    }
}

/// Keep write end alive so it doesn't get closed by RAII.
/// `OnceLock` ensures thread-safe single initialization (replaces `static mut`).
static SIGNAL_PIPE_WRITE_FD: OnceLock<OwnedFd> = OnceLock::new();

/// Install the SIGUSR1 handler and return the non-blocking read end of the
/// signal pipe. The read end is polled by `MacEventSource::poll`.
///
/// # Errors
/// Returns an error if the pipe, `fcntl`, or `sigaction` calls fail.
pub fn setup_signal_pipe() -> Result<OwnedFd, String> {
    // nix::unistd::pipe: safe pipe creation returning OwnedFd (RAII)
    let (read_fd, write_fd) = unistd::pipe().map_err(|e| format!("pipe failed: {e}"))?;

    // Set read end to non-blocking via nix::fcntl (safe)
    let flags =
        fcntl(&read_fd, FcntlArg::F_GETFL).map_err(|e| format!("fcntl F_GETFL failed: {e}"))?;
    let new_flags = OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK;
    fcntl(&read_fd, FcntlArg::F_SETFL(new_flags))
        .map_err(|e| format!("fcntl F_SETFL failed: {e}"))?;

    // Preserve write end first (RAII), then expose fd to signal handler.
    // Order matters: ownership must be taken before the atomic store,
    // otherwise the signal handler could see the fd before it's kept alive.
    let write_raw_fd = write_fd.as_raw_fd();
    let _ = SIGNAL_PIPE_WRITE_FD.set(write_fd);
    SIGNAL_PIPE_WRITE.store(write_raw_fd, Ordering::Release);

    // Install SIGUSR1 handler (AFTER fd is set up)
    let sa = SigAction::new(
        SigHandler::Handler(sigusr1_handler),
        SaFlags::SA_RESTART,
        SigSet::empty(),
    );
    unsafe {
        signal::sigaction(signal::Signal::SIGUSR1, &sa)
            .map_err(|e| format!("sigaction failed: {e}"))?;
    }

    Ok(read_fd)
}

/// Non-blocking drain of the signal pipe. Returns `true` if a snapshot request
/// (at least one byte written by `sigusr1_handler`) was pending.
fn drain_signal_pipe(read_fd: &OwnedFd) -> bool {
    let mut buf = [0u8; 16];
    match unistd::read(read_fd, &mut buf) {
        Ok(n) => n > 0,
        Err(_) => false, // EAGAIN (non-blocking, no data)
    }
}

// ═══════════════════════════════════════════════════
//  MacEventSource — real OS event sources
// ═══════════════════════════════════════════════════

pub struct MacEventSource {
    exc_rx: mpsc::Receiver<platform::ExceptionListenerEvent>,
    exception_wake_fd: OwnedFd,
    signal_read_fd: OwnedFd,
    child_pid: nix::unistd::Pid,
    child_started_at: Instant,
    kqueue: Kqueue,
}

impl MacEventSource {
    /// Assemble the event source from its three OS channels: the Mach exception
    /// receiver, the SIGUSR1 pipe read end, and the child PID for `waitpid`.
    pub fn new(
        exc_rx: mpsc::Receiver<platform::ExceptionListenerEvent>,
        signal_read_fd: OwnedFd,
        child_pid: nix::unistd::Pid,
        child_started_at: Instant,
    ) -> Result<Self, String> {
        let (exc_rx, exception_wake_fd) = bridge_exception_listener(exc_rx)?;
        let kqueue = Kqueue::new().map_err(|error| format!("kqueue failed: {error}"))?;
        let changes = [
            KEvent::new(
                signal_read_fd.as_raw_fd() as usize,
                EventFilter::EVFILT_READ,
                EvFlags::EV_ADD | EvFlags::EV_ENABLE | EvFlags::EV_CLEAR,
                FilterFlag::empty(),
                0,
                0,
            ),
            KEvent::new(
                exception_wake_fd.as_raw_fd() as usize,
                EventFilter::EVFILT_READ,
                EvFlags::EV_ADD | EvFlags::EV_ENABLE | EvFlags::EV_CLEAR,
                FilterFlag::empty(),
                0,
                0,
            ),
            KEvent::new(
                child_pid.as_raw() as usize,
                EventFilter::EVFILT_PROC,
                EvFlags::EV_ADD | EvFlags::EV_ENABLE | EvFlags::EV_ONESHOT,
                FilterFlag::NOTE_EXIT,
                0,
                0,
            ),
        ];
        let mut no_events = [];
        kqueue
            .kevent(&changes, &mut no_events, Some(libc::timespec::default()))
            .map_err(|error| format!("registering kqueue event sources failed: {error}"))?;

        Ok(Self {
            exc_rx,
            exception_wake_fd,
            signal_read_fd,
            child_pid,
            child_started_at,
            kqueue,
        })
    }
}

/// Turn the standard listener channel into a file-descriptor wakeup that can
/// participate in the same kqueue as process and signal events.
fn bridge_exception_listener(
    incoming: mpsc::Receiver<platform::ExceptionListenerEvent>,
) -> Result<(mpsc::Receiver<platform::ExceptionListenerEvent>, OwnedFd), String> {
    let (wake_read, wake_write) = nonblocking_cloexec_pipe("exception wake pipe")?;
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("exception-event-wakeup".to_string())
        .spawn(move || {
            while let Ok(event) = incoming.recv() {
                if tx.send(event).is_err() {
                    return;
                }
                wake_fd(&wake_write);
            }
            // Wake the supervisor so a disconnected listener is observed
            // immediately rather than only at a watchdog deadline.
            wake_fd(&wake_write);
        })
        .map_err(|error| format!("spawning exception wake bridge failed: {error}"))?;
    Ok((rx, wake_read))
}

fn nonblocking_cloexec_pipe(context: &str) -> Result<(OwnedFd, OwnedFd), String> {
    let (read_fd, write_fd) =
        unistd::pipe().map_err(|error| format!("{context} failed: {error}"))?;
    for fd in [&read_fd, &write_fd] {
        fcntl(fd, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC))
            .map_err(|error| format!("{context} CLOEXEC failed: {error}"))?;
        let flags = fcntl(fd, FcntlArg::F_GETFL)
            .map_err(|error| format!("{context} F_GETFL failed: {error}"))?;
        fcntl(
            fd,
            FcntlArg::F_SETFL(OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK),
        )
        .map_err(|error| format!("{context} O_NONBLOCK failed: {error}"))?;
    }
    Ok((read_fd, write_fd))
}

fn wake_fd(fd: &OwnedFd) {
    match unistd::write(fd, &[1]) {
        Ok(_) | Err(nix::errno::Errno::EAGAIN) => {}
        Err(error) => eprintln!("[monitor] exception wake pipe write failed: {error}"),
    }
}

enum ExceptionListenerPoll {
    Exception(platform::ExceptionInfo),
    Failure(String),
    Empty,
}

fn poll_exception_listener(
    receiver: &std::sync::mpsc::Receiver<platform::ExceptionListenerEvent>,
) -> ExceptionListenerPoll {
    match receiver.try_recv() {
        Ok(platform::ExceptionListenerEvent::Exception(info)) => {
            ExceptionListenerPoll::Exception(info)
        }
        Ok(platform::ExceptionListenerEvent::Fatal { message }) => {
            ExceptionListenerPoll::Failure(message)
        }
        Err(std::sync::mpsc::TryRecvError::Empty) => ExceptionListenerPoll::Empty,
        Err(std::sync::mpsc::TryRecvError::Disconnected) => ExceptionListenerPoll::Failure(
            "Mach exception listener disconnected without a terminal event".to_string(),
        ),
    }
}

fn prioritize_ready_events(
    child: Option<MonitorEvent>,
    listener: Option<MonitorEvent>,
) -> Option<MonitorEvent> {
    child.or(listener)
}

/// Normalize every terminal `WaitStatus` without losing signal/core metadata.
/// Non-terminal statuses deliberately return `None`.
#[must_use]
pub fn termination_from_wait_status(
    status: WaitStatus,
    runtime: Duration,
) -> Option<TerminationReason> {
    let runtime_ms = u64::try_from(runtime.as_millis()).unwrap_or(u64::MAX);
    match status {
        WaitStatus::Exited(_, exit_code) => Some(TerminationReason::Exited {
            exit_code,
            runtime_ms,
        }),
        WaitStatus::Signaled(_, signal, core_dumped) => Some(TerminationReason::Signaled {
            signal: signal as i32,
            core_dumped,
            runtime_ms,
        }),
        _ => None,
    }
}

impl EventSource for MacEventSource {
    fn poll(&mut self) -> Option<MonitorEvent> {
        drain_wake_fd(&self.exception_wake_fd);
        // Check for Mach exception (crash)
        let listener_event = match poll_exception_listener(&self.exc_rx) {
            ExceptionListenerPoll::Exception(exc_info) => {
                eprintln!(
                    "[monitor] Crash detected: {} (code={:#x}, subcode={:#x})",
                    platform::exception_type_name(exc_info.exception_type),
                    exc_info.code,
                    exc_info.subcode
                );
                Some(MonitorEvent::Crash {
                    received_at: exc_info.received_at,
                    exception_type: exc_info.exception_type,
                    code: exc_info.code,
                    subcode: exc_info.subcode,
                    raw_codes: exc_info.raw_codes,
                    request: exc_info.request,
                })
            }
            ExceptionListenerPoll::Failure(message) => {
                Some(MonitorEvent::MonitorFailure { message })
            }
            ExceptionListenerPoll::Empty => None,
        };

        // ANR detection is now handled inline by event_loop (no dedicated thread)

        // A terminal wait status wins over lower-priority snapshot/listener
        // events so a dead task can never enter the live-task capture path.
        let child_event = loop {
            match waitpid(self.child_pid, Some(WaitPidFlag::WNOHANG)) {
                Ok(status) => {
                    if let Some(reason) =
                        termination_from_wait_status(status, self.child_started_at.elapsed())
                    {
                        eprintln!("[monitor] Child terminated: {reason:?}.");
                        break Some(MonitorEvent::ChildTerminated(reason));
                    }
                    break None;
                }
                Err(nix::errno::Errno::EINTR) => {}
                Err(nix::errno::Errno::ECHILD) => {
                    break Some(MonitorEvent::MonitorFailure {
                        message: "waitpid lost ownership of the child (ECHILD)".to_string(),
                    });
                }
                Err(e) => {
                    break Some(MonitorEvent::MonitorFailure {
                        message: format!("waitpid failed: {e}"),
                    });
                }
            }
        };

        if let Some(event) = prioritize_ready_events(child_event, listener_event) {
            return Some(event);
        }

        // Check for SIGUSR1 (manual snapshot) only while the child is alive.
        if drain_signal_pipe(&self.signal_read_fd) {
            eprintln!("[monitor] Manual snapshot requested (SIGUSR1)");
            return Some(MonitorEvent::Snapshot);
        }

        None
    }

    fn wait_until(&mut self, deadline: Option<Instant>) -> Option<MonitorEvent> {
        if let Some(event) = self.poll() {
            return Some(event);
        }

        loop {
            let timeout = deadline.map(|deadline| {
                duration_to_timespec(deadline.saturating_duration_since(Instant::now()))
            });
            let placeholder = KEvent::new(
                self.exception_wake_fd.as_raw_fd() as usize,
                EventFilter::EVFILT_READ,
                EvFlags::empty(),
                FilterFlag::empty(),
                0,
                0,
            );
            let mut events = [placeholder; 3];
            match self.kqueue.kevent(&[], &mut events, timeout) {
                Ok(0) => return None,
                Ok(_) => return self.poll(),
                Err(nix::errno::Errno::EINTR) => {
                    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                        return None;
                    }
                }
                Err(error) => {
                    return Some(MonitorEvent::MonitorFailure {
                        message: format!("kqueue wait failed: {error}"),
                    });
                }
            }
        }
    }
}

fn drain_wake_fd(fd: &OwnedFd) {
    let mut buffer = [0_u8; 64];
    loop {
        match unistd::read(fd, &mut buffer) {
            Ok(0) | Err(nix::errno::Errno::EAGAIN) => return,
            Ok(_) | Err(nix::errno::Errno::EINTR) => {}
            Err(error) => {
                eprintln!("[monitor] exception wake pipe drain failed: {error}");
                return;
            }
        }
    }
}

fn duration_to_timespec(duration: Duration) -> libc::timespec {
    libc::timespec {
        tv_sec: duration.as_secs().try_into().unwrap_or(libc::time_t::MAX),
        tv_nsec: duration.subsec_nanos().into(),
    }
}

#[cfg(test)]
#[path = "../tests/unit/event_source_tests.rs"]
mod tests;
