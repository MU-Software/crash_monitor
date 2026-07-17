//! Safety utilities: panic-catching plugin wrapper, RAII port guard,
//! and Stage 1 raw data writer (fail-safe dump).

use crate::collectors::thread::RawThreadData;
use crate::platform::PlatformOps;

use std::io::Write;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::report::report_filename;

// ═══════════════════════════════════════════════════
//  run_plugin_safe
// ═══════════════════════════════════════════════════

/// RAII guard that cancels `alarm()` on drop — ensures no stray SIGALRM
/// even if the plugin panics or returns early.
struct AlarmGuard;

impl Drop for AlarmGuard {
    fn drop(&mut self) {
        // SAFETY: alarm(0) cancels any pending alarm. Always safe to call.
        unsafe {
            nix::libc::alarm(0);
        }
    }
}

/// No-op `SIGALRM` handler. `SA_RESTART` is NOT set, so blocked syscalls
/// receive `EINTR`, which propagates as errors through nix/mach2 wrappers.
extern "C" fn sigalrm_noop(_sig: nix::libc::c_int) {}

/// Execute a plugin closure with panic catching and no process-global timeout.
///
/// This is the safe wrapper for worker stages that have their own deadline or
/// isolation policy. Unlike [`run_plugin_safe`], it never installs a signal
/// handler or arms `alarm()`, so concurrent work cannot interfere with the
/// process-wide SIGALRM state.
pub fn run_plugin_catching_panics<T>(
    name: &str,
    f: impl FnOnce() -> Result<T, String>,
) -> Option<T> {
    finish_plugin_result(name, std::panic::catch_unwind(AssertUnwindSafe(f)))
}

/// Execute a plugin closure with panic catching and optional timeout.
///
/// `timeout_secs`: 0 = no timeout. Otherwise, `alarm(timeout_secs)` is set
/// before the closure runs. SIGALRM interrupts blocking syscalls with EINTR.
///
/// # Safety contract
/// Uses `AssertUnwindSafe` to catch panics across `&mut` references.
/// In safe Rust, panic during mutation leaves data in a field-level atomic state:
/// each `data.field = expr` either completes fully or doesn't execute at all.
/// Partial data is acceptable for crash reporting (more data > no data).
pub fn run_plugin_safe<T>(
    name: &str,
    timeout_secs: u32,
    f: impl FnOnce() -> Result<T, String>,
) -> Option<T> {
    // Install SIGALRM handler + set alarm if timeout requested
    let _alarm_guard = if timeout_secs > 0 {
        install_sigalrm_handler();
        // SAFETY: alarm() sets a process-wide timer. AlarmGuard cancels on drop.
        unsafe {
            nix::libc::alarm(timeout_secs);
        }
        Some(AlarmGuard)
    } else {
        None
    };

    // _alarm_guard remains alive while the closure runs and is dropped before
    // this function returns, cancelling any pending SIGALRM.
    run_plugin_catching_panics(name, f)
}

fn finish_plugin_result<T>(
    name: &str,
    result: std::thread::Result<Result<T, String>>,
) -> Option<T> {
    match result {
        Ok(Ok(val)) => Some(val),
        Ok(Err(e)) => {
            eprintln!("[monitor] plugin {name}: {e}");
            None
        }
        Err(_) => {
            eprintln!("[monitor] plugin {name} panicked");
            None
        }
    }
}

/// Install a no-op SIGALRM handler (idempotent, uses `sigaction` not `signal`).
fn install_sigalrm_handler() {
    use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};
    let action = SigAction::new(
        SigHandler::Handler(sigalrm_noop),
        SaFlags::empty(), // NO SA_RESTART → syscalls get EINTR
        SigSet::empty(),
    );
    // SAFETY: sigaction installs a signal handler. We use a no-op handler
    // that is async-signal-safe (empty function body).
    let _ = unsafe { sigaction(Signal::SIGALRM, &action) };
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
    dir: &Path,
    report_type: crate::pipeline::ReportType,
    pid: u32,
    threads: &[RawThreadData],
) -> Result<PathBuf, String> {
    let basename = report_filename(report_type, pid);
    let raw_path = dir.join(format!("{basename}_raw.bin"));

    let mut file =
        std::fs::File::create(&raw_path).map_err(|e| format!("Failed to create raw file: {e}"))?;

    for (i, thread) in threads.iter().enumerate() {
        writeln!(
            file,
            "---thread {} (port={}, name={:?}, crashed={})---",
            i, thread.thread_port, thread.name, thread.crashed
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

    Ok(raw_path)
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
    dir: &Path,
    report_type: crate::pipeline::ReportType,
    pid: u32,
    snapshot: &crate::pipeline::RawShmSnapshot,
) -> Result<(), String> {
    let basename = report_filename(report_type, pid);

    let crumb_path = dir.join(format!("{basename}_raw_breadcrumbs.bin"));
    std::fs::write(&crumb_path, &snapshot.breadcrumbs)
        .map_err(|e| format!("Failed to write raw breadcrumbs: {e}"))?;

    let ctx_path = dir.join(format!("{basename}_raw_context.bin"));
    std::fs::write(&ctx_path, &snapshot.context)
        .map_err(|e| format!("Failed to write raw context: {e}"))?;

    Ok(())
}
