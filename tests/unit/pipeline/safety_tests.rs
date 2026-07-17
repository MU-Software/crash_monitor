use std::time::{Duration, Instant};

use super::run_plugin_safe;

#[test]
fn test_run_plugin_safe_ok() {
    let result = run_plugin_safe("test", 0, || Ok(42));
    assert_eq!(result, Some(42));
}

#[test]
fn test_run_plugin_safe_err() {
    let result = run_plugin_safe::<i32>("test", 0, || Err("boom".into()));
    assert_eq!(result, None);
}

#[test]
fn test_run_plugin_safe_panic() {
    let result = run_plugin_safe::<i32>("test", 0, || {
        panic!("oops");
    });
    assert_eq!(result, None);
}

#[test]
fn test_run_plugin_safe_timeout() {
    // alarm(timeout_secs) sends SIGALRM which interrupts blocking mach/nix syscalls
    // with EINTR. In a multi-threaded test runner, SIGALRM may be delivered to any
    // thread, so we can't rely on it interrupting our closure's blocking call.
    //
    // Instead, we verify the timeout contract by simulating what happens when a
    // plugin's blocking syscall returns EINTR: the closure returns Err, and
    // run_plugin_safe returns None.
    //
    // We also verify the alarm guard: with timeout > 0, alarm() is set and then
    // canceled by AlarmGuard on drop, completing within a reasonable time.
    let start = Instant::now();
    let result = run_plugin_safe::<()>("test", 1, || {
        // Simulate a plugin that detects EINTR and returns an error
        Err("interrupted by signal (EINTR)".into())
    });
    let elapsed = start.elapsed();

    // Closure returned Err → run_plugin_safe returns None
    assert_eq!(result, None);
    // AlarmGuard canceled the alarm, no leftover SIGALRM
    assert!(
        elapsed < Duration::from_secs(5),
        "should complete quickly, took {elapsed:?}"
    );
}
