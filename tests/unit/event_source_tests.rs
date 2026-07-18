//! Unit tests for the signal-pipe plumbing in `event_source`.
//!
//! The Mach/`waitpid` channels need real OS resources and are covered
//! end-to-end by the E2E suite; here we test the pure, side-effect-free
//! `drain_signal_pipe` behaviour on a plain non-blocking pipe.

use super::*;

static SIGNAL_STATE_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn signal_state_guard() -> std::sync::MutexGuard<'static, ()> {
    SIGNAL_STATE_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

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
    let _guard = signal_state_guard();
    let (read_fd, _write_fd) = nonblocking_pipe();
    PENDING_SIGNALS.store(0, Ordering::Release);
    assert!(drain_signal_pipe(&read_fd).unwrap().is_empty());
}

#[test]
fn drain_reports_true_after_write_then_false_again() {
    let _guard = signal_state_guard();
    let (read_fd, write_fd) = nonblocking_pipe();

    // The handler publishes identity atomically and uses the pipe only to wake.
    PENDING_SIGNALS.store(PENDING_SIGUSR1, Ordering::Release);
    unistd::write(&write_fd, &[1u8]).unwrap();
    assert_eq!(drain_signal_pipe(&read_fd).unwrap(), vec![libc::SIGUSR1]);

    // Once drained, the pipe is empty again.
    assert!(drain_signal_pipe(&read_fd).unwrap().is_empty());
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
    let _guard = signal_state_guard();
    let (read_fd, write_fd) = nonblocking_cloexec_pipe("test signal pipe").unwrap();
    let burst = [1_u8; 512];
    PENDING_SIGNALS.store(PENDING_SIGUSR1, Ordering::Release);
    unistd::write(&write_fd, &burst).unwrap();

    assert_eq!(drain_signal_pipe(&read_fd).unwrap(), vec![libc::SIGUSR1]);
    assert!(drain_signal_pipe(&read_fd).unwrap().is_empty());
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
    let _guard = signal_state_guard();
    let previous = SIGNAL_PIPE_WRITE.swap(-1, Ordering::AcqRel);
    nix::errno::Errno::EBUSY.set();
    monitor_signal_handler(libc::SIGUSR1);
    assert_eq!(nix::errno::Errno::last(), nix::errno::Errno::EBUSY);
    PENDING_SIGNALS.store(0, Ordering::Release);
    SIGNAL_PIPE_WRITE.store(previous, Ordering::Release);
}

#[test]
fn signal_pipe_drain_propagates_non_retryable_errors() {
    let _guard = signal_state_guard();
    let write_only: OwnedFd = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/null")
        .unwrap()
        .into();
    let error = drain_signal_pipe(&write_only).unwrap_err();
    assert!(error.contains("EBADF"), "{error}");
}

#[test]
fn drain_preserves_coalesced_shutdown_and_snapshot_identities() {
    let _guard = signal_state_guard();
    let (read_fd, write_fd) = nonblocking_pipe();
    PENDING_SIGNALS.store(
        PENDING_SIGUSR1 | PENDING_SIGTERM | PENDING_SIGINT,
        Ordering::Release,
    );
    unistd::write(&write_fd, &[1]).unwrap();
    assert_eq!(
        drain_signal_pipe(&read_fd).unwrap(),
        vec![libc::SIGTERM, libc::SIGINT, libc::SIGUSR1]
    );
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
    let pid = nix::unistd::Pid::from_raw(42);
    for status in [
        WaitStatus::StillAlive,
        WaitStatus::Stopped(pid, nix::sys::signal::Signal::SIGSTOP),
        WaitStatus::Continued(pid),
    ] {
        assert_eq!(termination_from_wait_status(status, Duration::ZERO), None);
    }
}

#[test]
fn child_poll_fallback_bounds_an_unregistered_process_wait() {
    let now = Instant::now();
    let poll_deadline = now + CHILD_STATUS_POLL_INTERVAL;
    let earlier_deadline = now + Duration::from_millis(5);

    assert_eq!(child_wait_deadline(None, false, now), Some(poll_deadline));
    assert_eq!(
        child_wait_deadline(Some(now + Duration::from_secs(5)), false, now),
        Some(poll_deadline)
    );
    assert_eq!(
        child_wait_deadline(Some(earlier_deadline), false, now),
        Some(earlier_deadline)
    );
    assert_eq!(child_wait_deadline(None, true, now), None);
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
fn listener_disconnect_wakes_the_event_wait_without_a_watchdog_deadline() {
    let (listener_tx, listener_rx) = std::sync::mpsc::channel();
    let (bridged_rx, wake_fd) = bridge_exception_listener(listener_rx).unwrap();
    drop(listener_tx);

    let mut poll_fd = libc::pollfd {
        fd: wake_fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: poll_fd points to one initialized pollfd for the duration of the
    // bounded poll call.
    let ready = unsafe { libc::poll(std::ptr::from_mut(&mut poll_fd), 1, 500) };
    assert_eq!(ready, 1, "listener bridge did not wake on disconnect");
    assert!(matches!(
        poll_exception_listener(&bridged_rx),
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
