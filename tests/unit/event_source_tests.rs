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
