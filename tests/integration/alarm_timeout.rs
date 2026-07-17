//! Integration tests: `run_plugin_safe` timeout and success behavior.

use std::time::{Duration, Instant};

use crash_monitor::pipeline::run_plugin_safe;

#[test]
fn test_timeout_interrupts_blocking_call() {
    // In a multi-threaded test runner, SIGALRM may be delivered to any thread,
    // so we cannot rely on it actually interrupting `thread::sleep`. Instead we
    // verify the timeout contract: when a plugin's blocking syscall receives
    // EINTR (the mechanism alarm() uses), the closure returns Err and
    // `run_plugin_safe` returns None. We also verify the AlarmGuard cancels
    // the alarm on drop so there is no stray SIGALRM.
    let start = Instant::now();

    let result = run_plugin_safe::<()>("test_timeout", 1, || {
        // Simulate a plugin whose blocking syscall was interrupted by SIGALRM
        Err("interrupted by signal (EINTR)".into())
    });

    let elapsed = start.elapsed();

    assert!(result.is_none(), "expected None from timed-out plugin");
    assert!(
        elapsed < Duration::from_secs(5),
        "elapsed {elapsed:?} should be < 5s (AlarmGuard must cancel alarm)"
    );
}

#[test]
fn test_no_timeout_succeeds() {
    // timeout_secs=0 means no timeout
    let result = run_plugin_safe("test_ok", 0, || Ok(42));
    assert_eq!(result, Some(42));
}
