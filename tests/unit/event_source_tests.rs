//! Unit tests for the signal-pipe plumbing in `event_source`.
//!
//! The Mach/`waitpid` channels need real OS resources and are covered
//! end-to-end by the E2E suite; here we test the pure, side-effect-free
//! `drain_signal_pipe` behaviour on a plain non-blocking pipe.

use super::*;

/// Create a pipe and set its read end non-blocking, matching how
/// `setup_signal_pipe` configures the real signal pipe.
fn nonblocking_pipe() -> (OwnedFd, OwnedFd) {
    let (read_fd, write_fd) = unistd::pipe().unwrap();
    let flags = fcntl(&read_fd, FcntlArg::F_GETFL).unwrap();
    let nb = OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK;
    fcntl(&read_fd, FcntlArg::F_SETFL(nb)).unwrap();
    (read_fd, write_fd)
}

#[test]
fn drain_reports_false_on_empty_pipe() {
    let (read_fd, _write_fd) = nonblocking_pipe();
    assert!(!drain_signal_pipe(&read_fd));
}

#[test]
fn drain_reports_true_after_write_then_false_again() {
    let (read_fd, write_fd) = nonblocking_pipe();

    // A byte written (as sigusr1_handler would) is drained as a snapshot request.
    unistd::write(&write_fd, &[1u8]).unwrap();
    assert!(drain_signal_pipe(&read_fd));

    // Once drained, the pipe is empty again.
    assert!(!drain_signal_pipe(&read_fd));
}

#[test]
fn terminal_wait_status_preserves_exit_code_and_runtime() {
    let reason = termination_from_wait_status(
        WaitStatus::Exited(nix::unistd::Pid::from_raw(42), 23),
        Duration::from_millis(456),
    );
    assert_eq!(
        reason,
        Some(TerminationReason::Exited {
            exit_code: 23,
            runtime_ms: 456,
        })
    );
}

#[test]
fn terminal_wait_status_preserves_signal_core_and_runtime() {
    let reason = termination_from_wait_status(
        WaitStatus::Signaled(
            nix::unistd::Pid::from_raw(42),
            nix::sys::signal::Signal::SIGABRT,
            true,
        ),
        Duration::from_millis(789),
    );
    assert_eq!(
        reason,
        Some(TerminationReason::Signaled {
            signal: 6,
            core_dumped: true,
            runtime_ms: 789,
        })
    );
}

#[test]
fn nonterminal_wait_status_is_not_fabricated_into_a_termination() {
    assert_eq!(
        termination_from_wait_status(WaitStatus::StillAlive, Duration::ZERO),
        None
    );
}

#[test]
fn fatal_listener_error_is_preserved_for_the_supervisor() {
    let (tx, rx) = std::sync::mpsc::channel();
    tx.send(platform::ExceptionListenerEvent::Fatal {
        message: "mach_msg receive failed: test error".to_string(),
    })
    .unwrap();

    assert!(matches!(
        poll_exception_listener(&rx),
        ExceptionListenerPoll::Failure(message)
            if message == "mach_msg receive failed: test error"
    ));
}

#[test]
fn unexpected_listener_disconnect_is_a_supervisor_failure() {
    let (tx, rx) = std::sync::mpsc::channel::<platform::ExceptionListenerEvent>();
    drop(tx);

    assert!(matches!(
        poll_exception_listener(&rx),
        ExceptionListenerPoll::Failure(message)
            if message.contains("disconnected without a terminal event")
    ));
}
