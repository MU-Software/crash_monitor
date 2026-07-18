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
    assert!(!drain_signal_pipe(&read_fd).unwrap());
}

#[test]
fn drain_reports_true_after_write_then_false_again() {
    let (read_fd, write_fd) = nonblocking_pipe();

    // A byte written (as sigusr1_handler would) is drained as a snapshot request.
    unistd::write(&write_fd, &[1u8]).unwrap();
    assert!(drain_signal_pipe(&read_fd).unwrap());

    // Once drained, the pipe is empty again.
    assert!(!drain_signal_pipe(&read_fd).unwrap());
}

#[test]
fn signal_pipe_is_nonblocking_and_close_on_exec_at_both_ends() {
    let (read_fd, write_fd) = nonblocking_cloexec_pipe("test signal pipe").unwrap();

    for fd in [&read_fd, &write_fd] {
        let status = OFlag::from_bits_truncate(fcntl(fd, FcntlArg::F_GETFL).unwrap());
        let descriptor = FdFlag::from_bits_truncate(fcntl(fd, FcntlArg::F_GETFD).unwrap());
        assert!(status.contains(OFlag::O_NONBLOCK));
        assert!(descriptor.contains(FdFlag::FD_CLOEXEC));
    }
}

#[test]
fn signal_burst_is_fully_drained_and_coalesced_to_one_request() {
    let (read_fd, write_fd) = nonblocking_cloexec_pipe("test signal pipe").unwrap();
    let burst = [1_u8; 512];
    unistd::write(&write_fd, &burst).unwrap();

    assert!(drain_signal_pipe(&read_fd).unwrap());
    assert!(!drain_signal_pipe(&read_fd).unwrap());
}

#[test]
fn full_signal_pipe_never_blocks_the_writer() {
    let (_read_fd, write_fd) = nonblocking_cloexec_pipe("test signal pipe").unwrap();
    let bytes = [1_u8; 1024];
    loop {
        match unistd::write(&write_fd, &bytes) {
            Ok(_) => {}
            Err(nix::errno::Errno::EAGAIN) => break,
            Err(error) => panic!("unexpected pipe fill error: {error}"),
        }
    }
}

#[test]
fn duplicate_signal_pipe_owner_is_reported_without_replacing_the_live_fd() {
    let owner = OnceLock::new();
    let (_first_read, first_write) = nonblocking_pipe();
    let expected_fd = first_write.as_raw_fd();
    assert_eq!(
        install_signal_write_owner(&owner, first_write).unwrap(),
        expected_fd
    );

    let (_second_read, second_write) = nonblocking_pipe();
    assert!(install_signal_write_owner(&owner, second_write).is_err());
    assert_eq!(owner.get().unwrap().as_raw_fd(), expected_fd);
}

#[test]
fn signal_handler_restores_interrupted_errno() {
    let previous = SIGNAL_PIPE_WRITE.swap(-1, Ordering::AcqRel);
    nix::errno::Errno::EBUSY.set();
    sigusr1_handler(libc::SIGUSR1);
    assert_eq!(nix::errno::Errno::last(), nix::errno::Errno::EBUSY);
    SIGNAL_PIPE_WRITE.store(previous, Ordering::Release);
}

#[test]
fn signal_pipe_drain_propagates_non_retryable_errors() {
    let write_only: OwnedFd = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/null")
        .unwrap()
        .into();
    let error = drain_signal_pipe(&write_only).unwrap_err();
    assert!(error.contains("EBADF"), "{error}");
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

#[test]
fn terminal_child_status_has_priority_over_a_simultaneous_listener_failure() {
    let child = Some(MonitorEvent::ChildTerminated(TerminationReason::Exited {
        exit_code: 0,
        runtime_ms: 1,
    }));
    let listener = Some(MonitorEvent::MonitorFailure {
        message: "simultaneous listener event".to_string(),
    });

    assert!(matches!(
        prioritize_ready_events(child, listener),
        Some(MonitorEvent::ChildTerminated(TerminationReason::Exited {
            exit_code: 0,
            ..
        }))
    ));
}
