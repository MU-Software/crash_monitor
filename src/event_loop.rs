//! Extracted event loop — testable without real processes or Mach ports.
//!
//! The core monitoring logic: poll an event source, dispatch to the pipeline,
//! and handle exit conditions. ANR detection is integrated directly via the
//! pure `WatchdogState` state machine (no dedicated thread).

use mach2::message::mach_msg_header_t;
use mach2::port::mach_port_t;
use std::sync::Arc;
use std::time::Instant;

use crate::pipeline::{CrashEvent, Pipeline, ReportType};
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
    /// Child exited normally.
    ChildExited { status: i32 },
    /// Child killed by signal.
    ChildSignaled { signal: i32 },
    /// Child no longer exists (ECHILD).
    ChildGone,
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

/// The extracted event loop. Returns the process exit code.
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
) -> i32 {
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
                };
                let _diagnostics = pipeline.handle_event(&event, task);

                if let Some(ref header) = reply_header {
                    reply_fn(header);
                }
                return 1;
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
                };
                let _diagnostics = pipeline.handle_event(&event, task);
            }

            Some(MonitorEvent::ChildExited { status }) => {
                return status;
            }

            Some(MonitorEvent::ChildSignaled { signal }) => {
                // SIGKILL is the macOS jetsam (memory-pressure) signal — treat as
                // probable OOM and dispatch through the pipeline. We can't reliably
                // distinguish a true jetsam kill from a manual `kill -9`, hence the
                // "probable" framing via `header.trigger = "sigkill"`. Other signals
                // are surfaced as crashes through the Mach exception path already.
                if oom_detection && signal == SIGKILL_NUM {
                    let event = CrashEvent {
                        report_type: ReportType::Oom,
                        exception_type: None,
                        exception_code: None,
                        exception_subcode: None,
                        crashed_thread: None,
                        // Child is already dead — suspend_task will fail. Continue
                        // best-effort so shm-based collectors (breadcrumb, context,
                        // screenshot, attachment) still produce data.
                        bail_on_suspend_failure: false,
                        pid,
                        process_name: process_name.to_string(),
                        hang_duration_ms: None,
                    };
                    let _diagnostics = pipeline.handle_event(&event, task);
                }
                return 128 + signal;
            }

            Some(MonitorEvent::ChildGone) => {
                return 0;
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
