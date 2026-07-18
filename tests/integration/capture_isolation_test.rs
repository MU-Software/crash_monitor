//! Integration coverage for the real exec-based task capture boundary.

use std::fs::File;
use std::time::Duration;

use crash_monitor::test_support::capture::{OwnedThreadPort, spawn_capture_helper};
use nix::sys::wait::{WaitStatus, waitpid};

#[test]
fn helper_exec_inherits_task_and_optional_thread_rights() {
    run_helper_handoff(false);
    run_helper_handoff(true);
}

fn run_helper_handoff(include_crashed_thread: bool) {
    let result_file = tempfile::tempfile().expect("create capture result file");
    let crashed_thread = include_crashed_thread.then(|| {
        // SAFETY: `mach_thread_self` creates a send-right reference owned by
        // the returned RAII wrapper.
        let thread = unsafe { mach2::mach_init::mach_thread_self() };
        OwnedThreadPort::new(thread)
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
                OwnedThreadPort::raw
            ),
            "bail_on_suspend_failure": false,
            "pid": std::process::id(),
            "process_name": "capture-isolation-test",
            "hang_duration_ms": null
        },
        "capture_stack_memory": false,
        "collectors": [{ "kind": "Dylib", "timeout_secs": 5 }],
        "expect_crashed_thread": include_crashed_thread
    })
    .to_string();
    // SAFETY: `mach_task_self` returns the current process's stable task send
    // right; this test borrows it only for the synchronous posix_spawn call.
    let task = unsafe { mach2::traps::mach_task_self() };
    let helper = spawn_capture_helper(
        std::path::Path::new(env!("CARGO_BIN_EXE_crash_monitor")),
        &request,
        &result_file,
        task,
        crashed_thread.as_ref().map(OwnedThreadPort::raw),
        Duration::from_secs(1),
    )
    .expect("spawn capture helper");

    let status = waitpid(helper, None).expect("reap capture helper");
    assert_eq!(status, WaitStatus::Exited(helper, 0));
    assert!(
        file_len(&result_file) > 0,
        "capture helper must publish its bounded wire result"
    );
}

fn file_len(file: &File) -> u64 {
    file.metadata().expect("inspect capture result").len()
}
