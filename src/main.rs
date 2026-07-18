//! `crash_monitor` — Out-of-process crash monitor for Model Block Builder.
//!
//! Spawns the desktop app as a child process, monitors for crashes via
//! Mach exception ports, and handles F8 manual snapshots via SIGUSR1.
//!
//! macOS only — uses Mach kernel APIs (exception ports, `vm_read`, `task_for_pid`).

#[cfg(not(target_os = "macos"))]
compile_error!("crash_monitor requires macOS (Mach kernel APIs)");
#[cfg(not(target_arch = "aarch64"))]
compile_error!(
    "crash_monitor currently supports native arm64 macOS only; x86_64 and Rosetta are unsupported"
);

#[cfg(panic = "abort")]
compile_error!(
    "crash_monitor requires panic=unwind because cooperative plugin isolation uses catch_unwind"
);

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
use nix::sys::wait::{WaitPidFlag, waitpid};
use std::ffi::{CString, OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::sync::Arc;
use std::time::{Duration, Instant};

// ═══════════════════════════════════════════════════
//  CLI
// ═══════════════════════════════════════════════════

#[derive(Parser)]
#[command(
    name = "crash_monitor",
    version,
    about = "Out-of-process crash monitor for native applications"
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
    /// Internal killable boundary for task-facing collectors.
    #[command(hide = true)]
    CaptureHelper {
        #[arg(long)]
        request_json: String,
    },
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

const CRASH_MONITOR_SHM_ENV: &str = "CRASH_MONITOR_SHM";

/// Build the child's POSIX environment without inheriting a stale shared-memory
/// capability from the monitor's own environment.
///
/// `vars_os()` values are encoded directly so non-UTF-8 environment entries
/// survive unchanged. A freshly-created mapping contributes the sole
/// `CRASH_MONITOR_SHM` entry; when no mapping exists, the key is omitted.
fn build_child_environment<I>(
    inherited: I,
    shared_memory_name: Option<&str>,
) -> Result<Vec<CString>, String>
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    let shm_key = OsStr::new(CRASH_MONITOR_SHM_ENV);
    let mut environment = Vec::new();

    for (key, value) in inherited {
        if key == shm_key {
            continue;
        }

        let mut entry = Vec::with_capacity(key.as_bytes().len() + value.as_bytes().len() + 1);
        entry.extend_from_slice(key.as_bytes());
        entry.push(b'=');
        entry.extend_from_slice(value.as_bytes());
        environment.push(
            CString::new(entry)
                .map_err(|_| "inherited environment contains a null byte".to_string())?,
        );
    }

    if let Some(name) = shared_memory_name {
        environment.push(
            CString::new(format!("{CRASH_MONITOR_SHM_ENV}={name}"))
                .map_err(|_| "shared-memory name contains a null byte".to_string())?,
        );
    }

    Ok(environment)
}

const DEBUGGER_ENTITLEMENT: &str = "com.apple.security.cs.debugger";

#[derive(Debug, PartialEq, Eq)]
enum EntitlementCheckError {
    CurrentExecutableUnavailable(String),
    CodesignUnavailable(String),
    UnsignedBinary,
    MalformedSignature(String),
    EntitlementExtractionFailed(String),
    MalformedEntitlementPlist(String),
    MissingDebuggerEntitlement,
    DebuggerEntitlementFalse,
    DebuggerEntitlementNotBoolean,
}

impl std::fmt::Display for EntitlementCheckError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CurrentExecutableUnavailable(error) => {
                write!(f, "cannot determine own executable path: {error}")
            }
            Self::CodesignUnavailable(error) => write!(f, "failed to run codesign: {error}"),
            Self::UnsignedBinary => f.write_str("binary is unsigned"),
            Self::MalformedSignature(error) => {
                write!(f, "binary has an invalid or malformed signature: {error}")
            }
            Self::EntitlementExtractionFailed(error) => {
                write!(f, "cannot extract signed entitlements: {error}")
            }
            Self::MalformedEntitlementPlist(error) => {
                write!(f, "signed entitlement plist is malformed: {error}")
            }
            Self::MissingDebuggerEntitlement => {
                write!(
                    f,
                    "signed entitlements do not contain {DEBUGGER_ENTITLEMENT}"
                )
            }
            Self::DebuggerEntitlementFalse => {
                write!(f, "signed entitlement {DEBUGGER_ENTITLEMENT} is false")
            }
            Self::DebuggerEntitlementNotBoolean => write!(
                f,
                "signed entitlement {DEBUGGER_ENTITLEMENT} is not a boolean"
            ),
        }
    }
}

fn codesign_error_text(output: &[u8]) -> String {
    let message = String::from_utf8_lossy(output).trim().to_string();
    if message.is_empty() {
        "codesign returned a failure status without diagnostics".to_string()
    } else {
        message
    }
}

fn classify_signature_failure(stderr: &[u8]) -> EntitlementCheckError {
    let message = codesign_error_text(stderr);
    if message
        .to_ascii_lowercase()
        .contains("code object is not signed at all")
    {
        EntitlementCheckError::UnsignedBinary
    } else {
        EntitlementCheckError::MalformedSignature(message)
    }
}

fn parse_debugger_entitlement(plist_bytes: &[u8]) -> Result<(), EntitlementCheckError> {
    // `codesign` emits no plist bytes for a validly signed binary that has no
    // entitlement blob at all. That is a missing key, not malformed data.
    if plist_bytes.iter().all(u8::is_ascii_whitespace) {
        return Err(EntitlementCheckError::MissingDebuggerEntitlement);
    }
    let value = plist::Value::from_reader(std::io::Cursor::new(plist_bytes))
        .map_err(|error| EntitlementCheckError::MalformedEntitlementPlist(error.to_string()))?;
    let dictionary = value.as_dictionary().ok_or_else(|| {
        EntitlementCheckError::MalformedEntitlementPlist(
            "top-level plist value is not a dictionary".to_string(),
        )
    })?;

    match dictionary.get(DEBUGGER_ENTITLEMENT) {
        Some(plist::Value::Boolean(true)) => Ok(()),
        Some(plist::Value::Boolean(false)) => Err(EntitlementCheckError::DebuggerEntitlementFalse),
        Some(_) => Err(EntitlementCheckError::DebuggerEntitlementNotBoolean),
        None => Err(EntitlementCheckError::MissingDebuggerEntitlement),
    }
}

/// Check whether a binary has a valid signature whose signed entitlement plist
/// grants the debugger entitlement with an actual boolean `true` value.
fn check_debugger_entitlement(path: &std::path::Path) -> Result<(), EntitlementCheckError> {
    // Strict verification distinguishes an unsigned code object from a signed
    // object whose signature or sealed resources are malformed before trusting
    // the entitlement payload printed by the platform codesign tool.
    let verification = std::process::Command::new("codesign")
        .args(["--verify", "--strict"])
        .arg(path)
        .output()
        .map_err(|error| EntitlementCheckError::CodesignUnavailable(error.to_string()))?;
    if !verification.status.success() {
        return Err(classify_signature_failure(&verification.stderr));
    }

    let output = std::process::Command::new("codesign")
        .args(["-d", "--entitlements", "-", "--xml"])
        .arg(path)
        .output()
        .map_err(|error| EntitlementCheckError::CodesignUnavailable(error.to_string()))?;

    if !output.status.success() {
        return Err(EntitlementCheckError::EntitlementExtractionFailed(
            codesign_error_text(&output.stderr),
        ));
    }

    parse_debugger_entitlement(&output.stdout)
}

/// Verify that this monitor binary has the debugger entitlement required for `task_for_pid()`.
/// Returns `Ok(())` on success, `Err(message)` if the check fails.
fn verify_self_entitlement() -> Result<(), EntitlementCheckError> {
    let exe = std::env::current_exe()
        .map_err(|error| EntitlementCheckError::CurrentExecutableUnavailable(error.to_string()))?;
    check_debugger_entitlement(&exe)
}

enum TaskAcquisition {
    Acquired(mach_port_t),
    ChildTerminated(pipeline::TerminationReason),
}

/// Poll the child without losing any terminal status. `EINTR` is retried and
/// `ECHILD` is an ownership failure rather than a successful child exit.
fn poll_child_termination(
    child_pid: nix::unistd::Pid,
    child_started_at: Instant,
) -> Result<Option<pipeline::TerminationReason>, String> {
    loop {
        match waitpid(child_pid, Some(WaitPidFlag::WNOHANG)) {
            Ok(status) => {
                return Ok(event_source::termination_from_wait_status(
                    status,
                    child_started_at.elapsed(),
                ));
            }
            Err(nix::errno::Errno::EINTR) => {}
            Err(nix::errno::Errno::ECHILD) => {
                return Err("waitpid lost ownership of the child (ECHILD)".to_string());
            }
            Err(e) => return Err(format!("waitpid failed: {e}")),
        }
    }
}

const CRASH_REAP_GRACE_DEADLINE: Duration = Duration::from_secs(3);
const CRASH_REAP_AFTER_KILL_DEADLINE: Duration = Duration::from_secs(1);
const CRASH_REAP_POLL_INTERVAL: Duration = Duration::from_millis(100);
const TASK_ACQUISITION_DEADLINE: Duration = Duration::from_secs(1);
const TASK_ACQUISITION_RETRY_INTERVAL: Duration = Duration::from_millis(50);
const UNMONITORED_TERMINATION_GRACE_DEADLINE: Duration = Duration::from_secs(2);
const UNMONITORED_REAP_AFTER_KILL_DEADLINE: Duration = Duration::from_secs(1);

#[derive(Clone, Copy)]
struct CrashReapDeadlines {
    before_sigkill: Duration,
    after_sigkill: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildPoll {
    Running,
    Interrupted,
    Terminated(pipeline::TerminationReason),
}

fn poll_child_after_crash_once(
    child_pid: nix::unistd::Pid,
    child_started_at: Instant,
) -> Result<ChildPoll, String> {
    match waitpid(child_pid, Some(WaitPidFlag::WNOHANG)) {
        Ok(status) => Ok(event_source::termination_from_wait_status(
            status,
            child_started_at.elapsed(),
        )
        .map_or(ChildPoll::Running, ChildPoll::Terminated)),
        Err(nix::errno::Errno::EINTR) => Ok(ChildPoll::Interrupted),
        Err(error) => Err(format!("waitpid after crash failed: {error}")),
    }
}

fn poll_until_deadline<P, N, S>(
    poll: &mut P,
    deadline: Instant,
    now: &mut N,
    sleep: &mut S,
) -> Result<Option<pipeline::TerminationReason>, String>
where
    P: FnMut() -> Result<ChildPoll, String>,
    N: FnMut() -> Instant,
    S: FnMut(Duration),
{
    loop {
        match poll()? {
            ChildPoll::Terminated(reason) => return Ok(Some(reason)),
            ChildPoll::Running | ChildPoll::Interrupted => {}
        }

        let current = now();
        if current >= deadline {
            return Ok(None);
        }
        sleep(CRASH_REAP_POLL_INTERVAL.min(deadline.saturating_duration_since(current)));
    }
}

fn reap_after_detected_crash_with<P, K, N, S>(
    sigkill_already_sent: bool,
    deadlines: CrashReapDeadlines,
    mut poll: P,
    mut send_sigkill: K,
    mut now: N,
    mut sleep: S,
) -> Result<pipeline::TerminationReason, String>
where
    P: FnMut() -> Result<ChildPoll, String>,
    K: FnMut() -> Result<(), String>,
    N: FnMut() -> Instant,
    S: FnMut(Duration),
{
    if !sigkill_already_sent {
        let deadline = now() + deadlines.before_sigkill;
        if let Some(reason) = poll_until_deadline(&mut poll, deadline, &mut now, &mut sleep)? {
            return Ok(reason);
        }

        eprintln!("[monitor] Child still alive after crash grace period, sending SIGKILL");
        send_sigkill()?;
    }

    let deadline = now() + deadlines.after_sigkill;
    if let Some(reason) = poll_until_deadline(&mut poll, deadline, &mut now, &mut sleep)? {
        return Ok(reason);
    }

    Err(format!(
        "child did not terminate within {}ms after SIGKILL",
        deadlines.after_sigkill.as_millis()
    ))
}

/// Obtain the child's task port while also observing child termination during
/// startup. This replaces the old 50ms heuristic: `posix_spawn` errors are exec
/// setup failures, while every successfully spawned fast exit is a real child
/// `TerminationReason` regardless of how quickly it happens.
fn acquire_task_port_or_termination_with<A, P, N, S>(
    deadline_after: Duration,
    retry_interval: Duration,
    mut acquire: A,
    mut poll_child: P,
    mut now: N,
    mut sleep: S,
) -> Result<TaskAcquisition, String>
where
    A: FnMut() -> Result<mach_port_t, String>,
    P: FnMut() -> Result<Option<pipeline::TerminationReason>, String>,
    N: FnMut() -> Instant,
    S: FnMut(Duration),
{
    let deadline = now() + deadline_after;

    loop {
        match acquire() {
            Ok(task) => return Ok(TaskAcquisition::Acquired(task)),
            Err(last_err) => {
                if let Some(reason) = poll_child()? {
                    return Ok(TaskAcquisition::ChildTerminated(reason));
                }

                let current = now();
                if current >= deadline {
                    return Err(format!(
                        "task_for_pid acquisition deadline expired after {}ms: {last_err}",
                        deadline_after.as_millis()
                    ));
                }
                sleep(retry_interval.min(deadline.saturating_duration_since(current)));
            }
        }
    }
}

fn acquire_task_port_or_termination(
    child_pid: nix::unistd::Pid,
    child_started_at: Instant,
) -> Result<TaskAcquisition, String> {
    acquire_task_port_or_termination_with(
        TASK_ACQUISITION_DEADLINE,
        TASK_ACQUISITION_RETRY_INTERVAL,
        || platform::get_task_for_pid(child_pid.as_raw()).map_err(|error| error.to_string()),
        || poll_child_termination(child_pid, child_started_at),
        Instant::now,
        std::thread::sleep,
    )
}

/// An uninspectable child is not left running indefinitely. The monitor owns
/// the child it spawned, so task-port acquisition failure selects the bounded
/// terminate policy: SIGTERM, a grace deadline, SIGKILL, then a final reap
/// deadline. A failure to observe terminal status is reported without falling
/// back to blocking `waitpid`.
fn terminate_unmonitorable_child_with<T, P, K, N, S>(
    deadlines: CrashReapDeadlines,
    mut send_initial_signal: T,
    poll: P,
    send_sigkill: K,
    now: N,
    sleep: S,
) -> Result<pipeline::TerminationReason, String>
where
    T: FnMut() -> Result<(), String>,
    P: FnMut() -> Result<ChildPoll, String>,
    K: FnMut() -> Result<(), String>,
    N: FnMut() -> Instant,
    S: FnMut(Duration),
{
    send_initial_signal()?;
    reap_after_detected_crash_with(false, deadlines, poll, send_sigkill, now, sleep)
}

/// Own the complete spawned process group from successful spawn through reap.
struct ChildProcessGroup {
    pid: nix::unistd::Pid,
    pgid: nix::unistd::Pid,
    started_at: Instant,
    reaped: bool,
    parent_death_guard: platform::ParentDeathGuard,
}

impl ChildProcessGroup {
    fn new(pid: nix::unistd::Pid, started_at: Instant) -> Result<Self, String> {
        // SAFETY: read-only query of the freshly spawned child.
        let actual_pgid = unsafe { nix::libc::getpgid(pid.as_raw()) };
        if actual_pgid != pid.as_raw() {
            return Err(format!(
                "spawned child {pid} is not its process-group leader (getpgid={actual_pgid})"
            ));
        }
        let parent_death_guard = platform::ParentDeathGuard::install(pid.as_raw())?;
        Ok(Self {
            pid,
            // POSIX_SPAWN_SETPGROUP with pgroup=0 makes the child PID its PGID.
            pgid: pid,
            started_at,
            reaped: false,
            parent_death_guard,
        })
    }

    fn signal(&self, signal: nix::sys::signal::Signal) -> Result<(), String> {
        nix::sys::signal::killpg(self.pgid, signal)
            .or_else(|error| {
                (error == nix::errno::Errno::ESRCH)
                    .then_some(())
                    .ok_or(error)
            })
            .map_err(|error| {
                format!(
                    "failed to send {signal:?} to child process group {}: {error}",
                    self.pgid
                )
            })
    }

    fn mark_reaped(&mut self) {
        self.reaped = true;
        self.parent_death_guard.disarm();
    }

    fn reap_after_crash(
        &mut self,
        sigkill_already_sent: bool,
    ) -> Result<pipeline::TerminationReason, String> {
        let result = reap_after_detected_crash_with(
            sigkill_already_sent,
            CrashReapDeadlines {
                before_sigkill: CRASH_REAP_GRACE_DEADLINE,
                after_sigkill: CRASH_REAP_AFTER_KILL_DEADLINE,
            },
            || poll_child_after_crash_once(self.pid, self.started_at),
            || self.signal(nix::sys::signal::Signal::SIGKILL),
            Instant::now,
            std::thread::sleep,
        );
        if result.is_ok() {
            self.mark_reaped();
        }
        result
    }

    fn terminate_and_reap(
        &mut self,
        signal: nix::sys::signal::Signal,
    ) -> Result<pipeline::TerminationReason, String> {
        let result = terminate_unmonitorable_child_with(
            CrashReapDeadlines {
                before_sigkill: UNMONITORED_TERMINATION_GRACE_DEADLINE,
                after_sigkill: UNMONITORED_REAP_AFTER_KILL_DEADLINE,
            },
            || self.signal(signal),
            || poll_child_after_crash_once(self.pid, self.started_at),
            || self.signal(nix::sys::signal::Signal::SIGKILL),
            Instant::now,
            std::thread::sleep,
        );
        if result.is_ok() {
            self.mark_reaped();
        }
        result
    }
}

impl Drop for ChildProcessGroup {
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        eprintln!(
            "[monitor] lifecycle supervisor cleaning up live child process group {}",
            self.pgid
        );
        if let Err(error) = self.terminate_and_reap(nix::sys::signal::Signal::SIGTERM) {
            eprintln!("[monitor] bounded process-group cleanup failed: {error}");
        }
    }
}

struct MonitorSupervisor {
    child: ChildProcessGroup,
    child_task: platform::OwnedTaskPort,
    exception_port: platform::OwnedExceptionPort,
    event_source: event_source::MacEventSource,
    shared_memory: Option<Arc<shm::SharedMemory>>,
}

impl Drop for MonitorSupervisor {
    fn drop(&mut self) {
        // Wake the exception listener before its receiver and task/SHM owners
        // are released. `destroy` is idempotent with the crash cleanup path.
        self.exception_port.destroy();
    }
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
        return event_loop::EXIT_MONITOR_INTERNAL;
    }

    // Load and normalize enablement once. Every startup branch and the
    // pipeline factory must observe this same immutable policy snapshot.
    let validated_config = match config::load_validated_config() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("[monitor] Invalid crash monitor configuration: {error}");
            return event_loop::EXIT_MONITOR_INTERNAL;
        }
    };
    for diagnostic in validated_config.diagnostics() {
        eprintln!("[monitor] Configuration: {diagnostic}");
    }

    eprintln!("[monitor] Starting: {app_path}");

    // Set up SIGUSR1 signal pipe
    let signal_read_fd = match event_source::setup_signal_pipe() {
        Ok(fd) => fd,
        Err(e) => {
            eprintln!("[monitor] Failed to set up signal pipe: {e}");
            return event_loop::EXIT_MONITOR_INTERNAL;
        }
    };

    // Create exception port BEFORE spawning child.
    // posix_spawn will configure the child to use this port (survives exec).
    let exception_port = match platform::create_exception_port() {
        Ok(port) => platform::OwnedExceptionPort::new(port),
        Err(e) => {
            eprintln!("[monitor] Failed to create exception port: {e}");
            return event_loop::EXIT_MONITOR_INTERNAL;
        }
    };

    // Create shared memory for breadcrumbs/context/screenshots
    let shared_memory = if validated_config.enabled {
        match shm::SharedMemory::create(std::process::id()) {
            Ok(s) => Some(Arc::new(s)),
            Err(e) => {
                eprintln!("[monitor] Shared memory creation failed (continuing without): {e}");
                None
            }
        }
    } else {
        None
    };

    // Build argv and envp for posix_spawn
    let Ok(c_path) = CString::new(app_path) else {
        eprintln!("[monitor] app_path contains null byte");
        return event_loop::EXIT_MONITOR_INTERNAL;
    };
    let mut c_argv_owned: Vec<CString> = vec![c_path.clone()];
    for arg in app_args {
        let Ok(c_arg) = CString::new(arg.as_str()) else {
            eprintln!("[monitor] argument contains null byte: {arg}");
            return event_loop::EXIT_MONITOR_INTERNAL;
        };
        c_argv_owned.push(c_arg);
    }
    let c_argv: Vec<&std::ffi::CStr> = c_argv_owned.iter().map(AsRef::as_ref).collect();

    // Inherit the current environment, replacing (or removing) any stale SHM
    // capability with the mapping created for this child.
    let env_strings = match build_child_environment(
        std::env::vars_os(),
        shared_memory.as_ref().map(|mapping| mapping.name()),
    ) {
        Ok(environment) => environment,
        Err(error) => {
            eprintln!("[monitor] Failed to build child environment: {error}");
            return event_loop::EXIT_MONITOR_INTERNAL;
        }
    };
    let c_envp: Vec<&std::ffi::CStr> = env_strings.iter().map(AsRef::as_ref).collect();

    // Build the runtime registry from the same immutable environment bytes
    // that posix_spawn receives. This prevents EnvironmentCollector from
    // accidentally reporting the monitor's environment.
    let child_environment = Arc::new(collectors::ChildEnvironmentSnapshot::from_c_strings(
        &env_strings,
    ));
    let child_output = Arc::new(platform::ChildOutputCapture::new(
        platform::DEFAULT_CHILD_OUTPUT_TAIL_BYTES,
    ));
    let pl = match pipeline::default_macos_pipeline_from_config_with_runtime(
        shared_memory.clone(),
        &validated_config,
        Some(child_environment),
        Some(child_output.clone()),
    ) {
        Ok(pipeline) => Arc::new(pipeline),
        Err(error) => {
            eprintln!("[monitor] Invalid plugin pipeline: {error}");
            return event_loop::EXIT_MONITOR_INTERNAL;
        }
    };
    match pl.recover_startup_artifacts() {
        Ok(recovery) if recovery.recovered != 0 || recovery.scavenged != 0 => {
            eprintln!(
                "[monitor] startup artifact recovery: {} prepared, {} stale incomplete transaction(s)",
                recovery.recovered, recovery.scavenged
            );
        }
        Ok(_) => {}
        Err(error) => {
            eprintln!("[monitor] artifact recovery failed during startup: {error}");
        }
    }

    // Spawn child with exception port pre-configured (survives exec)
    // Start the runtime clock before entering posix_spawn so a child that
    // exits before the call returns still has its full observable lifetime
    // represented.
    let child_started_at = Instant::now();
    let child_pid_raw = match platform::spawn_with_exception_port_and_output(
        exception_port.raw(),
        &c_path,
        &c_argv,
        &c_envp,
        child_output,
    ) {
        Ok(pid) => pid,
        Err(e) => {
            eprintln!("[monitor] {e}");
            return event_loop::EXIT_MONITOR_INTERNAL;
        }
    };
    let child_pid = nix::unistd::Pid::from_raw(child_pid_raw);
    let mut child_group = match ChildProcessGroup::new(child_pid, child_started_at) {
        Ok(group) => group,
        Err(error) => {
            eprintln!("[monitor] Failed to establish child lifecycle ownership: {error}");
            // The child was already spawned, so fail closed even when the
            // guardian itself cannot be created.
            if nix::sys::signal::killpg(child_pid, nix::sys::signal::Signal::SIGKILL).is_err() {
                let _ = nix::sys::signal::kill(child_pid, nix::sys::signal::Signal::SIGKILL);
            }
            let _ = reap_after_detected_crash_with(
                true,
                CrashReapDeadlines {
                    before_sigkill: Duration::ZERO,
                    after_sigkill: UNMONITORED_REAP_AFTER_KILL_DEADLINE,
                },
                || poll_child_after_crash_once(child_pid, child_started_at),
                || Ok(()),
                Instant::now,
                std::thread::sleep,
            );
            return event_loop::EXIT_MONITOR_INTERNAL;
        }
    };

    eprintln!("[monitor] Child PID: {child_pid}");

    // Extract process name from app_path for reports.
    let process_name = std::path::Path::new(app_path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(app_path);

    // Get child task port for suspend/resume/vm_read (needed for crash introspection + snapshots).
    // Must succeed before starting listener — otherwise early crashes can't be inspected.
    let child_task = match acquire_task_port_or_termination(child_pid, child_started_at) {
        Ok(TaskAcquisition::Acquired(task)) => platform::OwnedTaskPort::new(task),
        Ok(TaskAcquisition::ChildTerminated(reason)) => {
            child_group.mark_reaped();
            #[allow(clippy::cast_sign_loss)]
            let child_pid_u32 = child_pid_raw as u32;
            return event_loop::handle_child_termination(&pl, child_pid_u32, process_name, reason)
                .exit_code();
        }
        Err(e) => {
            eprintln!(
                "[monitor] {e}. Cannot inspect crashes or take snapshots.\n\
                 [monitor] This usually means crash_monitor lacks the debugger entitlement.\n\
                 [monitor] Run `make crash-monitor` to rebuild with codesign."
            );
            // An uninspectable child is terminated and reaped within explicit
            // deadlines. Continuing without a task port would silently lose
            // crash detection; blocking here would hang the monitor for the
            // lifetime of a healthy long-running child.
            match child_group.terminate_and_reap(nix::sys::signal::Signal::SIGTERM) {
                Ok(reason) => {
                    eprintln!("[monitor] Unmonitored child terminated: {reason:?}");
                    #[allow(clippy::cast_sign_loss)]
                    let child_pid_u32 = child_pid_raw as u32;
                    let _ = event_loop::handle_child_termination(
                        &pl,
                        child_pid_u32,
                        process_name,
                        reason,
                    );
                }
                Err(wait_err) => eprintln!("[monitor] bounded child cleanup failed: {wait_err}"),
            }
            return event_loop::EXIT_MONITOR_INTERNAL;
        }
    };

    // Start exception listener thread AFTER task port is acquired
    let exc_rx = platform::start_listener(exception_port.raw());

    eprintln!("[monitor] Monitoring active. Press F8 in app for manual snapshot.");

    // ANR watchdog config (used inline by event_loop, no dedicated thread).
    // Configuration alone does not arm it: the child must publish its first
    // heartbeat and the shared-memory producer-ready handshake. Environment
    // overrides allow E2E tests to use shorter timeouts.
    let anr_config = pl
        .report_enabled(pipeline::ReportType::Anr)
        .then(|| event_loop::AnrConfig {
            warmup_ms: env_u64("CRASH_MONITOR_ANR_WARMUP_MS", 10_000),
            threshold_ms: env_u64("CRASH_MONITOR_ANR_THRESHOLD_MS", 5_000),
            check_interval_ms: env_u64("CRASH_MONITOR_ANR_CHECK_INTERVAL_MS", 2_000),
            cooldown_ms: env_u64("CRASH_MONITOR_ANR_COOLDOWN_MS", 60_000),
        });

    // Build event source from Mac-specific channels
    #[allow(clippy::cast_sign_loss)] // PID is always positive
    let child_pid_u32 = child_pid_raw as u32;
    let source = match event_source::MacEventSource::new(
        exc_rx,
        signal_read_fd,
        child_pid,
        child_started_at,
    ) {
        Ok(source) => source,
        Err(error) => {
            eprintln!("[monitor] failed to initialize event source: {error}");
            return event_loop::EXIT_MONITOR_INTERNAL;
        }
    };
    let mut supervisor = MonitorSupervisor {
        child: child_group,
        child_task,
        exception_port,
        event_source: source,
        shared_memory,
    };

    let event_loop::EventLoopResult {
        mut outcome,
        mut crash_finalization,
        crash_cleanup_required,
        listener_loss_containment_required,
    } = {
        let MonitorSupervisor {
            child_task,
            event_source,
            shared_memory,
            ..
        } = &mut supervisor;
        let target = event_loop::MonitoredTarget::new(
            event_loop::MonitoredTask::new(child_task.raw()),
            event_loop::ProcessId::new(child_pid_u32),
            process_name,
        );
        let reply = |request: &mut platform::ReceivedMachMessage| {
            platform::send_deferred_reply(request).map_err(|error| error.to_string())
        };
        let context = event_loop::EventLoopContext::new(
            &pl,
            target,
            &reply,
            shared_memory.as_ref(),
            anr_config.as_ref(),
        );
        event_loop::event_loop(event_source, context)
    };

    if matches!(&outcome, event_loop::MonitorOutcome::ChildTerminated(_)) {
        supervisor.child.mark_reaped();
    }

    if let event_loop::MonitorOutcome::ShutdownRequested { signal, .. } = &outcome {
        let signal = *signal;
        let termination = nix::sys::signal::Signal::try_from(signal)
            .map_err(|_| format!("unsupported shutdown signal {signal}"))
            .and_then(|signal| supervisor.child.terminate_and_reap(signal));
        outcome = match termination {
            Ok(reason) => outcome.with_shutdown_termination(Some(reason)),
            Err(error) => {
                eprintln!("[monitor] shutdown process-group cleanup failed: {error}");
                event_loop::MonitorOutcome::MonitorFailure(error)
            }
        };
    }

    let task_control_health = pl.platform.supervisor_health();
    let task_control_containment = task_control_health
        .task_control_failures
        .iter()
        .any(platform::TaskControlFailure::prevents_continued_monitoring);
    let task_control_escalation = task_control_health.requires_escalation();
    let must_reap_child =
        crash_cleanup_required || task_control_containment || listener_loss_containment_required;

    // After receiving a crash (including a failed bounded reply), or entering
    // task-control containment, destroy the exception port so that if the
    // child re-faults
    // (e.g. kernel re-executes faulting instruction after KERN_FAILURE reply),
    // there is no Mach exception handler and the kernel falls back to delivering
    // a Unix signal (SIG_DFL → terminate). Without this, the child gets stuck
    // in an uninterruptible Mach exception wait, immune to even SIGKILL.
    if must_reap_child {
        // Destroying the receive right wakes the listener's blocking mach_msg.
        supervisor.exception_port.destroy();
        if !listener_loss_containment_required {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }

    // `task_terminate` is the first containment boundary for a task that
    // could not be resumed. If that Mach operation also failed, escalate once
    // through the process supervisor. Do not block indefinitely waiting for a
    // process whose task-control state is already unreliable.
    let supervisor_sigkill_sent = if listener_loss_containment_required || task_control_escalation {
        if listener_loss_containment_required {
            eprintln!("[monitor] exception listener lost; sending SIGKILL to unmonitored child");
        } else {
            eprintln!("[monitor] task-control recovery exhausted; sending SIGKILL to child");
        }
        match supervisor.child.signal(nix::sys::signal::Signal::SIGKILL) {
            Ok(()) => true,
            Err(error) => {
                eprintln!("[monitor] supervisor SIGKILL escalation failed: {error}");
                false
            }
        }
    } else {
        false
    };

    // Wait for every crashed or task-control-contained child with finite
    // pre/post-SIGKILL deadlines. A fatal ticket is consumed even when the
    // public outcome remains MonitorFailure, preserving captured evidence and
    // TaskResume diagnostics without allowing reaping to block forever.
    if must_reap_child {
        let termination = match supervisor.child.reap_after_crash(supervisor_sigkill_sent) {
            Ok(reason) => {
                eprintln!("[monitor] Contained child terminated: {reason:?}");
                Some(reason)
            }
            Err(e) => {
                eprintln!("[monitor] {e}");
                None
            }
        };

        let report_expected = crash_finalization.is_some();
        let report_path = crash_finalization.take().and_then(|finalization| {
            finalization
                .complete(
                    pl.clone(),
                    termination,
                    pipeline::worker::CRASH_FINALIZE_WAIT,
                )
                .and_then(|diagnostics| diagnostics.report_path)
        });
        if report_expected && report_path.is_none() {
            eprintln!("[monitor] Fatal report finalization did not produce an artifact");
        }
        // This enriches DetectedCrash but deliberately leaves MonitorFailure
        // unchanged, preserving the supervisor-facing containment exit code.
        outcome = outcome.with_crash_result(termination, report_path);
    }

    outcome.exit_code()
}

// ═══════════════════════════════════════════════════
//  Entry point
// ═══════════════════════════════════════════════════

fn main() {
    let cli = Cli::parse();

    let exit_code = match cli.command {
        Some(Commands::CaptureHelper { request_json }) => {
            match pipeline::capture_isolation::run_capture_helper(&request_json) {
                Ok(()) => 0,
                Err(error) => {
                    eprintln!("[capture-helper] {error}");
                    1
                }
            }
        }
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
            // Usage: crash_monitor ./voxelcore_desktop [args...]
            //    or: crash_monitor -- ./voxelcore_desktop [args...]
            if cli.args.is_empty() {
                eprintln!("Usage: crash_monitor [run] <app_path> [args...]");
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::collections::VecDeque;
    use std::os::unix::ffi::OsStringExt;

    const TEST_REAP_DEADLINE: Duration = Duration::from_millis(10);

    fn entitlement_plist(value: &str) -> Vec<u8> {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict><key>{DEBUGGER_ENTITLEMENT}</key>{value}</dict></plist>"#
        )
        .into_bytes()
    }

    #[test]
    fn debugger_entitlement_requires_boolean_true() {
        assert_eq!(
            parse_debugger_entitlement(&entitlement_plist("<true/>")),
            Ok(())
        );
        assert_eq!(
            parse_debugger_entitlement(&entitlement_plist("<false/>")),
            Err(EntitlementCheckError::DebuggerEntitlementFalse)
        );
        assert_eq!(
            parse_debugger_entitlement(&entitlement_plist("<string>true</string>")),
            Err(EntitlementCheckError::DebuggerEntitlementNotBoolean)
        );
    }

    #[test]
    fn debugger_entitlement_distinguishes_missing_and_malformed_plists() {
        assert_eq!(
            parse_debugger_entitlement(b""),
            Err(EntitlementCheckError::MissingDebuggerEntitlement)
        );
        let missing = br#"<?xml version="1.0"?><plist version="1.0"><dict/></plist>"#;
        assert_eq!(
            parse_debugger_entitlement(missing),
            Err(EntitlementCheckError::MissingDebuggerEntitlement)
        );
        assert!(matches!(
            parse_debugger_entitlement(b"not a plist"),
            Err(EntitlementCheckError::MalformedEntitlementPlist(_))
        ));
        let array = br#"<?xml version="1.0"?><plist version="1.0"><array/></plist>"#;
        assert!(matches!(
            parse_debugger_entitlement(array),
            Err(EntitlementCheckError::MalformedEntitlementPlist(_))
        ));
    }

    #[test]
    fn signature_failures_distinguish_unsigned_from_malformed() {
        assert_eq!(
            classify_signature_failure(b"bundle: code object is not signed at all"),
            EntitlementCheckError::UnsignedBinary
        );
        assert_eq!(
            classify_signature_failure(b"bundle: invalid signature (code or signature modified)"),
            EntitlementCheckError::MalformedSignature(
                "bundle: invalid signature (code or signature modified)".to_string()
            )
        );
    }

    fn exited_reason() -> pipeline::TerminationReason {
        pipeline::TerminationReason::Exited {
            exit_code: 23,
            runtime_ms: 17,
        }
    }

    fn signaled_reason() -> pipeline::TerminationReason {
        pipeline::TerminationReason::Signaled {
            signal: 9,
            core_dumped: false,
            runtime_ms: 29,
        }
    }

    #[test]
    fn child_environment_replaces_inherited_shm_and_preserves_non_utf8() {
        let inherited = vec![
            (OsString::from("PATH"), OsString::from("/usr/bin")),
            (
                OsString::from(CRASH_MONITOR_SHM_ENV),
                OsString::from("/stale-one"),
            ),
            (
                OsString::from(CRASH_MONITOR_SHM_ENV),
                OsString::from("/stale-two"),
            ),
            (
                OsString::from_vec(b"NON_UTF8".to_vec()),
                OsString::from_vec(vec![b'v', b'a', b'l', 0xFF]),
            ),
        ];

        let environment = build_child_environment(inherited, Some("/fresh-shm"))
            .expect("valid environment should be encoded");
        let entries: Vec<&[u8]> = environment.iter().map(CString::as_bytes).collect();

        assert_eq!(
            entries
                .iter()
                .filter(|entry| entry.starts_with(b"CRASH_MONITOR_SHM="))
                .count(),
            1
        );
        assert!(entries.contains(&b"CRASH_MONITOR_SHM=/fresh-shm".as_slice()));
        assert!(
            entries.contains(
                &[
                    b'N', b'O', b'N', b'_', b'U', b'T', b'F', b'8', b'=', b'v', b'a', b'l', 0xFF
                ]
                .as_slice()
            )
        );
    }

    #[test]
    fn child_environment_omits_shm_when_mapping_is_unavailable() {
        let inherited = vec![
            (OsString::from("LANG"), OsString::from("ko_KR.UTF-8")),
            (
                OsString::from(CRASH_MONITOR_SHM_ENV),
                OsString::from("/stale"),
            ),
        ];

        let environment =
            build_child_environment(inherited, None).expect("valid environment should be encoded");

        assert!(
            environment
                .iter()
                .all(|entry| { !entry.as_bytes().starts_with(b"CRASH_MONITOR_SHM=") })
        );
        assert!(
            environment
                .iter()
                .any(|entry| entry.as_bytes() == b"LANG=ko_KR.UTF-8")
        );
    }

    #[test]
    fn task_port_entitlement_failure_expires_at_the_configured_deadline() {
        let base = Instant::now();
        let elapsed = Cell::new(Duration::ZERO);
        let mut attempts = 0;

        let result = acquire_task_port_or_termination_with(
            TEST_REAP_DEADLINE,
            TEST_REAP_DEADLINE,
            || {
                attempts += 1;
                Err("task_for_pid failed: KERN_FAILURE (missing entitlement)".to_string())
            },
            || Ok(None),
            || base + elapsed.get(),
            |duration| elapsed.set(elapsed.get() + duration),
        );

        let Err(error) = result else {
            panic!("missing entitlement must not acquire a task port");
        };
        assert!(error.contains("acquisition deadline expired"));
        assert!(error.contains("missing entitlement"));
        assert_eq!(elapsed.get(), TEST_REAP_DEADLINE);
        assert_eq!(attempts, 2);
    }

    #[test]
    fn long_running_unmonitorable_child_is_terminated_and_reaped_boundedly() {
        let base = Instant::now();
        let elapsed = Cell::new(Duration::ZERO);
        let mut polls = VecDeque::from([
            ChildPoll::Running,
            ChildPoll::Running,
            ChildPoll::Terminated(signaled_reason()),
        ]);
        let mut sigterm_count = 0;
        let mut sigkill_count = 0;

        let reason = terminate_unmonitorable_child_with(
            CrashReapDeadlines {
                before_sigkill: TEST_REAP_DEADLINE,
                after_sigkill: TEST_REAP_DEADLINE,
            },
            || {
                sigterm_count += 1;
                Ok(())
            },
            || Ok(polls.pop_front().unwrap_or(ChildPoll::Running)),
            || {
                sigkill_count += 1;
                Ok(())
            },
            || base + elapsed.get(),
            |duration| elapsed.set(elapsed.get() + duration),
        )
        .expect("child must be reaped within the post-SIGKILL deadline");

        assert_eq!(reason, signaled_reason());
        assert_eq!(sigterm_count, 1);
        assert_eq!(sigkill_count, 1);
        assert_eq!(elapsed.get(), TEST_REAP_DEADLINE);
    }

    #[test]
    fn child_early_exit_wins_over_task_port_retry_failure() {
        let base = Instant::now();
        let elapsed = Cell::new(Duration::ZERO);
        let mut polls = VecDeque::from([None, Some(exited_reason())]);
        let mut attempts = 0;

        let result = acquire_task_port_or_termination_with(
            Duration::from_secs(30),
            TEST_REAP_DEADLINE,
            || {
                attempts += 1;
                Err("task_for_pid raced child startup".to_string())
            },
            || Ok(polls.pop_front().flatten()),
            || base + elapsed.get(),
            |duration| elapsed.set(elapsed.get() + duration),
        )
        .expect("early child exit must be preserved as terminal status");

        match result {
            TaskAcquisition::ChildTerminated(reason) => assert_eq!(reason, exited_reason()),
            TaskAcquisition::Acquired(_) => panic!("task port unexpectedly acquired"),
        }
        assert_eq!(attempts, 2);
        assert_eq!(elapsed.get(), TEST_REAP_DEADLINE);
    }

    #[test]
    fn crash_reap_returns_pre_kill_termination_without_sigkill() {
        let base = Instant::now();
        let elapsed = Cell::new(Duration::ZERO);
        let mut polls =
            VecDeque::from([ChildPoll::Running, ChildPoll::Terminated(exited_reason())]);
        let mut sigkill_count = 0;

        let reason = reap_after_detected_crash_with(
            false,
            CrashReapDeadlines {
                before_sigkill: TEST_REAP_DEADLINE,
                after_sigkill: TEST_REAP_DEADLINE,
            },
            || Ok(polls.pop_front().unwrap_or(ChildPoll::Running)),
            || {
                sigkill_count += 1;
                Ok(())
            },
            || base + elapsed.get(),
            |duration| elapsed.set(elapsed.get() + duration),
        )
        .expect("child should be reaped during the pre-kill grace period");

        assert_eq!(reason, exited_reason());
        assert_eq!(sigkill_count, 0);
    }

    #[test]
    fn crash_reap_returns_post_kill_termination() {
        let base = Instant::now();
        let elapsed = Cell::new(Duration::ZERO);
        let mut polls = VecDeque::from([
            ChildPoll::Running,
            ChildPoll::Running,
            ChildPoll::Interrupted,
            ChildPoll::Terminated(signaled_reason()),
        ]);
        let mut sigkill_count = 0;

        let reason = reap_after_detected_crash_with(
            false,
            CrashReapDeadlines {
                before_sigkill: TEST_REAP_DEADLINE,
                after_sigkill: TEST_REAP_DEADLINE,
            },
            || Ok(polls.pop_front().unwrap_or(ChildPoll::Running)),
            || {
                sigkill_count += 1;
                Ok(())
            },
            || base + elapsed.get(),
            |duration| elapsed.set(elapsed.get() + duration),
        )
        .expect("child should be reaped during the post-kill deadline");

        assert_eq!(reason, signaled_reason());
        assert_eq!(sigkill_count, 1);
    }

    #[test]
    fn crash_reap_bounds_repeated_eintr_after_an_existing_sigkill() {
        let base = Instant::now();
        let elapsed = Cell::new(Duration::ZERO);
        let mut poll_count = 0;
        let mut sigkill_count = 0;

        let error = reap_after_detected_crash_with(
            true,
            CrashReapDeadlines {
                before_sigkill: Duration::from_secs(30),
                after_sigkill: TEST_REAP_DEADLINE,
            },
            || {
                poll_count += 1;
                Ok(ChildPoll::Interrupted)
            },
            || {
                sigkill_count += 1;
                Ok(())
            },
            || base + elapsed.get(),
            |duration| elapsed.set(elapsed.get() + duration),
        )
        .expect_err("post-kill EINTR must time out instead of blocking");

        assert!(error.contains("after SIGKILL"));
        assert_eq!(elapsed.get(), TEST_REAP_DEADLINE);
        assert_eq!(poll_count, 2);
        assert_eq!(sigkill_count, 0);
    }
}
