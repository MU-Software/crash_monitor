//! Safety utilities: panic-catching plugin wrapper, RAII port guard,
//! and Stage 1 raw data writer (fail-safe dump).

use crate::collectors::thread::RawThreadData;
use crate::platform::PlatformOps;

use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::{ArtifactKind, ArtifactTransaction};

// ═══════════════════════════════════════════════════
//  Deadline-aware plugin execution
// ═══════════════════════════════════════════════════

const SUBPROCESS_POLL_INTERVAL: Duration = Duration::from_millis(10);
const OUTPUT_READER_POLL_INTERVAL: Duration = Duration::from_millis(5);
const OUTPUT_DRAIN_GRACE: Duration = Duration::from_secs(2);
const PROCESS_EXIT_GRACE: Duration = Duration::from_secs(2);
const MAX_SUBPROCESS_STREAM_BYTES: usize = 1024 * 1024;
const MAX_STOP_DRAIN_READS: usize = MAX_SUBPROCESS_STREAM_BYTES / 8192;

/// Cloneable cancellation flag passed to cooperative plugins.
///
/// Cancellation never relies on a process-global signal. A cooperative plugin
/// checks the associated [`PluginContext`] at bounded work boundaries and
/// returns promptly when this flag is set.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
}

impl CancellationToken {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn from_atomic(cancelled: Arc<AtomicBool>) -> Self {
        Self { cancelled }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

/// Per-invocation deadline and cancellation state for an in-process plugin.
///
/// The context is deliberately local to one invocation. It does not install a
/// signal handler, arm a process timer, or mutate any other process-global
/// state. Code that cannot cooperate with this context must execute through
/// [`run_plugin_subprocess`] instead.
#[derive(Clone, Debug)]
pub struct PluginContext {
    deadline: Option<Instant>,
    cancellation: CancellationToken,
    /// Immutable shared-memory payload captured before the target resumed.
    ///
    /// Keeping this on the per-invocation context prevents collectors from
    /// retaining or reaching back into the live mapping after capture.
    shm_snapshot: Option<Arc<crate::shm::OwnedShmSnapshot>>,
    /// Event-scoped artifact transaction used by finalization plugins.
    artifact_transaction: Option<Arc<crate::pipeline::ArtifactTransaction>>,
    report_context: Option<Arc<crate::pipeline::ReportContext>>,
    subprocess_boundary: Arc<AtomicU8>,
    subprocess_cleanup_failure: Arc<OnceLock<String>>,
}

const SUBPROCESS_UNUSED: u8 = 0;
const SUBPROCESS_STARTED: u8 = 1;
const SUBPROCESS_NOT_REQUIRED: u8 = 2;

impl PluginContext {
    #[must_use]
    pub fn without_deadline() -> Self {
        Self {
            deadline: None,
            cancellation: CancellationToken::new(),
            shm_snapshot: None,
            artifact_transaction: None,
            report_context: None,
            subprocess_boundary: Arc::new(AtomicU8::new(SUBPROCESS_UNUSED)),
            subprocess_cleanup_failure: Arc::new(OnceLock::new()),
        }
    }

    #[must_use]
    pub fn with_timeout(timeout: Duration) -> Self {
        let now = Instant::now();
        Self {
            // Overflow cannot sensibly mean an unbounded timeout. Treat an
            // unrepresentable deadline as already expired instead.
            deadline: Some(now.checked_add(timeout).unwrap_or(now)),
            cancellation: CancellationToken::new(),
            shm_snapshot: None,
            artifact_transaction: None,
            report_context: None,
            subprocess_boundary: Arc::new(AtomicU8::new(SUBPROCESS_UNUSED)),
            subprocess_cleanup_failure: Arc::new(OnceLock::new()),
        }
    }

    #[must_use]
    pub fn from_timeout(timeout: Option<Duration>) -> Self {
        timeout.map_or_else(Self::without_deadline, Self::with_timeout)
    }

    pub(crate) fn from_timeout_and_cancellation(
        timeout: Option<Duration>,
        cancellation: CancellationToken,
    ) -> Self {
        let now = Instant::now();
        Self {
            deadline: timeout.map(|timeout| now.checked_add(timeout).unwrap_or(now)),
            cancellation,
            shm_snapshot: None,
            artifact_transaction: None,
            report_context: None,
            subprocess_boundary: Arc::new(AtomicU8::new(SUBPROCESS_UNUSED)),
            subprocess_cleanup_failure: Arc::new(OnceLock::new()),
        }
    }

    /// Attach the immutable shared-memory payload for this plugin invocation.
    #[must_use]
    pub(crate) fn with_shm_snapshot(
        mut self,
        snapshot: Option<Arc<crate::shm::OwnedShmSnapshot>>,
    ) -> Self {
        self.shm_snapshot = snapshot;
        self
    }

    #[must_use]
    pub(crate) fn with_artifact_transaction(
        mut self,
        transaction: Arc<crate::pipeline::ArtifactTransaction>,
    ) -> Self {
        self.report_context = Some(transaction.report_context_arc());
        self.artifact_transaction = Some(transaction);
        self
    }

    #[must_use]
    pub(crate) fn with_report_context(
        mut self,
        report_context: Arc<crate::pipeline::ReportContext>,
    ) -> Self {
        self.report_context = Some(report_context);
        self
    }

    /// Return the immutable report context shared by all finalization stages.
    #[must_use]
    pub fn report_context(&self) -> Option<&crate::pipeline::ReportContext> {
        self.report_context.as_deref()
    }

    /// Return the final immutable destination after atomic report publish.
    #[must_use]
    pub fn committed_report(&self) -> Option<crate::pipeline::CommittedReport> {
        self.artifact_transaction
            .as_deref()
            .and_then(crate::pipeline::ArtifactTransaction::committed_report)
    }

    pub(crate) fn artifact_transaction(&self) -> Option<&crate::pipeline::ArtifactTransaction> {
        self.artifact_transaction.as_deref()
    }

    /// Return the shared-memory payload captured before task resume.
    ///
    /// `None` means shared memory was unavailable, invalid, or could not be
    /// snapshotted under an owned task suspension.
    #[must_use]
    pub fn shm_snapshot(&self) -> Option<&crate::shm::OwnedShmSnapshot> {
        self.shm_snapshot.as_deref()
    }

    /// Return a context that shares cancellation state and cannot outlive the
    /// current context or the supplied relative timeout.
    #[must_use]
    pub fn bounded_by(&self, timeout: Duration) -> Self {
        let now = Instant::now();
        let local_deadline = now.checked_add(timeout).unwrap_or(now);
        Self {
            deadline: Some(
                self.deadline
                    .map_or(local_deadline, |deadline| deadline.min(local_deadline)),
            ),
            cancellation: self.cancellation.clone(),
            shm_snapshot: self.shm_snapshot.clone(),
            artifact_transaction: self.artifact_transaction.clone(),
            report_context: self.report_context.clone(),
            subprocess_boundary: self.subprocess_boundary.clone(),
            subprocess_cleanup_failure: self.subprocess_cleanup_failure.clone(),
        }
    }

    #[must_use]
    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    pub(crate) fn mark_subprocess_not_required(&self) {
        self.subprocess_boundary
            .store(SUBPROCESS_NOT_REQUIRED, Ordering::Release);
    }

    pub(crate) fn subprocess_boundary_satisfied(&self) -> bool {
        self.subprocess_boundary.load(Ordering::Acquire) != SUBPROCESS_UNUSED
    }

    fn mark_subprocess_started(&self) {
        self.subprocess_boundary
            .store(SUBPROCESS_STARTED, Ordering::Release);
    }

    fn record_subprocess_cleanup_failure(&self, error: String) {
        let _ = self.subprocess_cleanup_failure.set(error);
    }

    fn subprocess_cleanup_failure(&self) -> Option<&str> {
        self.subprocess_cleanup_failure
            .get()
            .map(std::string::String::as_str)
    }

    #[must_use]
    pub fn is_timed_out(&self) -> bool {
        self.cancellation.is_cancelled()
            || self
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
    }

    #[must_use]
    pub fn remaining(&self) -> Option<Duration> {
        self.deadline
            .map(|deadline| deadline.saturating_duration_since(Instant::now()))
    }

    /// Checkpoint for cooperative plugins.
    ///
    /// # Errors
    /// Returns a stable error string once the deadline or cancellation token
    /// has fired. The outer runner classifies that invocation as `TimedOut`.
    pub fn checkpoint(&self) -> Result<(), String> {
        if self.is_timed_out() {
            Err("plugin deadline reached".to_string())
        } else {
            Ok(())
        }
    }
}

/// Outcome of one guarded plugin invocation.
#[derive(Debug)]
pub enum PluginRunResult<T> {
    Completed(T),
    Failed(String),
    Panicked(String),
    TimedOut,
}

impl<T> PluginRunResult<T> {
    #[must_use]
    pub fn into_option(self) -> Option<T> {
        match self {
            Self::Completed(value) => Some(value),
            Self::Failed(_) | Self::Panicked(_) | Self::TimedOut => None,
        }
    }
}

/// Execute a cooperative plugin in-process with panic and deadline diagnosis.
///
/// This function does not create a helper thread: borrowed mutable pipeline
/// state therefore cannot outlive its owner. The plugin must call
/// [`PluginContext::checkpoint`] during long-running work. Non-cooperative or
/// untrusted work belongs in [`run_plugin_subprocess`], where timeout means the
/// operating system has terminated the owned process group and the supervisor
/// has reaped its direct child.
pub fn run_plugin_cooperative<T>(
    name: &str,
    context: &PluginContext,
    f: impl FnOnce(&PluginContext) -> Result<T, String>,
) -> PluginRunResult<T> {
    if context.is_timed_out() {
        eprintln!("[monitor] plugin {name} timed out before it started");
        return PluginRunResult::TimedOut;
    }

    let result = std::panic::catch_unwind(AssertUnwindSafe(|| f(context)));

    // A deadline does not imply isolation succeeded. Preserve a supervisor's
    // kill/reap failure instead of relabeling it TimedOut, because TimedOut is
    // only safe to report after the direct child has been reaped.
    if let Some(error) = context.subprocess_cleanup_failure() {
        eprintln!("[monitor] plugin {name} subprocess cleanup failed: {error}");
        return PluginRunResult::Failed(error.to_string());
    }

    // A plugin may discover expiry at a checkpoint and return an ordinary
    // error. Inspect the context first so expiry is never mislabeled Error.
    if context.is_timed_out() {
        eprintln!("[monitor] plugin {name} timed out");
        return PluginRunResult::TimedOut;
    }

    match result {
        Ok(Ok(value)) => PluginRunResult::Completed(value),
        Ok(Err(error)) => {
            eprintln!("[monitor] plugin {name}: {error}");
            PluginRunResult::Failed(error)
        }
        Err(payload) => {
            let message = panic_payload_message(payload.as_ref());
            eprintln!("[monitor] plugin {name} panicked: {message}");
            PluginRunResult::Panicked(message)
        }
    }
}

fn panic_payload_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| {
            payload
                .downcast_ref::<&'static str>()
                .map(ToString::to_string)
        })
        .unwrap_or_else(|| format!("non-string panic payload ({:?})", payload.type_id()))
}

/// Execute a plugin closure with panic catching and no process-global timeout.
///
/// Compatibility wrapper for callers that only need panic isolation and have
/// no deadline. New pipeline code should use [`run_plugin_cooperative`] with an
/// explicit context.
pub fn run_plugin_catching_panics<T>(
    name: &str,
    f: impl FnOnce() -> Result<T, String>,
) -> Option<T> {
    run_plugin_cooperative(name, &PluginContext::without_deadline(), |_| f()).into_option()
}

/// Captured output from a completed isolated plugin process.
#[derive(Debug)]
pub struct SubprocessOutput {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
}

struct StreamCapture {
    bytes: Vec<u8>,
    truncated: bool,
    error: Option<String>,
}

fn set_nonblocking(reader: &impl AsRawFd) -> Result<(), String> {
    let fd = reader.as_raw_fd();
    // SAFETY: `fd` is owned by the live ChildStdout/ChildStderr value. F_GETFL
    // only reads descriptor flags and F_SETFL preserves every existing flag.
    let flags = unsafe { nix::libc::fcntl(fd, nix::libc::F_GETFL) };
    if flags == -1 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    // SAFETY: the descriptor remains live for the call and O_NONBLOCK is a
    // valid status flag for a pipe.
    if unsafe { nix::libc::fcntl(fd, nix::libc::F_SETFL, flags | nix::libc::O_NONBLOCK) } == -1 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(())
}

fn read_capped(mut reader: impl Read, stop: &AtomicBool) -> StreamCapture {
    let mut bytes = Vec::new();
    let mut truncated = false;
    let mut buffer = [0_u8; 8192];
    let mut reads_after_stop = 0;
    loop {
        let stopping = stop.load(Ordering::Acquire);
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                let remaining = MAX_SUBPROCESS_STREAM_BYTES.saturating_sub(bytes.len());
                let retained = remaining.min(count);
                bytes.extend_from_slice(&buffer[..retained]);
                truncated |= retained < count;
                // Once cancellation is requested, still consume data that is
                // already queued so a closed pipe can reach EOF. Bound those
                // reads in case an escaped writer continuously refills it.
                if stopping {
                    reads_after_stop += 1;
                }
                if reads_after_stop >= MAX_STOP_DRAIN_READS {
                    return StreamCapture {
                        bytes,
                        truncated,
                        error: Some("output capture stopped before EOF".to_string()),
                    };
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if stop.load(Ordering::Acquire) {
                    return StreamCapture {
                        bytes,
                        truncated,
                        error: Some("output capture stopped before EOF".to_string()),
                    };
                }
                std::thread::sleep(OUTPUT_READER_POLL_INTERVAL);
            }
            Err(error) => {
                return StreamCapture {
                    bytes,
                    truncated,
                    error: Some(error.to_string()),
                };
            }
        }
    }
    StreamCapture {
        bytes,
        truncated,
        error: None,
    }
}

fn join_capture(
    name: &str,
    stream: &str,
    handle: JoinHandle<StreamCapture>,
    stop: &AtomicBool,
    drain_deadline: Option<Instant>,
) -> StreamCapture {
    if let Some(deadline) = drain_deadline {
        while !handle.is_finished() && Instant::now() < deadline {
            std::thread::sleep(OUTPUT_READER_POLL_INTERVAL);
        }
    }
    if !handle.is_finished() {
        // A descendant may have escaped the owned process group while keeping
        // an inherited pipe open. Nonblocking readers make this cancellation
        // bounded even in that case.
        stop.store(true, Ordering::Release);
    }
    if let Ok(capture) = handle.join() {
        if let Some(error) = &capture.error {
            eprintln!("[monitor] plugin {name} {stream} read failed: {error}");
        }
        capture
    } else {
        eprintln!("[monitor] plugin {name} {stream} reader panicked");
        StreamCapture {
            bytes: Vec::new(),
            truncated: false,
            error: Some("reader panicked".to_string()),
        }
    }
}

fn kill_and_reap_process_group(
    child: &mut std::process::Child,
    pgid: nix::unistd::Pid,
) -> Result<(), String> {
    let group_error = match nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGKILL) {
        Ok(()) | Err(nix::errno::Errno::ESRCH) => None,
        Err(error) => Some(format!(
            "failed to kill plugin process group {pgid}: {error}"
        )),
    };
    // The direct child may have changed its process group. The owned Child
    // handle remains authoritative even if group delivery reported success.
    let _ = child.kill();
    wait_for_child_exit(child, pgid, PROCESS_EXIT_GRACE)?;
    if let Some(error) = group_error {
        Err(error)
    } else {
        Ok(())
    }
}

fn wait_for_child(
    child: &mut std::process::Child,
    pid: nix::unistd::Pid,
    grace: Duration,
) -> Result<ExitStatus, String> {
    let deadline = Instant::now()
        .checked_add(grace)
        .unwrap_or_else(Instant::now);
    loop {
        match child.wait() {
            Ok(status) => return Ok(status),
            Err(error)
                if error.kind() == std::io::ErrorKind::Interrupted && Instant::now() < deadline => {
            }
            Err(error) => {
                let message = format!("failed to reap plugin process {pid}: {error}");
                eprintln!("[monitor] {message}");
                return Err(message);
            }
        }
    }
}

fn wait_for_child_exit(
    child: &mut std::process::Child,
    pid: nix::unistd::Pid,
    grace: Duration,
) -> Result<ExitStatus, String> {
    let deadline = Instant::now()
        .checked_add(grace)
        .unwrap_or_else(Instant::now);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) => {}
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => {
                return Err(format!("failed to poll plugin process {pid}: {error}"));
            }
        }
        if Instant::now() >= deadline {
            let message = format!("plugin process {pid} did not exit within cleanup grace");
            return match spawn_late_reaper(pid) {
                Ok(()) => Err(format!(
                    "{message}; direct-child reaping was handed to a background waiter"
                )),
                Err(error) => Err(format!(
                    "{message}; could not retain reaping ownership: {error}"
                )),
            };
        }
        std::thread::sleep(SUBPROCESS_POLL_INTERVAL);
    }
}

/// Keep wait ownership when a killed child does not become waitable within the
/// caller's bounded cleanup grace. An unreaped child cannot reuse its PID, so a
/// dedicated waiter may safely block until the kernel eventually releases it.
fn spawn_late_reaper(pid: nix::unistd::Pid) -> Result<(), String> {
    std::thread::Builder::new()
        .name(format!("plugin-reaper-{pid}"))
        .spawn(move || {
            loop {
                match nix::sys::wait::waitpid(pid, None) {
                    Ok(_) | Err(nix::errno::Errno::ECHILD) => return,
                    Err(nix::errno::Errno::EINTR) => {}
                    Err(error) => {
                        eprintln!("[monitor] failed to reap delayed plugin process {pid}: {error}");
                        return;
                    }
                }
            }
        })
        .map(|_| ())
        .map_err(|error| format!("failed to spawn delayed plugin reaper: {error}"))
}

/// Observe child exit without reaping it. Keeping the direct child as a zombie
/// reserves its PID/process-group identifier while we clean up descendants,
/// avoiding a kill-after-reap PID-reuse race.
fn child_exited_unreaped(raw_pid: i32) -> Result<bool, String> {
    let wait_pid = nix::libc::id_t::try_from(raw_pid)
        .map_err(|error| format!("plugin pid does not fit id_t: {error}"))?;
    // SAFETY: waitid initializes `info` on success. WNOWAIT leaves the
    // selected child waitable so the owned Child can reap it afterward.
    let (result, info) = unsafe {
        let mut info = std::mem::zeroed::<nix::libc::siginfo_t>();
        let result = nix::libc::waitid(
            nix::libc::P_PID,
            wait_pid,
            std::ptr::addr_of_mut!(info),
            nix::libc::WEXITED | nix::libc::WNOHANG | nix::libc::WNOWAIT,
        );
        (result, info)
    };
    if result == 0 {
        // SAFETY: `info` was initialized by successful waitid; si_pid == 0
        // is the specified WNOHANG "no status available" result.
        return Ok(unsafe { info.si_pid() } != 0);
    }
    let error = std::io::Error::last_os_error();
    if error.kind() == std::io::ErrorKind::Interrupted {
        Ok(false)
    } else {
        Err(format!("failed to poll isolated plugin: {error}"))
    }
}

fn kill_remaining_process_group(pgid: nix::unistd::Pid) -> Result<(), String> {
    match nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGKILL) {
        Ok(()) | Err(nix::errno::Errno::ESRCH | nix::errno::Errno::EPERM) => {
            // Darwin reports EPERM when the observed group contains only the
            // unreaped zombie leader and therefore has no signalable member.
            // A live same-credential descendant is signalable and produces
            // Ok; helpers that change credentials are outside this lifecycle
            // boundary's trust model.
            Ok(())
        }
        Err(error) => {
            let message = format!("failed to clean plugin process group {pgid}: {error}");
            eprintln!("[monitor] {message}");
            Err(message)
        }
    }
}

fn failed_for_context<T>(
    context: &PluginContext,
    message: impl Into<String>,
) -> PluginRunResult<T> {
    if context.is_timed_out() {
        PluginRunResult::TimedOut
    } else {
        PluginRunResult::Failed(message.into())
    }
}

fn failed_with_cleanup<T>(
    context: &PluginContext,
    message: impl Into<String>,
    cleanup: Result<(), String>,
) -> PluginRunResult<T> {
    let message = message.into();
    match cleanup {
        Ok(()) => failed_for_context(context, message),
        Err(cleanup_error) => {
            let failure = format!("{message}; cleanup failed: {cleanup_error}");
            context.record_subprocess_cleanup_failure(failure.clone());
            PluginRunResult::Failed(failure)
        }
    }
}

/// Execute non-cooperative or untrusted plugin work in a killable process.
///
/// The command is placed in its own process group. Stdout and stderr are
/// drained concurrently (retaining at most 1 MiB per stream), so a verbose
/// child cannot deadlock on a full pipe. Deadline expiry kills the entire group
/// with `SIGKILL` and reaps the direct child before returning `TimedOut`.
/// This function never changes the monitor's signal disposition.
#[allow(clippy::too_many_lines)]
pub fn run_plugin_subprocess(
    name: &str,
    command: &mut Command,
    context: &PluginContext,
) -> PluginRunResult<SubprocessOutput> {
    context.mark_subprocess_started();
    if context.is_timed_out() {
        eprintln!("[monitor] plugin {name} timed out before process spawn");
        return PluginRunResult::TimedOut;
    }

    command
        .process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let message = format!("failed to spawn isolated plugin: {error}");
            eprintln!("[monitor] plugin {name}: {message}");
            return failed_for_context(context, message);
        }
    };

    let raw_pid = match i32::try_from(child.id()) {
        Ok(pid) => pid,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            let message = format!("plugin pid does not fit pid_t: {error}");
            eprintln!("[monitor] plugin {name}: {message}");
            return PluginRunResult::Failed(message);
        }
    };
    let pgid = nix::unistd::Pid::from_raw(raw_pid);

    let Some(stdout) = child.stdout.take() else {
        let cleanup = kill_and_reap_process_group(&mut child, pgid);
        return failed_with_cleanup(context, "isolated plugin stdout pipe missing", cleanup);
    };
    let Some(stderr) = child.stderr.take() else {
        let cleanup = kill_and_reap_process_group(&mut child, pgid);
        return failed_with_cleanup(context, "isolated plugin stderr pipe missing", cleanup);
    };
    if let Err(error) = set_nonblocking(&stdout) {
        let cleanup = kill_and_reap_process_group(&mut child, pgid);
        return failed_with_cleanup(
            context,
            format!("failed to make isolated plugin stdout nonblocking: {error}"),
            cleanup,
        );
    }
    if let Err(error) = set_nonblocking(&stderr) {
        let cleanup = kill_and_reap_process_group(&mut child, pgid);
        return failed_with_cleanup(
            context,
            format!("failed to make isolated plugin stderr nonblocking: {error}"),
            cleanup,
        );
    }
    let stdout_stop = Arc::new(AtomicBool::new(false));
    let stdout_reader_stop = stdout_stop.clone();
    let stdout_reader = std::thread::Builder::new()
        .name(format!("{name}-stdout"))
        .spawn(move || read_capped(stdout, &stdout_reader_stop));
    let stdout_reader = match stdout_reader {
        Ok(handle) => handle,
        Err(error) => {
            let cleanup = kill_and_reap_process_group(&mut child, pgid);
            return failed_with_cleanup(
                context,
                format!("failed to spawn stdout reader: {error}"),
                cleanup,
            );
        }
    };
    let stderr_stop = Arc::new(AtomicBool::new(false));
    let stderr_reader_stop = stderr_stop.clone();
    let stderr_reader = std::thread::Builder::new()
        .name(format!("{name}-stderr"))
        .spawn(move || read_capped(stderr, &stderr_reader_stop));
    let stderr_reader = match stderr_reader {
        Ok(handle) => handle,
        Err(error) => {
            stdout_stop.store(true, Ordering::Release);
            let cleanup = kill_and_reap_process_group(&mut child, pgid);
            let _ = join_capture(name, "stdout", stdout_reader, &stdout_stop, None);
            return failed_with_cleanup(
                context,
                format!("failed to spawn stderr reader: {error}"),
                cleanup,
            );
        }
    };

    let outcome = loop {
        if context.is_timed_out() {
            eprintln!("[monitor] plugin {name} timed out; killing process group {pgid}");
            break match kill_and_reap_process_group(&mut child, pgid) {
                Ok(()) => Err(PluginRunResult::TimedOut),
                Err(error) => {
                    let failure = format!("plugin timed out but cleanup failed: {error}");
                    context.record_subprocess_cleanup_failure(failure.clone());
                    Err(PluginRunResult::Failed(failure))
                }
            };
        }

        match child_exited_unreaped(raw_pid) {
            Ok(true) => {
                // The unreaped direct child keeps PGID reuse impossible while
                // we terminate any descendants that inherited its group.
                let cleanup = kill_remaining_process_group(pgid);
                let reaped = wait_for_child(&mut child, pgid, PROCESS_EXIT_GRACE);
                match (cleanup, reaped) {
                    (Ok(()), Ok(status)) => break Ok(status),
                    (Err(cleanup_error), Ok(_)) => {
                        context.record_subprocess_cleanup_failure(cleanup_error.clone());
                        break Err(PluginRunResult::Failed(cleanup_error));
                    }
                    (Ok(()), Err(reap_error)) => {
                        context.record_subprocess_cleanup_failure(reap_error.clone());
                        break Err(PluginRunResult::Failed(reap_error));
                    }
                    (Err(cleanup_error), Err(reap_error)) => {
                        let failure = format!("{cleanup_error}; {reap_error}");
                        context.record_subprocess_cleanup_failure(failure.clone());
                        break Err(PluginRunResult::Failed(failure));
                    }
                }
            }
            Ok(false) => {}
            Err(error) => {
                let cleanup = kill_and_reap_process_group(&mut child, pgid);
                break Err(failed_with_cleanup(context, error, cleanup));
            }
        }

        let sleep_for = context
            .remaining()
            .map_or(SUBPROCESS_POLL_INTERVAL, |remaining| {
                remaining.min(SUBPROCESS_POLL_INTERVAL)
            });
        if !sleep_for.is_zero() {
            std::thread::sleep(sleep_for);
        }
    };

    let allow_drain = outcome.is_ok();
    if !allow_drain {
        stdout_stop.store(true, Ordering::Release);
        stderr_stop.store(true, Ordering::Release);
    }
    // Both reader threads drain concurrently. Give them one shared grace
    // deadline so a busy stdout reader cannot either cancel stderr or double
    // the total bounded wait.
    let drain_deadline = allow_drain.then(|| Instant::now() + OUTPUT_DRAIN_GRACE);
    let stdout = join_capture(name, "stdout", stdout_reader, &stdout_stop, drain_deadline);
    let stderr = join_capture(name, "stderr", stderr_reader, &stderr_stop, drain_deadline);

    if outcome.is_ok() && context.is_timed_out() {
        eprintln!("[monitor] plugin {name} completed after its deadline");
        return PluginRunResult::TimedOut;
    }

    if outcome.is_ok()
        && let Some(error) = stdout.error.as_ref().or(stderr.error.as_ref())
    {
        return PluginRunResult::Failed(format!(
            "isolated plugin output capture was incomplete: {error}"
        ));
    }

    match outcome {
        Ok(status) => PluginRunResult::Completed(SubprocessOutput {
            status,
            stdout: stdout.bytes,
            stderr: stderr.bytes,
            stdout_truncated: stdout.truncated,
            stderr_truncated: stderr.truncated,
        }),
        Err(result) => result,
    }
}

// ═══════════════════════════════════════════════════
//  PortGuard
// ═══════════════════════════════════════════════════

/// Ensures thread port send rights are deallocated even if the pipeline panics.
/// Owns a copy of port values (not a borrow) so it doesn't conflict with &mut data.
pub struct PortGuard {
    ports: Vec<u32>,
    platform: Arc<dyn PlatformOps>,
}

impl PortGuard {
    pub fn new(ports: Vec<u32>, platform: Arc<dyn PlatformOps>) -> Self {
        Self { ports, platform }
    }
}

impl Drop for PortGuard {
    fn drop(&mut self) {
        for &port in &self.ports {
            self.platform.deallocate_thread_port(port);
        }
    }
}

// ═══════════════════════════════════════════════════
//  Stage 1: Critical raw data (must succeed)
// ═══════════════════════════════════════════════════

/// Write raw register + backtrace data for ALL threads immediately to disk.
/// This is Stage 1 of the fail-safe cascade.
///
/// # Errors
/// Returns an error if file creation or write I/O fails.
pub fn write_raw_stage1(
    transaction: &ArtifactTransaction,
    threads: &[RawThreadData],
) -> Result<PathBuf, String> {
    let sanitizer = crate::preprocessors::Sanitizer::new();
    transaction.write_artifact("threads.txt", ArtifactKind::ThreadRaw, |file| {
        for (i, thread) in threads.iter().enumerate() {
            let mut thread_name = thread.name.clone();
            if let Some(name) = &mut thread_name {
                sanitizer.sanitize_str(name);
            }
            writeln!(
                file,
                "---thread {} (port={}, name={:?}, crashed={})---",
                i, thread.thread_port, thread_name, thread.crashed
            )
            .map_err(|e| format!("write: {e}"))?;

            match &thread.registers {
                Some(regs) => {
                    for (name, val) in regs {
                        writeln!(file, "{name}={val:#018x}").map_err(|e| format!("write: {e}"))?;
                    }
                    writeln!(file, "---backtrace---").map_err(|e| format!("write: {e}"))?;
                    for addr in &thread.backtrace {
                        writeln!(file, "{addr:#018x}").map_err(|e| format!("write: {e}"))?;
                    }
                }
                None => {
                    writeln!(file, "ERROR: register inspection failed")
                        .map_err(|e| format!("write: {e}"))?;
                }
            }
        }
        Ok(())
    })
}

#[cfg(test)]
#[path = "../../tests/unit/pipeline/safety_tests.rs"]
mod tests;

/// Write raw shared memory sections (breadcrumbs + context) to disk.
/// Separate from Stage 1 thread data — both are fail-safe dumps.
///
/// # Errors
/// Returns an error if file write I/O fails.
pub fn write_raw_shm_stage1(
    transaction: &ArtifactTransaction,
    snapshot: &crate::pipeline::RawShmSnapshot,
) -> Result<(), String> {
    transaction
        .write_bytes(
            "breadcrumbs.bin",
            ArtifactKind::BreadcrumbsRaw,
            &snapshot.breadcrumbs,
        )
        .map_err(|e| format!("Failed to write raw breadcrumbs: {e}"))?;
    transaction
        .write_bytes("context.bin", ArtifactKind::ContextRaw, &snapshot.context)
        .map_err(|e| format!("Failed to write raw context: {e}"))?;

    Ok(())
}
