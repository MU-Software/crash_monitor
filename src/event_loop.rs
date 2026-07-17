//! Extracted event loop — testable without real processes or Mach ports.
//!
//! The core monitoring logic: poll an event source, dispatch to the pipeline,
//! and handle exit conditions. ANR detection is integrated directly via the
//! pure `WatchdogState` state machine (no dedicated thread).

use mach2::message::mach_msg_header_t;
use mach2::port::mach_port_t;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use crate::pipeline::{CrashEvent, Pipeline, ReportType, TerminationReason};
use crate::shm::SharedMemory;
use crate::watchdog::{WatchdogState, update_watchdog_state};

// ═══════════════════════════════════════════════════
//  MonitorEvent + EventSource trait
// ═══════════════════════════════════════════════════

/// Events the monitor can receive from various sources.
pub enum MonitorEvent {
    /// Mach exception (crash).
    Crash {
        exception_type: u32,
        code: u64,
        subcode: u64,
        thread_port: mach_port_t,
        /// Copy of the Mach reply header for deferred reply (None in tests).
        reply_header: Option<mach_msg_header_t>,
    },
    /// SIGUSR1 manual snapshot.
    Snapshot,
    /// A terminal child status, normalized from every `waitpid` owner.
    ChildTerminated(TerminationReason),
    /// The monitor can no longer determine the child's state reliably.
    MonitorFailure { message: String },
}

/// Abstract source of monitor events.
///
/// Implementations:
/// - `MacEventSource` (main.rs): real Mach exception port + signal pipe + waitpid
/// - `TestEventSource` (tests): yields predetermined events from a `VecDeque`
pub trait EventSource {
    /// Poll for the next event. Returns `None` if no event is ready yet.
    fn poll(&mut self) -> Option<MonitorEvent>;
}

// ═══════════════════════════════════════════════════
//  ANR watchdog configuration
// ═══════════════════════════════════════════════════

/// Configuration for inline ANR detection within the event loop.
#[allow(clippy::struct_field_names)] // _ms suffix clarifies unit, removing it hurts readability
pub struct AnrConfig {
    pub warmup_ms: u64,
    pub threshold_ms: u64,
    pub check_interval_ms: u64,
    pub cooldown_ms: u64,
}

// ═══════════════════════════════════════════════════
//  Event loop
// ═══════════════════════════════════════════════════

/// Stable process exit codes for monitor-owned outcomes.
///
/// Child signals use the separate conventional `128 + signal` namespace. The
/// original child exit code and signal are preserved in `TerminationReason`
/// rather than being overloaded into this one-byte process status.
pub const EXIT_MONITOR_INTERNAL: i32 = 70;
pub const EXIT_CHILD_FAILURE: i32 = 80;
pub const EXIT_DETECTED_CRASH: i32 = 81;

/// Typed result of monitoring. Integer process status is encoded only at the
/// outermost CLI boundary so monitor failures, detected crashes, and child
/// failures cannot collapse into the same internal value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MonitorOutcome {
    ChildTerminated(TerminationReason),
    DetectedCrash {
        /// Filled by the supervisor after replying to the Mach exception and
        /// reaping the child.
        termination: Option<TerminationReason>,
        /// Final JSON or ZIP produced by the pipeline. The supervisor uses it
        /// to persist the terminal wait status after reaping the child.
        report_path: Option<PathBuf>,
    },
    MonitorFailure(String),
}

impl MonitorOutcome {
    /// Encode the public CLI exit-code contract.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::ChildTerminated(TerminationReason::Exited { exit_code: 0, .. }) => 0,
            Self::ChildTerminated(TerminationReason::Exited { .. }) => EXIT_CHILD_FAILURE,
            Self::ChildTerminated(TerminationReason::Signaled { signal, .. }) => 128 + signal,
            Self::DetectedCrash { .. } => EXIT_DETECTED_CRASH,
            Self::MonitorFailure(_) => EXIT_MONITOR_INTERNAL,
        }
    }

    /// Attach the terminal status observed after a Mach exception report.
    #[must_use]
    pub fn with_crash_termination(self, reason: Option<TerminationReason>) -> Self {
        match self {
            Self::DetectedCrash { report_path, .. } => Self::DetectedCrash {
                termination: reason,
                report_path,
            },
            other => other,
        }
    }

    /// Return the finalized artifact associated with a detected Mach crash.
    #[must_use]
    pub fn report_path(&self) -> Option<&Path> {
        match self {
            Self::DetectedCrash {
                report_path: Some(path),
                ..
            } => Some(path),
            _ => None,
        }
    }
}

/// Convert a terminal child status into a typed report (when abnormal) and a
/// typed monitor outcome. This is shared by startup and steady-state paths.
#[must_use]
pub fn handle_child_termination(
    pipeline: &Pipeline,
    pid: u32,
    process_name: &str,
    reason: TerminationReason,
    oom_detection: bool,
) -> MonitorOutcome {
    let report_type = match reason {
        TerminationReason::Exited { exit_code: 0, .. } => None,
        TerminationReason::Exited { .. } => Some(ReportType::ExitFailure),
        TerminationReason::Signaled { signal, .. } if oom_detection && signal == SIGKILL_NUM => {
            Some(ReportType::Oom)
        }
        TerminationReason::Signaled { .. } => Some(ReportType::SignalFailure),
    };

    if let Some(report_type) = report_type {
        let event = CrashEvent {
            report_type,
            exception_type: None,
            exception_code: None,
            exception_subcode: None,
            crashed_thread: None,
            // The child has already terminated, so the pipeline uses its
            // task-independent termination finalization path.
            bail_on_suspend_failure: false,
            pid,
            process_name: process_name.to_string(),
            hang_duration_ms: None,
            termination: Some(reason),
        };
        let _diagnostics = pipeline.handle_termination_event(&event);
    }

    MonitorOutcome::ChildTerminated(reason)
}

/// The extracted event loop. Returns a typed monitor outcome.
///
/// ANR detection is integrated directly: if `shm` and `anr_config` are provided,
/// the event loop polls the heartbeat counter and fires ANR events inline
/// (no dedicated watchdog thread needed).
/// Signal number for SIGKILL — used to identify probable OOM kills.
const SIGKILL_NUM: i32 = 9;

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub fn event_loop(
    source: &mut dyn EventSource,
    pipeline: &Pipeline,
    task: mach_port_t,
    pid: u32,
    process_name: &str,
    reply_fn: &dyn Fn(&mach_msg_header_t),
    shm: Option<&Arc<SharedMemory>>,
    anr_config: Option<&AnrConfig>,
    oom_detection: bool,
) -> MonitorOutcome {
    // Initialize ANR state if both shm and config are available
    let mut anr_state = match (&shm, &anr_config) {
        (Some(s), Some(cfg)) => Some((
            WatchdogState {
                prev_heartbeat: s.read_heartbeat(),
                hang_accumulated_ms: 0,
                cooldown_remaining_ms: 0,
            },
            cfg.warmup_ms,
        )),
        _ => None,
    };
    let mut last_anr_check = Instant::now();

    loop {
        match source.poll() {
            Some(MonitorEvent::Crash {
                exception_type,
                code,
                subcode,
                thread_port,
                reply_header,
            }) => {
                let event = CrashEvent {
                    report_type: ReportType::Crash,
                    exception_type: Some(exception_type),
                    exception_code: Some(code),
                    exception_subcode: Some(subcode),
                    crashed_thread: Some(thread_port),
                    bail_on_suspend_failure: false,
                    pid,
                    process_name: process_name.to_string(),
                    hang_duration_ms: None,
                    termination: None,
                };
                let diagnostics = pipeline.handle_event(&event, task);

                if let Some(ref header) = reply_header {
                    reply_fn(header);
                }
                return MonitorOutcome::DetectedCrash {
                    termination: None,
                    report_path: diagnostics.report_path,
                };
            }

            Some(MonitorEvent::Snapshot) => {
                let event = CrashEvent {
                    report_type: ReportType::Snapshot,
                    exception_type: None,
                    exception_code: None,
                    exception_subcode: None,
                    crashed_thread: None,
                    bail_on_suspend_failure: true,
                    pid,
                    process_name: process_name.to_string(),
                    hang_duration_ms: None,
                    termination: None,
                };
                let _diagnostics = pipeline.handle_event(&event, task);
            }

            Some(MonitorEvent::ChildTerminated(reason)) => {
                return handle_child_termination(
                    pipeline,
                    pid,
                    process_name,
                    reason,
                    oom_detection,
                );
            }

            Some(MonitorEvent::MonitorFailure { message }) => {
                eprintln!("[monitor] {message}");
                return MonitorOutcome::MonitorFailure(message);
            }

            None => {
                // ── Inline ANR check ──
                if let (Some((state, warmup_remaining)), Some(s), Some(cfg)) =
                    (&mut anr_state, &shm, &anr_config)
                {
                    #[allow(clippy::cast_possible_truncation)]
                    let elapsed = last_anr_check.elapsed().as_millis() as u64;
                    if elapsed >= cfg.check_interval_ms {
                        last_anr_check = Instant::now();

                        // Warmup: skip checks during startup
                        if *warmup_remaining > 0 {
                            *warmup_remaining = warmup_remaining.saturating_sub(elapsed);
                        } else {
                            let heartbeat = s.read_heartbeat();
                            if let Some(hang_duration_ms) = update_watchdog_state(
                                state,
                                heartbeat,
                                elapsed,
                                cfg.threshold_ms,
                                cfg.cooldown_ms,
                            ) {
                                eprintln!(
                                    "[monitor] ANR detected: heartbeat stale for {hang_duration_ms}ms"
                                );
                                let event = CrashEvent {
                                    report_type: ReportType::Anr,
                                    exception_type: None,
                                    exception_code: None,
                                    exception_subcode: None,
                                    crashed_thread: None,
                                    bail_on_suspend_failure: true,
                                    pid,
                                    process_name: process_name.to_string(),
                                    hang_duration_ms: Some(hang_duration_ms),
                                    termination: None,
                                };
                                let _diagnostics = pipeline.handle_event(&event, task);
                            }
                        }
                    }
                }

                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
}
