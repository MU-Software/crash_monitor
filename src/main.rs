//! `mbb_monitor` — Out-of-process crash monitor for Model Block Builder.
//!
//! Spawns the desktop app as a child process, monitors for crashes via
//! Mach exception ports, and handles F8 manual snapshots via SIGUSR1.
//!
//! macOS only — uses Mach kernel APIs (exception ports, `vm_read`, `task_for_pid`).

#[cfg(not(target_os = "macos"))]
compile_error!("mbb_monitor requires macOS (Mach kernel APIs)");

mod cli;
mod collectors;
mod config;
mod event_loop;
mod event_source;
mod filters;
mod notifiers;
pub mod pipeline;
mod platform;
mod postprocessors;
mod preprocessors;
pub mod shm;
mod utils;
mod watchdog;

use clap::{Parser, Subcommand};
use mach2::port::mach_port_t;
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use std::ffi::CString;
use std::sync::Arc;
use std::time::Duration;

// ═══════════════════════════════════════════════════
//  CLI
// ═══════════════════════════════════════════════════

#[derive(Parser)]
#[command(
    name = "mbb_monitor",
    version,
    about = "Crash monitor for Model Block Builder"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Path to the child executable (when used without subcommand)
    #[arg(trailing_var_arg = true)]
    args: Vec<String>,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the monitor with a child process
    Run {
        /// Path to the child executable
        app_path: String,
        /// Arguments to pass to the child
        #[arg(trailing_var_arg = true)]
        args: Vec<String>,
    },
    /// Display a human-readable summary of a crash/snapshot report
    Analyze {
        /// Path to a report JSON file or ZIP archive
        report: String,
    },
    /// Hex dump of a thread's stack memory from a report
    Stack {
        /// Path to a report JSON file or ZIP archive
        report: String,
        /// Thread index (0-based)
        #[arg(long)]
        thread: u32,
    },
    /// Symbolicate backtrace addresses to source `file:line` using a dSYM bundle
    Symbolicate {
        /// Path to a report JSON file or ZIP archive
        report: String,
        /// Path to the dSYM bundle directory (or DWARF binary)
        #[arg(long)]
        dsym: String,
        /// Write symbolicated report to a new file (default: print to stdout)
        #[arg(long, short)]
        output: Option<String>,
    },
}

// ═══════════════════════════════════════════════════
//  Setup helpers
// ═══════════════════════════════════════════════════

/// Read a u64 from an environment variable, falling back to `default`.
/// Warns on stderr if the variable is set but cannot be parsed.
fn env_u64(key: &str, default: u64) -> u64 {
    let Ok(val) = std::env::var(key) else {
        return default;
    };
    val.parse().unwrap_or_else(|_| {
        eprintln!("[monitor] Warning: {key}={val:?} is not a valid u64, using default {default}");
        default
    })
}

/// Check whether a binary at `path` has the `com.apple.security.cs.debugger` entitlement.
/// Returns `Ok(())` if entitled, `Err(message)` otherwise.
fn check_debugger_entitlement(path: &std::path::Path) -> Result<(), String> {
    let output = std::process::Command::new("codesign")
        .args(["-d", "--entitlements", "-", "--xml"])
        .arg(path)
        .output()
        .map_err(|e| format!("failed to run codesign: {e}"))?;

    if !output.status.success() {
        return Err("binary is not codesigned".to_string());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.contains("com.apple.security.cs.debugger") {
        Ok(())
    } else {
        Err("binary is codesigned but lacks com.apple.security.cs.debugger entitlement".to_string())
    }
}

/// Verify that this monitor binary has the debugger entitlement required for `task_for_pid()`.
/// Returns `Ok(())` on success, `Err(message)` if the check fails.
fn verify_self_entitlement() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot determine own path: {e}"))?;
    check_debugger_entitlement(&exe)
}

/// Check if the child exited immediately (e.g. exec failure). Returns exit code if so.
fn check_immediate_exit(child_pid: nix::unistd::Pid) -> Option<i32> {
    std::thread::sleep(Duration::from_millis(50));
    if let Ok(WaitStatus::Exited(_, status)) = waitpid(child_pid, Some(WaitPidFlag::WNOHANG)) {
        Some(status)
    } else {
        None
    }
}

/// Obtain the child's task port, retrying briefly while the child starts.
fn acquire_task_port(pid: i32) -> Result<mach_port_t, String> {
    let mut last_err = String::from("task_for_pid failed (no attempts)");
    for _ in 0..20 {
        match platform::get_task_for_pid(pid) {
            Ok(task) => return Ok(task),
            Err(e) => {
                last_err = e.to_string();
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
    Err(last_err)
}

// ═══════════════════════════════════════════════════
//  Main monitor loop
// ═══════════════════════════════════════════════════

#[allow(clippy::too_many_lines)] // orchestration function — splitting would scatter lifecycle logic
fn run_monitor(app_path: &str, app_args: &[String]) -> i32 {
    // Fail fast if the monitor binary itself lacks the debugger entitlement.
    // Without it, task_for_pid() will fail after child spawn, wasting time.
    if let Err(reason) = verify_self_entitlement() {
        eprintln!(
            "[monitor] Entitlement check failed: {reason}\n\
             [monitor] task_for_pid() requires com.apple.security.cs.debugger.\n\
             [monitor] Run `make crash-monitor` to rebuild with codesign."
        );
        return 1;
    }

    eprintln!("[monitor] Starting: {app_path}");

    // Set up SIGUSR1 signal pipe
    let signal_read_fd = match event_source::setup_signal_pipe() {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("[monitor] Failed to set up signal pipe: {e}");
            return 1;
        }
    };

    // Create exception port BEFORE spawning child.
    // posix_spawn will configure the child to use this port (survives exec).
    let exc_port = match platform::create_exception_port() {
        Ok(port) => port,
        Err(e) => {
            eprintln!("[monitor] Failed to create exception port: {e}");
            return 1;
        }
    };

    // Create shared memory for breadcrumbs/context/screenshots
    let shared_memory = match shm::SharedMemory::create(std::process::id()) {
        Ok(s) => Some(Arc::new(s)),
        Err(e) => {
            eprintln!("[monitor] Shared memory creation failed (continuing without): {e}");
            None
        }
    };

    // Build argv and envp for posix_spawn
    let Ok(c_path) = CString::new(app_path) else {
        eprintln!("[monitor] app_path contains null byte");
        return 1;
    };
    let mut c_argv_owned: Vec<CString> = vec![c_path.clone()];
    for arg in app_args {
        let Ok(c_arg) = CString::new(arg.as_str()) else {
            eprintln!("[monitor] argument contains null byte: {arg}");
            return 1;
        };
        c_argv_owned.push(c_arg);
    }
    let c_argv: Vec<&std::ffi::CStr> = c_argv_owned.iter().map(AsRef::as_ref).collect();

    // Build environment: inherit current env + add MBB_CRASH_SHM=<name>
    let mut env_strings: Vec<CString> = std::env::vars()
        .filter_map(|(k, v)| CString::new(format!("{k}={v}")).ok()) // skip invalid env vars
        .collect();
    let shm_env_value = shared_memory
        .as_ref()
        .map_or_else(|| "1".to_string(), |s| s.name().to_string());
    if let Ok(shm_env) = CString::new(format!("MBB_CRASH_SHM={shm_env_value}")) {
        env_strings.push(shm_env);
    }
    let c_envp: Vec<&std::ffi::CStr> = env_strings.iter().map(AsRef::as_ref).collect();

    // Spawn child with exception port pre-configured (survives exec)
    let child_pid_raw =
        match platform::spawn_with_exception_port(exc_port, &c_path, &c_argv, &c_envp) {
            Ok(pid) => pid,
            Err(e) => {
                eprintln!("[monitor] {e}");
                return 1;
            }
        };
    let child_pid = nix::unistd::Pid::from_raw(child_pid_raw);

    eprintln!("[monitor] Child PID: {child_pid}");

    // Detect exec failure (child exits immediately)
    if let Some(status) = check_immediate_exit(child_pid) {
        eprintln!("[monitor] Child exited immediately (status={status}). Likely exec failure.");
        return status;
    }

    // Get child task port for suspend/resume/vm_read (needed for crash introspection + snapshots).
    // Must succeed before starting listener — otherwise early crashes can't be inspected.
    let child_task = match acquire_task_port(child_pid_raw) {
        Ok(task) => platform::OwnedMachPort::new(task),
        Err(e) => {
            eprintln!(
                "[monitor] {e}. Cannot inspect crashes or take snapshots.\n\
                 [monitor] This usually means mbb_monitor lacks the debugger entitlement.\n\
                 [monitor] Run `make crash-monitor` to rebuild with codesign."
            );
            let _ = waitpid(child_pid, None);
            return 1;
        }
    };

    // Start exception listener thread AFTER task port is acquired
    let exc_rx = platform::start_listener(exc_port);

    // Extract process name from app_path for reports
    let process_name = std::path::Path::new(app_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(app_path);

    eprintln!("[monitor] Monitoring active. Press F8 in app for manual snapshot.");

    // Create the plugin pipeline
    let pl = pipeline::default_macos_pipeline(shared_memory.clone());

    // Inline-trigger toggles (Mach/SIGUSR1 are always-on; only OOM is opt-in).
    let cfg = config::load_config();
    let oom_detection = cfg.enabled && cfg.triggers.enabled && cfg.triggers.oom_detection.enabled;

    // ANR watchdog config (used inline by event_loop, no dedicated thread).
    // Environment overrides allow E2E tests to use shorter timeouts.
    let anr_config = event_loop::AnrConfig {
        warmup_ms: env_u64("MBB_ANR_WARMUP_MS", 10_000),
        threshold_ms: env_u64("MBB_ANR_THRESHOLD_MS", 5_000),
        check_interval_ms: env_u64("MBB_ANR_CHECK_INTERVAL_MS", 2_000),
        cooldown_ms: env_u64("MBB_ANR_COOLDOWN_MS", 60_000),
    };

    // Build event source from Mac-specific channels
    #[allow(clippy::cast_sign_loss)] // PID is always positive
    let child_pid_u32 = child_pid_raw as u32;
    let mut source = event_source::MacEventSource::new(exc_rx, signal_read_fd, child_pid);

    let exit_code = event_loop::event_loop(
        &mut source,
        &pl,
        child_task.raw(),
        child_pid_u32,
        process_name,
        &|header| platform::send_deferred_reply(header),
        shared_memory.as_ref(),
        Some(&anr_config),
        oom_detection,
    );

    // After a crash, destroy the exception port so that if the child re-faults
    // (e.g. kernel re-executes faulting instruction after KERN_FAILURE reply),
    // there is no Mach exception handler and the kernel falls back to delivering
    // a Unix signal (SIG_DFL → terminate). Without this, the child gets stuck
    // in an uninterruptible Mach exception wait, immune to even SIGKILL.
    if exit_code != 0 {
        // SAFETY: mach_port_destroy removes all rights (receive + send) from
        // this task. The listener thread's mach_msg will return an error and exit.
        unsafe {
            mach2::mach_port::mach_port_destroy(mach2::traps::mach_task_self(), exc_port);
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }

    // Wait for child to exit after crash (with SIGKILL fallback).
    if exit_code != 0 {
        use nix::sys::wait::WaitPidFlag;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while let Ok(nix::sys::wait::WaitStatus::StillAlive) =
            waitpid(child_pid, Some(WaitPidFlag::WNOHANG))
        {
            if std::time::Instant::now() >= deadline {
                eprintln!("[monitor] Child still alive after 3s, sending SIGKILL");
                let _ = nix::sys::signal::kill(child_pid, nix::sys::signal::Signal::SIGKILL);
                let _ = waitpid(child_pid, None);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    exit_code
}

// ═══════════════════════════════════════════════════
//  Entry point
// ═══════════════════════════════════════════════════

fn main() {
    let cli = Cli::parse();

    let exit_code = match cli.command {
        Some(Commands::Run { app_path, args }) => run_monitor(&app_path, &args),
        Some(Commands::Analyze { report }) => cli::analyze::run(&report),
        Some(Commands::Stack { report, thread }) => cli::stack::run(&report, thread),
        Some(Commands::Symbolicate {
            report,
            dsym,
            output,
        }) => cli::symbolicate::run(&report, &dsym, output.as_deref()),
        None => {
            // No subcommand: treat positional args as "run" mode
            // Usage: mbb_monitor ./voxelcore_desktop [args...]
            //    or: mbb_monitor -- ./voxelcore_desktop [args...]
            if cli.args.is_empty() {
                eprintln!("Usage: mbb_monitor [run] <app_path> [args...]");
                1
            } else {
                let app_path = &cli.args[0];
                let args = &cli.args[1..];
                run_monitor(app_path, args)
            }
        }
    };

    std::process::exit(exit_code);
}
