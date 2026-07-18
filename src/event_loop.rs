//! Extracted event loop — testable without real processes or Mach ports.
//!
//! The core monitoring logic: poll an event source, dispatch to the pipeline,
//! and handle exit conditions. ANR detection is integrated directly via the
//! pure `WatchdogState` state machine (no dedicated thread).

use mach2::port::mach_port_t;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::pipeline::worker::{
    BACKGROUND_DRAIN_DEADLINE, BackgroundFinalizeWorker, CAPTURE_DEADLINE, CRASH_FINALIZE_WAIT,
    CaptureWorker, CrashFinalization, finalize_terminated_child,
};
use crate::pipeline::{
    CaptureOutcome, CrashEvent, Pipeline, ReportId, ReportType, TerminationReason,
};
use crate::platform::macos::ReceivedMachMessage;
use crate::shm::SharedMemory;
use crate::watchdog::{
    MonitorWorkRebase, WatchdogState, exclude_monitor_work_from_anr_clock, update_watchdog_state,
};

// ═══════════════════════════════════════════════════
//  MonitorEvent + EventSource trait
// ═══════════════════════════════════════════════════

/// Events the monitor can receive from various sources.
pub enum MonitorEvent {
    /// Mach exception (crash).
    Crash {
        /// Monotonic time when the listener received the Mach request.
        received_at: Instant,
        exception_type: u32,
        code: u64,
        subcode: u64,
        /// Exact MIG exception code array, including its original count.
        raw_codes: Vec<u64>,
        /// Owns the original receive buffer and every Mach right it carries.
        request: ReceivedMachMessage,
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
        /// Final JSON or ZIP produced after the terminal wait status is handed
        /// to the fatal finalization worker.
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

    /// Attach both pieces produced after the exception reply: the terminal
    /// wait status and the final artifact created by the fatal worker.
    #[must_use]
    pub fn with_crash_result(
        self,
        reason: Option<TerminationReason>,
        report_path: Option<PathBuf>,
    ) -> Self {
        match self {
            Self::DetectedCrash { .. } => Self::DetectedCrash {
                termination: reason,
                report_path,
            },
            other => other,
        }
    }

    /// Return the finalized artifact associated with a detected Mach crash.
    #[must_use]
    #[allow(dead_code)] // Public through the library; private in the binary's shared module copy.
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

/// Event-loop handoff. Fatal finalization is deliberately separate from the
/// monitor outcome because it must not start expensive work before Mach reply.
pub struct EventLoopResult {
    pub outcome: MonitorOutcome,
    pub crash_finalization: Option<CrashFinalization>,
    /// The supervisor must destroy the exception port and boundedly reap the
    /// child even when the deferred reply itself failed.
    pub crash_cleanup_required: bool,
}

impl EventLoopResult {
    #[must_use]
    #[allow(dead_code)] // Public through the library; private in the binary's shared module copy.
    pub fn exit_code(&self) -> i32 {
        self.outcome.exit_code()
    }

    #[must_use]
    #[allow(dead_code)] // Public through the library; private in the binary's shared module copy.
    pub fn report_path(&self) -> Option<&Path> {
        self.outcome.report_path()
    }
}

/// Convert a terminal child status into a typed report (when abnormal) and a
/// typed monitor outcome. This is shared by startup and steady-state paths.
#[must_use]
pub fn termination_report_type(
    pipeline: &Pipeline,
    reason: TerminationReason,
) -> Option<ReportType> {
    match reason {
        TerminationReason::Exited { exit_code: 0, .. } => None,
        TerminationReason::Exited { .. } if pipeline.report_enabled(ReportType::ExitFailure) => {
            Some(ReportType::ExitFailure)
        }
        TerminationReason::Signaled { signal, .. }
            if signal == SIGKILL_NUM && pipeline.report_enabled(ReportType::Oom) =>
        {
            Some(ReportType::Oom)
        }
        TerminationReason::Signaled { .. }
            if pipeline.report_enabled(ReportType::SignalFailure) =>
        {
            Some(ReportType::SignalFailure)
        }
        TerminationReason::Exited { .. } | TerminationReason::Signaled { .. } => None,
    }
}

/// Finalize the report selected by the primary child-termination trigger.
/// Mach exceptions take their separate crash path and use termination only as
/// metadata, so a single incident cannot produce both crash and signal/exit
/// reports (or bypass a disabled crash policy through a fallback report).
#[must_use]
fn finalize_child_termination_report(
    pipeline: &Arc<Pipeline>,
    pid: u32,
    process_name: &str,
    reason: TerminationReason,
) -> Option<crate::pipeline::Diagnostics> {
    let report_type = termination_report_type(pipeline, reason)?;
    let event = CrashEvent {
        report_id: ReportId::default(),
        report_type,
        exception_type: None,
        exception_code: None,
        exception_subcode: None,
        exception_codes: Vec::new(),
        crashed_thread: None,
        // The child has already terminated, so the pipeline uses its
        // task-independent termination finalization path.
        bail_on_suspend_failure: false,
        pid,
        process_name: process_name.to_string(),
        hang_duration_ms: None,
        termination: Some(reason),
    };
    finalize_terminated_child(pipeline.clone(), event, CRASH_FINALIZE_WAIT)
}

/// Convert a terminal child status into a typed report (when abnormal) and a
/// typed monitor outcome. This is shared by startup and steady-state paths.
#[must_use]
pub fn handle_child_termination(
    pipeline: &Arc<Pipeline>,
    pid: u32,
    process_name: &str,
    reason: TerminationReason,
) -> MonitorOutcome {
    let _diagnostics = finalize_child_termination_report(pipeline, pid, process_name, reason);

    MonitorOutcome::ChildTerminated(reason)
}

fn task_control_monitor_failure(pipeline: &Pipeline) -> Option<String> {
    pipeline
        .platform
        .supervisor_health()
        .task_control_failures
        .into_iter()
        .rev()
        .find(crate::platform::TaskControlFailure::prevents_continued_monitoring)
        .map(|failure| format!("task-control containment activated: {failure}"))
}

/// Persist the current snapshot/ANR after its task suspension has already been
/// released or contained. A dedicated ticket avoids losing the `TaskResume`
/// diagnostic when the event loop must stop, while keeping finalization off the
/// task-facing capture path and bounding the wait.
fn finalize_contained_capture(pipeline: &Arc<Pipeline>, capture: CaptureOutcome) {
    let CaptureOutcome::Captured(captured) = capture else {
        return;
    };
    let diagnostics = CrashFinalization::start(pipeline.clone(), captured).complete(
        pipeline.clone(),
        None,
        BACKGROUND_DRAIN_DEADLINE,
    );
    if diagnostics
        .as_ref()
        .and_then(|diagnostics| diagnostics.report_path.as_ref())
        .is_none()
    {
        eprintln!("[monitor] contained capture finalization did not produce an artifact");
    }
}

/// Finish ANR accounting for monitor-owned work that may have suspended the
/// child. `CaptureWorker::capture` returns only after releasing its suspension
/// guard, so the acquire sample below is the first post-resume observation.
/// When the heartbeat is unchanged, pre-capture stale time remains valid and
/// only the measured monitor interval is removed from the elapsed clock.
fn finish_anr_monitor_work(
    anr_state: Option<&mut WatchdogState>,
    shm: Option<&SharedMemory>,
    last_anr_check: &mut Instant,
    monitor_work_started: Instant,
) {
    let (Some(state), Some(shm)) = (anr_state, shm) else {
        return;
    };
    let application_elapsed_before_monitor_ms = u64::try_from(
        monitor_work_started
            .saturating_duration_since(*last_anr_check)
            .as_millis(),
    )
    .unwrap_or(u64::MAX);
    let rebase = state.rebase_after_monitor_work(
        shm.read_live_anr_heartbeat(),
        application_elapsed_before_monitor_ms,
    );
    let monitor_work_finished = Instant::now();
    *last_anr_check = match rebase {
        MonitorWorkRebase::PreserveElapsed => exclude_monitor_work_from_anr_clock(
            *last_anr_check,
            monitor_work_started,
            monitor_work_finished,
        ),
        MonitorWorkRebase::ResetElapsed => monitor_work_finished,
    };
}

/// The extracted event loop. Returns a typed monitor outcome.
///
/// ANR detection is integrated directly: if `shm` and `anr_config` are provided,
/// the event loop waits for the producer-ready handshake, polls the heartbeat
/// counter, and fires ANR events inline (no dedicated watchdog thread needed).
/// Signal number for SIGKILL — used to identify probable OOM kills.
const SIGKILL_NUM: i32 = 9;

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub fn event_loop(
    source: &mut dyn EventSource,
    pipeline: &Arc<Pipeline>,
    task: mach_port_t,
    pid: u32,
    process_name: &str,
    reply_fn: &dyn Fn(&mut ReceivedMachMessage) -> Result<(), String>,
    shm: Option<&Arc<SharedMemory>>,
    anr_config: Option<&AnrConfig>,
) -> EventLoopResult {
    // Worker construction is monitor-owned setup time, so finish it before
    // establishing the first ANR heartbeat/time baseline.
    let mut capture_worker = CaptureWorker::start(pipeline.clone(), task);
    let background_worker = BackgroundFinalizeWorker::start(pipeline.clone());

    // Initialize ANR state only when both SHM and config are available. A
    // zeroed, unclaimed mapping yields `None` and remains unarmed until the
    // producer publishes its readiness handshake.
    let mut anr_state = match (pipeline.report_enabled(ReportType::Anr), &shm, &anr_config) {
        (true, Some(s), Some(cfg)) => Some(WatchdogState::new(
            cfg.warmup_ms,
            s.read_live_anr_heartbeat(),
        )),
        _ => None,
    };
    let mut last_anr_check = Instant::now();

    loop {
        match source.poll() {
            Some(MonitorEvent::Crash {
                received_at,
                exception_type,
                code,
                subcode,
                raw_codes,
                mut request,
            }) => {
                let thread_port = request.thread_port();
                let captured = if pipeline.report_enabled(ReportType::Crash) {
                    let event = CrashEvent {
                        report_id: ReportId::default(),
                        report_type: ReportType::Crash,
                        exception_type: Some(exception_type),
                        exception_code: Some(code),
                        exception_subcode: Some(subcode),
                        exception_codes: raw_codes,
                        crashed_thread: Some(thread_port),
                        bail_on_suspend_failure: false,
                        pid,
                        process_name: process_name.to_string(),
                        hang_duration_ms: None,
                        termination: None,
                    };
                    let deadline = received_at
                        .checked_add(CAPTURE_DEADLINE)
                        .unwrap_or(received_at);
                    match capture_worker.capture(event, task, deadline) {
                        crate::pipeline::CaptureOutcome::Captured(captured) => Some(captured),
                        crate::pipeline::CaptureOutcome::Skipped(_) => None,
                    }
                } else {
                    None
                };

                let reply_result = reply_fn(&mut request);
                // On success the callback has disarmed the consumed reply
                // right; on failure it remains armed. Either way Drop now
                // releases both descriptor rights exactly once.
                drop(request);
                let task_control_failure = task_control_monitor_failure(pipeline);
                // No drain or join is allowed between reply and returning to
                // the supervisor, which must destroy the exception port and
                // begin reaping immediately.
                capture_worker.detach();
                background_worker.detach();
                let crash_finalization =
                    captured.map(|captured| CrashFinalization::start(pipeline.clone(), captured));
                let outcome = match (task_control_failure, reply_result) {
                    (None, Ok(())) => MonitorOutcome::DetectedCrash {
                        termination: None,
                        report_path: None,
                    },
                    (None, Err(error)) => MonitorOutcome::MonitorFailure(format!(
                        "failed to send deferred Mach exception reply: {error}"
                    )),
                    (Some(message), Ok(())) => MonitorOutcome::MonitorFailure(message),
                    (Some(message), Err(error)) => MonitorOutcome::MonitorFailure(format!(
                        "{message}; failed to send deferred Mach exception reply: {error}"
                    )),
                };
                return EventLoopResult {
                    outcome,
                    crash_finalization,
                    crash_cleanup_required: true,
                };
            }

            Some(MonitorEvent::Snapshot) => {
                if !pipeline.report_enabled(ReportType::Snapshot) {
                    continue;
                }
                let monitor_work_started = Instant::now();
                let event = CrashEvent {
                    report_id: ReportId::default(),
                    report_type: ReportType::Snapshot,
                    exception_type: None,
                    exception_code: None,
                    exception_subcode: None,
                    exception_codes: Vec::new(),
                    crashed_thread: None,
                    bail_on_suspend_failure: true,
                    pid,
                    process_name: process_name.to_string(),
                    hang_duration_ms: None,
                    termination: None,
                };
                let capture =
                    capture_worker.capture(event, task, Instant::now() + CAPTURE_DEADLINE);
                if let Some(message) = task_control_monitor_failure(pipeline) {
                    capture_worker.detach();
                    finalize_contained_capture(pipeline, capture);
                    background_worker.detach();
                    return EventLoopResult {
                        outcome: MonitorOutcome::MonitorFailure(message),
                        crash_finalization: None,
                        crash_cleanup_required: false,
                    };
                }
                if let crate::pipeline::CaptureOutcome::Captured(captured) = capture {
                    let _ = background_worker.try_submit(*captured);
                }
                finish_anr_monitor_work(
                    anr_state.as_mut(),
                    shm.map(Arc::as_ref),
                    &mut last_anr_check,
                    monitor_work_started,
                );
            }

            Some(MonitorEvent::ChildTerminated(reason)) => {
                let outcome = handle_child_termination(pipeline, pid, process_name, reason);
                capture_worker.shutdown(Duration::from_millis(100));
                background_worker.shutdown(BACKGROUND_DRAIN_DEADLINE);
                return EventLoopResult {
                    outcome,
                    crash_finalization: None,
                    crash_cleanup_required: false,
                };
            }

            Some(MonitorEvent::MonitorFailure { message }) => {
                eprintln!("[monitor] {message}");
                capture_worker.shutdown(Duration::from_millis(100));
                background_worker.shutdown(BACKGROUND_DRAIN_DEADLINE);
                return EventLoopResult {
                    outcome: MonitorOutcome::MonitorFailure(message),
                    crash_finalization: None,
                    crash_cleanup_required: false,
                };
            }

            None => {
                // ── Inline ANR check ──
                if let (Some(state), Some(s), Some(cfg)) = (&mut anr_state, &shm, &anr_config) {
                    #[allow(clippy::cast_possible_truncation)]
                    let elapsed = last_anr_check.elapsed().as_millis() as u64;
                    if elapsed >= cfg.check_interval_ms {
                        let heartbeat = s.read_live_anr_heartbeat();
                        let detected = update_watchdog_state(
                            state,
                            heartbeat,
                            elapsed,
                            cfg.threshold_ms,
                            cfg.cooldown_ms,
                        );
                        // Sampling and state transition are monitor work too;
                        // begin the next interval only after both complete.
                        last_anr_check = Instant::now();

                        if let Some(hang_duration_ms) = detected {
                            let monitor_work_started = last_anr_check;
                            eprintln!(
                                "[monitor] ANR detected: heartbeat stale for {hang_duration_ms}ms"
                            );
                            let event = CrashEvent {
                                report_id: ReportId::default(),
                                report_type: ReportType::Anr,
                                exception_type: None,
                                exception_code: None,
                                exception_subcode: None,
                                exception_codes: Vec::new(),
                                crashed_thread: None,
                                bail_on_suspend_failure: true,
                                pid,
                                process_name: process_name.to_string(),
                                hang_duration_ms: Some(hang_duration_ms),
                                termination: None,
                            };
                            let capture = capture_worker.capture(
                                event,
                                task,
                                Instant::now() + CAPTURE_DEADLINE,
                            );
                            if let Some(message) = task_control_monitor_failure(pipeline) {
                                capture_worker.detach();
                                finalize_contained_capture(pipeline, capture);
                                background_worker.detach();
                                return EventLoopResult {
                                    outcome: MonitorOutcome::MonitorFailure(message),
                                    crash_finalization: None,
                                    crash_cleanup_required: false,
                                };
                            }
                            if let crate::pipeline::CaptureOutcome::Captured(captured) = capture {
                                let _ = background_worker.try_submit(*captured);
                            }
                            finish_anr_monitor_work(
                                Some(state),
                                Some(s.as_ref()),
                                &mut last_anr_check,
                                monitor_work_started,
                            );
                        }
                    }
                }

                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    }
}
