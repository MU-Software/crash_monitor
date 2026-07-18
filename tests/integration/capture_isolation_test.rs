//! Integration coverage for the real exec-based task capture boundary.

use std::fs::File;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::Duration;

use crash_monitor::platform::macos::ffi::capture_spawn::{CaptureHelperReap, spawn_capture_helper};

#[test]
fn helper_exec_inherits_task_and_optional_thread_rights() {
    run_helper_handoff(false, None, None);
    run_helper_handoff(true, None, None);
}

#[test]
fn helper_exec_closes_non_cloexec_parent_descriptor() {
    let sentinel_source = tempfile::tempfile().expect("create inherited-fd sentinel");
    // Keep the probe away from stdio and the fixed result channel even when
    // the test runner allocated the source as descriptor 3.
    // SAFETY: F_DUPFD returns a new uniquely owned descriptor on success.
    let duplicated =
        unsafe { nix::libc::fcntl(sentinel_source.as_raw_fd(), nix::libc::F_DUPFD, 10) };
    assert!(
        duplicated >= 10,
        "duplicate sentinel above reserved descriptors"
    );
    // SAFETY: the successful F_DUPFD result is uniquely owned here.
    let sentinel = unsafe { OwnedFd::from_raw_fd(duplicated) };
    let sentinel_fd = sentinel.as_raw_fd();
    // SAFETY: F_SETFD mutates only the live sentinel descriptor. Clearing
    // CLOEXEC makes this a meaningful test of POSIX_SPAWN_CLOEXEC_DEFAULT.
    assert_eq!(
        unsafe { nix::libc::fcntl(sentinel_fd, nix::libc::F_SETFD, 0) },
        0
    );
    run_helper_handoff(false, Some(sentinel_fd), None);
    // SAFETY: F_GETFD only verifies that the parent still owns the sentinel.
    assert!(unsafe { nix::libc::fcntl(sentinel_fd, nix::libc::F_GETFD) } >= 0);
}

#[test]
fn helper_timeout_kill_and_reap_still_works_with_fd_allowlist() {
    let sentinel_source = tempfile::tempfile().expect("create timeout sentinel");
    // SAFETY: F_DUPFD returns a new uniquely owned descriptor on success.
    let duplicated =
        unsafe { nix::libc::fcntl(sentinel_source.as_raw_fd(), nix::libc::F_DUPFD, 10) };
    assert!(duplicated >= 10);
    // SAFETY: the successful F_DUPFD result is uniquely owned here.
    let sentinel = unsafe { OwnedFd::from_raw_fd(duplicated) };
    let sentinel_fd = sentinel.as_raw_fd();
    // SAFETY: make the sentinel intentionally inheritable in the parent.
    assert_eq!(
        unsafe { nix::libc::fcntl(sentinel_fd, nix::libc::F_SETFD, 0) },
        0
    );
    run_helper_handoff(false, Some(sentinel_fd), Some(5_000));
    // SAFETY: the parent owner must remain intact after helper kill/reap.
    assert!(unsafe { nix::libc::fcntl(sentinel_fd, nix::libc::F_GETFD) } >= 0);
}

fn run_helper_handoff(
    include_crashed_thread: bool,
    inherited_fd_must_be_closed: Option<i32>,
    hold_after_handoff_ms: Option<u64>,
) {
    let result_file = tempfile::tempfile().expect("create capture result file");
    let crashed_thread = include_crashed_thread.then(|| {
        // SAFETY: `mach_thread_self` creates a send-right reference owned by
        // the returned RAII wrapper.
        let thread = unsafe { mach2::mach_init::mach_thread_self() };
        crash_monitor::platform::macos::ffi::types::OwnedThreadPort::new(thread)
    });
    let request = serde_json::json!({
        "version": 1,
        "event": {
            "report_id": "0123456789abcdef0123456789abcdef",
            "report_type": "snapshot",
            "termination": null,
            "exception_type": null,
            "exception_code": null,
            "exception_subcode": null,
            "exception_codes": [],
            "crashed_thread": crashed_thread.as_ref().map(
                crash_monitor::platform::macos::ffi::types::OwnedThreadPort::raw
            ),
            "bail_on_suspend_failure": false,
            "pid": std::process::id(),
            "process_name": "capture-isolation-test",
            "hang_duration_ms": null
        },
        "capture_stack_memory": false,
        "collectors": [{ "kind": "Dylib", "timeout_secs": 5 }],
        "expect_crashed_thread": include_crashed_thread
        ,"inherited_fd_must_be_closed": inherited_fd_must_be_closed,
        "hold_after_handoff_ms": hold_after_handoff_ms
    })
    .to_string();
    // SAFETY: `mach_task_self` returns the current process's stable task send
    // right; this test borrows it only for the synchronous posix_spawn call.
    let task = unsafe { mach2::traps::mach_task_self() };
    let mut helper = spawn_capture_helper(
        std::path::Path::new(env!("CARGO_BIN_EXE_crash_monitor")),
        &request,
        &result_file,
        task,
        crashed_thread
            .as_ref()
            .map(crash_monitor::platform::macos::ffi::types::OwnedThreadPort::raw),
        Duration::from_secs(1),
    )
    .expect("spawn capture helper");

    if hold_after_handoff_ms.is_some() {
        std::thread::sleep(Duration::from_millis(20));
        nix::sys::signal::kill(helper.pid(), nix::sys::signal::Signal::SIGKILL)
            .expect("kill held capture helper");
    }
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match helper.poll_reap().expect("poll capture helper") {
            CaptureHelperReap::StillRunning => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "capture helper did not exit"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            CaptureHelperReap::Exited(0) if hold_after_handoff_ms.is_none() => break,
            CaptureHelperReap::Signaled { signal: 9, .. } if hold_after_handoff_ms.is_some() => {
                break;
            }
            status => panic!("unexpected capture-helper terminal state: {status:?}"),
        }
    }
    if hold_after_handoff_ms.is_none() {
        assert!(
            file_len(&result_file) > 0,
            "capture helper must publish its bounded wire result"
        );
    }
}

fn file_len(file: &File) -> u64 {
    file.metadata().expect("inspect capture result").len()
}
