//! Integration tests for signal-free cooperative and subprocess deadlines.

use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use crash_monitor::pipeline::{
    PluginContext, PluginRunResult, run_plugin_cooperative, run_plugin_subprocess,
};

const HELPER_MODE: &str = "CRASH_MONITOR_TIMEOUT_HELPER_MODE";
const HELPER_PID_FILE: &str = "CRASH_MONITOR_TIMEOUT_HELPER_PID_FILE";
const HELPER_MARKER_FILE: &str = "CRASH_MONITOR_TIMEOUT_HELPER_MARKER_FILE";
const HELPER_GRANDCHILD_FILE: &str = "CRASH_MONITOR_TIMEOUT_HELPER_GRANDCHILD_FILE";

static SIGALRM_HITS: AtomicUsize = AtomicUsize::new(0);

extern "C" fn retry_signal_handler(_signal: nix::libc::c_int) {}

extern "C" fn sigalrm_sentinel_handler(_signal: nix::libc::c_int) {
    SIGALRM_HITS.fetch_add(1, Ordering::Relaxed);
}

fn write_env_file(key: &str, value: impl AsRef<[u8]>) {
    if let Some(path) = std::env::var_os(key) {
        std::fs::write(path, value).expect("helper marker write");
    }
}

fn run_retrying_read_helper() -> ! {
    use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};

    let action = SigAction::new(
        SigHandler::Handler(retry_signal_handler),
        SaFlags::empty(),
        SigSet::empty(),
    );
    // SAFETY: this runs only in the disposable helper process. The handler is
    // a no-op and is valid for the helper's entire remaining lifetime.
    unsafe { sigaction(Signal::SIGUSR2, &action) }.expect("install helper SIGUSR2 handler");

    let mut pipe_fds = [0; 2];
    // SAFETY: `pipe_fds` points to two writable c_int slots.
    assert_eq!(unsafe { nix::libc::pipe(pipe_fds.as_mut_ptr()) }, 0);

    // Target the blocking main pthread specifically so each signal interrupts
    // the `read`; process-directed delivery could select the signaler thread.
    // SAFETY: pthread_self returns the current live pthread identifier.
    let target_thread = unsafe { nix::libc::pthread_self() } as usize;
    std::thread::spawn(move || {
        loop {
            // pthread_t is pointer-shaped on macOS. The integer round trip is
            // confined to this child process and the target thread stays live.
            let target = target_thread as nix::libc::pthread_t;
            // SAFETY: the target pthread is the helper's main thread and the
            // installed signal handler is async-signal-safe.
            let _ = unsafe { nix::libc::pthread_kill(target, nix::libc::SIGUSR2) };
            std::thread::sleep(Duration::from_millis(5));
        }
    });

    let mut interrupted = 0_u32;
    let mut byte = 0_u8;
    loop {
        // The write end remains open and intentionally unwritten, so this is a
        // real blocking read rather than a synthetic error result.
        // SAFETY: the fd is valid and `byte` points to one writable byte.
        let result = unsafe {
            nix::libc::read(
                pipe_fds[0],
                std::ptr::from_mut(&mut byte).cast(),
                std::mem::size_of_val(&byte),
            )
        };
        if result == -1 && nix::errno::Errno::last() == nix::errno::Errno::EINTR {
            interrupted += 1;
            if interrupted == 3 {
                write_env_file(HELPER_MARKER_FILE, b"retried-eintr");
            }
            continue;
        }
        panic!("blocking helper read unexpectedly returned {result}");
    }
}

fn run_blocked_read_helper() -> ! {
    let mut pipe_fds = [0; 2];
    // SAFETY: `pipe_fds` points to two writable c_int slots.
    assert_eq!(unsafe { nix::libc::pipe(pipe_fds.as_mut_ptr()) }, 0);
    let mut byte = 0_u8;
    // SAFETY: both pipe ends remain live and the buffer is valid. This call is
    // expected to block until the supervisor kills the helper process group.
    let result = unsafe {
        nix::libc::read(
            pipe_fds[0],
            std::ptr::from_mut(&mut byte).cast(),
            std::mem::size_of_val(&byte),
        )
    };
    panic!("blocked helper read unexpectedly returned {result}");
}

fn run_sigalrm_sentinel_helper() {
    use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};

    let mut mask = SigSet::empty();
    mask.add(Signal::SIGUSR1);
    let action = SigAction::new(
        SigHandler::Handler(sigalrm_sentinel_handler),
        SaFlags::empty(),
        mask,
    );
    // SAFETY: this is a disposable helper process. The sentinel has static
    // lifetime and the previous disposition is restored before returning.
    let previous = unsafe { sigaction(Signal::SIGALRM, &action) }
        .expect("install disposable SIGALRM sentinel");
    let expected_disposition = sigalrm_disposition();
    let expected_timer = real_timer_state();

    SIGALRM_HITS.store(0, Ordering::Relaxed);
    let running = Arc::new(AtomicBool::new(true));
    let sender_running = running.clone();
    let sender = std::thread::spawn(move || {
        while sender_running.load(Ordering::Acquire) {
            // SAFETY: SIGALRM has the sentinel handler installed in this
            // disposable process; raise targets only the calling thread.
            unsafe { nix::libc::raise(nix::libc::SIGALRM) };
            std::thread::sleep(Duration::from_millis(2));
        }
    });

    let mut nested = Command::new(std::env::current_exe().expect("nested test executable"));
    nested
        .args(["--ignored", "--exact", "subprocess_helper", "--nocapture"])
        .env(HELPER_MODE, "sleep")
        .env_remove(HELPER_PID_FILE)
        .env_remove(HELPER_GRANDCHILD_FILE)
        .env_remove(HELPER_MARKER_FILE);
    let nested_result = run_plugin_subprocess(
        "nested-sigalrm-sentinel",
        &mut nested,
        &PluginContext::with_timeout(Duration::from_millis(200)),
    );

    running.store(false, Ordering::Release);
    sender.join().expect("sentinel signal sender");

    assert!(matches!(nested_result, PluginRunResult::TimedOut));
    assert!(
        SIGALRM_HITS.load(Ordering::Relaxed) > 5,
        "sentinel must remain installed throughout supervision"
    );
    assert_eq!(expected_disposition, sigalrm_disposition());
    assert_eq!(expected_timer, real_timer_state());

    // SAFETY: restore the disposition returned by the successful install.
    unsafe { sigaction(Signal::SIGALRM, &previous) }
        .expect("restore disposable SIGALRM disposition");
}

/// This ignored test is an exec target for the real subprocess supervisor.
#[test]
#[ignore = "spawned explicitly by timeout integration tests"]
#[allow(clippy::zombie_processes)] // Descendant lifecycle is intentionally owned by the supervisor.
fn subprocess_helper() {
    let mode = std::env::var(HELPER_MODE).expect("helper mode");
    write_env_file(HELPER_PID_FILE, std::process::id().to_string());

    match mode.as_str() {
        "normal" => println!("normal-helper-output"),
        "exit-error" => std::process::exit(23),
        "verbose" => {
            use std::io::Write as _;
            let payload = vec![b'x'; 2 * 1024 * 1024];
            std::io::stdout()
                .write_all(&payload)
                .expect("write verbose helper output");
        }
        "verbose-both" => {
            use std::io::Write as _;
            let stdout = std::thread::spawn(|| {
                std::io::stdout()
                    .write_all(&vec![b'o'; 2 * 1024 * 1024])
                    .expect("write verbose stdout");
            });
            let stderr = std::thread::spawn(|| {
                std::io::stderr()
                    .write_all(&vec![b'e'; 2 * 1024 * 1024])
                    .expect("write verbose stderr");
            });
            stdout.join().expect("verbose stdout thread");
            stderr.join().expect("verbose stderr thread");
        }
        "cpu" => loop {
            std::hint::spin_loop();
        },
        "sleep" => std::thread::sleep(Duration::from_secs(60)),
        "blocked-read" => run_blocked_read_helper(),
        "retry-eintr" => run_retrying_read_helper(),
        "grandchild" => {
            let child = Command::new("sleep")
                .arg("60")
                .spawn()
                .expect("spawn grandchild");
            write_env_file(HELPER_GRANDCHILD_FILE, child.id().to_string());
            std::thread::sleep(Duration::from_secs(60));
        }
        "successful-grandchild" => {
            let child = Command::new("sleep")
                .arg("60")
                .spawn()
                .expect("spawn grandchild");
            write_env_file(HELPER_GRANDCHILD_FILE, child.id().to_string());
            // Return successfully while the descendant still owns inherited
            // process-group membership and output pipes. The supervisor must
            // terminate it before draining those pipes and returning.
        }
        "escaped-pipe" => {
            let mut command = Command::new("sleep");
            command.arg("1").process_group(0);
            let child = command.spawn().expect("spawn escaped pipe holder");
            write_env_file(HELPER_GRANDCHILD_FILE, child.id().to_string());
            // The new process group escapes the supervisor's owned group but
            // deliberately retains stdout/stderr. Reader shutdown must still
            // be bounded and diagnose incomplete capture.
        }
        "sigalrm-sentinel" => run_sigalrm_sentinel_helper(),
        other => panic!("unknown helper mode {other}"),
    }
}

fn helper_command(mode: &str, pid_file: &Path) -> Command {
    let mut command = Command::new(std::env::current_exe().expect("integration test executable"));
    command
        .args(["--ignored", "--exact", "subprocess_helper", "--nocapture"])
        .env(HELPER_MODE, mode)
        .env(HELPER_PID_FILE, pid_file);
    command
}

fn run_helper(
    name: &str,
    mode: &str,
    timeout: Duration,
    pid_file: &Path,
    configure: impl FnOnce(&mut Command),
) -> PluginRunResult<crash_monitor::pipeline::SubprocessOutput> {
    let mut command = helper_command(mode, pid_file);
    configure(&mut command);
    run_plugin_subprocess(name, &mut command, &PluginContext::with_timeout(timeout))
}

fn read_pid(path: &Path) -> nix::unistd::Pid {
    let raw = std::fs::read_to_string(path)
        .expect("helper should record pid before blocking")
        .parse::<i32>()
        .expect("recorded pid");
    nix::unistd::Pid::from_raw(raw)
}

fn assert_process_gone(pid: nix::unistd::Pid) {
    for _ in 0..300 {
        if matches!(
            nix::sys::signal::kill(pid, None),
            Err(nix::errno::Errno::ESRCH)
        ) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("process {pid} survived supervisor timeout");
}

#[test]
fn cooperative_deadline_returns_typed_timeout() {
    let context = PluginContext::with_timeout(Duration::from_millis(20));
    let result = run_plugin_cooperative::<()>("cooperative", &context, |context| {
        loop {
            context.checkpoint()?;
            std::hint::spin_loop();
        }
    });
    assert!(matches!(result, PluginRunResult::TimedOut));
}

#[test]
fn subprocess_normal_completion_preserves_output() {
    let dir = tempfile::tempdir().unwrap();
    let result = run_helper(
        "normal",
        "normal",
        Duration::from_secs(5),
        &dir.path().join("pid"),
        |_| {},
    );
    let PluginRunResult::Completed(output) = result else {
        panic!("expected completed helper");
    };
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("normal-helper-output"));
    assert!(!output.stdout_truncated);
    assert!(!output.stderr_truncated);
}

#[test]
fn subprocess_exit_and_oversized_output_are_bounded_and_distinct_from_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let exited = run_helper(
        "exit-error",
        "exit-error",
        Duration::from_secs(5),
        &dir.path().join("exit.pid"),
        |_| {},
    );
    let PluginRunResult::Completed(exited) = exited else {
        panic!("a child exit is a completed supervisor operation");
    };
    assert_eq!(exited.status.code(), Some(23));

    let verbose = run_helper(
        "verbose",
        "verbose",
        Duration::from_secs(5),
        &dir.path().join("verbose.pid"),
        |_| {},
    );
    let PluginRunResult::Completed(verbose) = verbose else {
        panic!("verbose helper should complete");
    };
    assert!(verbose.status.success());
    assert_eq!(verbose.stdout.len(), 1024 * 1024);
    assert!(verbose.stdout_truncated);

    let both = run_helper(
        "verbose-both",
        "verbose-both",
        Duration::from_secs(5),
        &dir.path().join("verbose-both.pid"),
        |_| {},
    );
    let PluginRunResult::Completed(both) = both else {
        panic!("both verbose streams should complete");
    };
    assert_eq!(both.stdout.len(), 1024 * 1024);
    assert_eq!(both.stderr.len(), 1024 * 1024);
    assert!(both.stdout_truncated);
    assert!(both.stderr_truncated);
}

#[test]
fn concurrent_cpu_and_sleep_helpers_are_independently_killed() {
    let dir = tempfile::tempdir().unwrap();
    let cpu_pid = dir.path().join("cpu.pid");
    let sleep_pid = dir.path().join("sleep.pid");
    let started = Instant::now();

    let cpu = std::thread::spawn(move || {
        run_helper("cpu", "cpu", Duration::from_secs(2), &cpu_pid, |_| {})
    });
    let sleeper = std::thread::spawn(move || {
        run_helper("sleep", "sleep", Duration::from_secs(2), &sleep_pid, |_| {})
    });

    assert!(matches!(cpu.join().unwrap(), PluginRunResult::TimedOut));
    assert!(matches!(sleeper.join().unwrap(), PluginRunResult::TimedOut));
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "concurrent deadlines must not serialize"
    );

    assert_process_gone(read_pid(&dir.path().join("cpu.pid")));
    assert_process_gone(read_pid(&dir.path().join("sleep.pid")));
}

#[test]
fn cancellation_token_kills_an_already_running_helper() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("cancelled.pid");
    let mut command = helper_command("sleep", &pid_file);
    let context = PluginContext::without_deadline();
    let cancellation = context.cancellation_token();
    let observed_pid_file = pid_file.clone();
    let canceller = std::thread::spawn(move || {
        for _ in 0..300 {
            if observed_pid_file.exists() {
                cancellation.cancel();
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        cancellation.cancel();
        panic!("helper did not start before cancellation test deadline");
    });

    let result = run_plugin_subprocess("token-cancelled", &mut command, &context);
    canceller.join().unwrap();

    assert!(matches!(result, PluginRunResult::TimedOut));
    assert_process_gone(read_pid(&pid_file));
}

#[test]
fn blocking_and_eintr_retrying_io_are_hard_timed_out() {
    let dir = tempfile::tempdir().unwrap();
    let blocked_pid = dir.path().join("blocked.pid");
    let retry_pid = dir.path().join("retry.pid");
    let retry_marker = dir.path().join("retry.marker");

    let blocked = run_helper(
        "blocked-read",
        "blocked-read",
        Duration::from_secs(2),
        &blocked_pid,
        |_| {},
    );
    assert!(matches!(blocked, PluginRunResult::TimedOut));

    let retry = run_helper(
        "retry-eintr",
        "retry-eintr",
        Duration::from_secs(2),
        &retry_pid,
        |command| {
            command.env(HELPER_MARKER_FILE, &retry_marker);
        },
    );
    assert!(matches!(retry, PluginRunResult::TimedOut));
    assert_eq!(
        std::fs::read(&retry_marker).expect("helper should retry real EINTR"),
        b"retried-eintr"
    );
    assert_process_gone(read_pid(&blocked_pid));
    assert_process_gone(read_pid(&retry_pid));
}

#[test]
fn timeout_kills_the_helper_process_group() {
    let dir = tempfile::tempdir().unwrap();
    let helper_pid = dir.path().join("helper.pid");
    let grandchild_pid = dir.path().join("grandchild.pid");
    let result = run_helper(
        "grandchild",
        "grandchild",
        Duration::from_secs(2),
        &helper_pid,
        |command| {
            command.env(HELPER_GRANDCHILD_FILE, &grandchild_pid);
        },
    );
    assert!(matches!(result, PluginRunResult::TimedOut));
    assert_process_gone(read_pid(&helper_pid));
    assert_process_gone(read_pid(&grandchild_pid));
}

#[test]
fn successful_helper_cleans_up_remaining_process_group_descendants() {
    let dir = tempfile::tempdir().unwrap();
    let helper_pid = dir.path().join("helper.pid");
    let grandchild_pid = dir.path().join("grandchild.pid");
    let result = run_helper(
        "successful-grandchild",
        "successful-grandchild",
        Duration::from_secs(5),
        &helper_pid,
        |command| {
            command.env(HELPER_GRANDCHILD_FILE, &grandchild_pid);
        },
    );
    let PluginRunResult::Completed(output) = result else {
        panic!("helper should complete before its deadline");
    };

    assert!(output.status.success());
    assert_process_gone(read_pid(&helper_pid));
    assert_process_gone(read_pid(&grandchild_pid));
}

#[test]
fn escaped_pipe_holder_cannot_block_output_drain() {
    let dir = tempfile::tempdir().unwrap();
    let helper_pid = dir.path().join("helper.pid");
    let escaped_pid = dir.path().join("escaped.pid");
    let started = Instant::now();
    let result = run_helper(
        "escaped-pipe",
        "escaped-pipe",
        Duration::from_secs(5),
        &helper_pid,
        |command| {
            command.env(HELPER_GRANDCHILD_FILE, &escaped_pid);
        },
    );

    assert!(
        matches!(result, PluginRunResult::Failed(error) if error.contains("output capture was incomplete"))
    );
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "escaped inherited pipe must not stall the supervisor"
    );
    assert_process_gone(read_pid(&escaped_pid));
}

#[derive(Debug, PartialEq, Eq)]
struct SignalDisposition {
    handler: usize,
    flags: nix::libc::c_int,
    mask_members: Vec<nix::libc::c_int>,
}

#[derive(Debug, PartialEq, Eq)]
struct RealTimerState {
    interval_seconds: nix::libc::time_t,
    interval_microseconds: nix::libc::suseconds_t,
    value_seconds: nix::libc::time_t,
    value_microseconds: nix::libc::suseconds_t,
}

fn real_timer_state() -> RealTimerState {
    // SAFETY: `timer` points to initialized writable storage and ITIMER_REAL
    // only queries this process's current timer.
    let timer = unsafe {
        let mut timer = std::mem::zeroed::<nix::libc::itimerval>();
        assert_eq!(
            nix::libc::getitimer(nix::libc::ITIMER_REAL, std::ptr::from_mut(&mut timer)),
            0
        );
        timer
    };
    RealTimerState {
        interval_seconds: timer.it_interval.tv_sec,
        interval_microseconds: timer.it_interval.tv_usec,
        value_seconds: timer.it_value.tv_sec,
        value_microseconds: timer.it_value.tv_usec,
    }
}

fn sigalrm_disposition() -> SignalDisposition {
    // SAFETY: a null `act` queries the current disposition without changing it;
    // `current` points to initialized writable storage for the result.
    let current = unsafe {
        let mut current = std::mem::zeroed::<nix::libc::sigaction>();
        assert_eq!(
            nix::libc::sigaction(
                nix::libc::SIGALRM,
                std::ptr::null(),
                std::ptr::from_mut(&mut current),
            ),
            0
        );
        current
    };
    // Darwin's signal range is 1 through SIGUSR2 (31); libc does not expose
    // the C NSIG macro on this target.
    let mask_members = (1..=nix::libc::SIGUSR2)
        .filter(|&signal| {
            // SAFETY: `current.sa_mask` was initialized by successful
            // `sigaction`, and every value in 1..NSIG is a valid signal
            // number to query with sigismember.
            unsafe { nix::libc::sigismember(std::ptr::from_ref(&current.sa_mask), signal) == 1 }
        })
        .collect();
    SignalDisposition {
        handler: current.sa_sigaction,
        flags: current.sa_flags,
        mask_members,
    }
}

#[test]
fn subprocess_deadline_does_not_change_sigalrm_disposition() {
    let dir = tempfile::tempdir().unwrap();
    let before = sigalrm_disposition();
    let timer_before = real_timer_state();
    let result = run_helper(
        "signal-state",
        "sleep",
        Duration::from_secs(2),
        &dir.path().join("pid"),
        |_| {},
    );
    let after = sigalrm_disposition();
    let timer_after = real_timer_state();

    assert!(matches!(result, PluginRunResult::TimedOut));
    assert_eq!(before, after);
    assert_eq!(timer_before, timer_after);
}

#[test]
fn subprocess_deadline_preserves_live_sigalrm_sentinel() {
    let dir = tempfile::tempdir().unwrap();
    let result = run_helper(
        "sigalrm-sentinel",
        "sigalrm-sentinel",
        Duration::from_secs(5),
        &dir.path().join("pid"),
        |_| {},
    );
    let PluginRunResult::Completed(output) = result else {
        panic!("disposable sentinel helper should complete");
    };
    assert!(output.status.success());
}
