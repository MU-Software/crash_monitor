//! Bounded worker boundary between live-task capture and report finalization.

use mach2::port::mach_port_t;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::{
    CaptureOutcome, CapturePayload, CapturedEvent, Diagnostics, Pipeline, PluginStatus,
    TerminationReason, suspend_failure_policy,
};
use crate::platform::{
    RetainedTaskPort, TaskControlFailureSink, TaskSuspendGuard, contain_task_without_resume,
};

/// One absolute capture budget, measured from Mach request receipt for crashes.
pub const CAPTURE_DEADLINE: Duration = Duration::from_secs(5);
/// Maximum best-effort drain when the child exits normally.
pub const BACKGROUND_DRAIN_DEADLINE: Duration = Duration::from_secs(2);
/// Covers the five-minute feedback UI plus a small completion margin.
pub const CRASH_FINALIZE_WAIT: Duration = Duration::from_secs(310);

const CAPTURE_QUEUE_CAPACITY: usize = 1;
const BACKGROUND_QUEUE_CAPACITY: usize = 2;

#[cfg(test)]
thread_local! {
    /// Deterministic fault injection for the otherwise impractical
    /// `thread::Builder::spawn` resource-exhaustion path.
    static TEST_FATAL_SPAWN_FAILURES: std::cell::Cell<usize> = const {
        std::cell::Cell::new(0)
    };
}

struct CaptureJob {
    event: super::CrashEvent,
    report_context: Arc<super::ReportContext>,
    cancelled: Arc<AtomicBool>,
    shm_snapshot: Option<Arc<crate::shm::OwnedShmSnapshot>>,
    result_tx: SyncSender<CapturePayload>,
}

/// Production capture uses a killable exec helper. The in-process worker is a
/// compatibility boundary for deterministic mock platforms; once it times out
/// it is retired rather than accepting another task while an old collector may
/// still be running.
pub(crate) struct CaptureWorker {
    pipeline: Arc<Pipeline>,
    isolated: bool,
    sender: Option<SyncSender<CaptureJob>>,
    done_rx: Receiver<()>,
    handle: Option<JoinHandle<()>>,
    unavailable_reason: Option<String>,
}

impl CaptureWorker {
    pub(crate) fn start(pipeline: Arc<Pipeline>, task: mach_port_t) -> Self {
        let (sender, receiver) = sync_channel::<CaptureJob>(CAPTURE_QUEUE_CAPACITY);
        let (done_tx, done_rx) = mpsc::channel();
        if !pipeline.enabled {
            return Self {
                pipeline,
                isolated: false,
                sender: None,
                done_rx,
                handle: None,
                unavailable_reason: None,
            };
        }
        if pipeline.platform.supports_capture_isolation() {
            // Production task-facing collectors run in a killable exec helper.
            // Dropping both channel ends makes shutdown a no-op for this mode.
            drop(sender);
            drop(receiver);
            drop(done_tx);
            return Self {
                pipeline,
                isolated: true,
                sender: None,
                done_rx,
                handle: None,
                unavailable_reason: None,
            };
        }
        let retained_task = match RetainedTaskPort::retain(pipeline.platform.clone(), task) {
            Ok(retained_task) => retained_task,
            Err(error) => {
                return Self {
                    pipeline,
                    isolated: false,
                    sender: None,
                    done_rx,
                    handle: None,
                    unavailable_reason: Some(format!(
                        "failed to retain task port for capture worker: {error}"
                    )),
                };
            }
        };
        let worker_pipeline = pipeline.clone();
        let spawn = thread::Builder::new()
            .name("crash-capture".into())
            .spawn(move || {
                // The worker owns this send-right user reference until the
                // thread actually exits, even when its JoinHandle is detached.
                let task = retained_task.raw();
                while let Ok(CaptureJob {
                    event,
                    report_context,
                    cancelled,
                    shm_snapshot,
                    result_tx,
                }) = receiver.recv()
                {
                    let payload = worker_pipeline.collect_snapshot_for_worker(
                        &event,
                        task,
                        &cancelled,
                        shm_snapshot.as_ref(),
                        &report_context,
                    );
                    let _ = result_tx.send(payload);
                }
                let _ = done_tx.send(());
            });

        match spawn {
            Ok(handle) => Self {
                pipeline,
                isolated: false,
                sender: Some(sender),
                done_rx,
                handle: Some(handle),
                unavailable_reason: None,
            },
            Err(error) => Self {
                pipeline,
                isolated: false,
                sender: None,
                done_rx,
                handle: None,
                unavailable_reason: Some(format!("failed to start capture worker: {error}")),
            },
        }
    }

    /// Capture an event before an absolute deadline and always resume a task
    /// that this call successfully suspended.
    #[allow(clippy::too_many_lines)] // keep suspend/snapshot/enqueue/resume ordering linear
    pub(crate) fn capture(
        &mut self,
        event: super::CrashEvent,
        task: mach_port_t,
        deadline: Instant,
    ) -> CaptureOutcome {
        // This guard must precede deadline handling and task suspension. The
        // worker is a public pipeline bypass used by the event loop, so relying
        // on `Pipeline::handle_event` would leave the kill switch incomplete.
        if !self.pipeline.report_enabled(event.report_type) {
            return CaptureOutcome::Skipped(Diagnostics::new());
        }
        let report_context = match self.pipeline.create_report_context(&event) {
            Ok(report_context) => report_context,
            Err(error) => {
                let mut diagnostics = Diagnostics::new();
                diagnostics.record_immediate("ReportContext", PluginStatus::Error(error));
                return CaptureOutcome::Skipped(diagnostics);
            }
        };
        if Instant::now() >= deadline {
            return defer_owned_if_isolated(
                timed_out_capture(
                    event,
                    report_context,
                    "absolute capture deadline already elapsed",
                ),
                self.isolated,
                None,
            );
        }

        if let Some(reason) = &self.unavailable_reason {
            return failed_capture(event, report_context, reason);
        }

        // Allocate and validate every helper-side resource before suspending
        // the target. The suspended interval then contains only SHM copying,
        // helper execution, and bounded helper cleanup.
        let isolated_job = if self.isolated {
            let request = match super::capture_isolation::TaskCaptureRequest::from_pipeline(
                &self.pipeline,
                &event,
            ) {
                Ok(request) => request,
                Err(error) => {
                    return defer_owned_if_isolated(
                        failed_capture(event, report_context, &error),
                        true,
                        None,
                    );
                }
            };
            let result_file = if request.is_some() {
                match tempfile::tempfile() {
                    Ok(file) => Some(file),
                    Err(error) => {
                        return defer_owned_if_isolated(
                            failed_capture(
                                event,
                                report_context,
                                &format!("cannot create capture-helper result file: {error}"),
                            ),
                            true,
                            None,
                        );
                    }
                }
            } else {
                None
            };
            Some((request, result_file))
        } else {
            None
        };

        let failure_sink = TaskControlFailureSink::new();
        let mut suspend_error = None;
        let mut suspend_guard = match TaskSuspendGuard::acquire(
            self.pipeline.platform.clone(),
            task,
            failure_sink.clone(),
        ) {
            Ok(guard) => Some(guard),
            Err(error) => match suspend_failure_policy(&event) {
                super::SuspendFailurePolicy::SkipCapture => {
                    eprintln!("[monitor] suspend_task failed; capture skipped: {error}");
                    let mut diagnostics = Diagnostics::new();
                    diagnostics.record_immediate("CaptureSuspend", PluginStatus::Error(error));
                    return CaptureOutcome::Skipped(diagnostics);
                }
                super::SuspendFailurePolicy::BestEffort => {
                    eprintln!(
                        "[monitor] suspend_task failed (proceeding with best-effort): {error}"
                    );
                    suspend_error = Some(error);
                    None
                }
            },
        };

        #[cfg(feature = "test-support")]
        if suspend_guard.is_some() {
            crate::test_hooks::pause_at("capture_suspended");
        }

        if Instant::now() >= deadline {
            finish_suspend(&mut suspend_guard);
            let outcome = defer_owned_if_isolated(
                timed_out_capture(
                    event,
                    report_context,
                    "absolute capture deadline elapsed during suspend",
                ),
                self.isolated,
                None,
            );
            return with_task_control_diagnostics(outcome, suspend_error, None, &failure_sink);
        }

        // The guard-owning event-loop thread snapshots every authorized SHM
        // payload section before either capture backend can run. Production
        // task collectors then execute in the killable helper; mock platforms
        // retain the legacy in-process worker for deterministic unit tests.
        let mut snapshot_error = None;
        let shm_snapshot = if suspend_guard.is_some() {
            match self.pipeline.snapshot_shm_while_suspended(Some(deadline)) {
                Ok(snapshot) => {
                    snapshot_error = Pipeline::snapshot_consistency_error(snapshot.as_deref());
                    snapshot
                }
                Err(error) => {
                    snapshot_error = Some(error);
                    None
                }
            }
        } else {
            None
        };

        if Instant::now() >= deadline {
            finish_suspend(&mut suspend_guard);
            let outcome = defer_owned_if_isolated(
                timed_out_capture_with_snapshot(
                    event,
                    report_context,
                    "absolute capture deadline elapsed during shm snapshot",
                    shm_snapshot.as_deref(),
                    self.pipeline.collection_policy.persist_raw_shm,
                ),
                self.isolated,
                shm_snapshot,
            );
            return with_task_control_diagnostics(
                outcome,
                suspend_error,
                snapshot_error,
                &failure_sink,
            );
        }

        if let Some((request, mut result_file)) = isolated_job {
            let isolated_outcome = match (request.as_ref(), result_file.as_mut()) {
                (Some(request), Some(result_file)) => {
                    super::capture_isolation::run_isolated_capture(
                        request,
                        result_file,
                        task,
                        deadline,
                    )
                }
                (None, None) => super::capture_isolation::IsolatedCaptureOutcome::Completed(
                    super::capture_isolation::TaskCaptureData::default(),
                    Vec::new(),
                ),
                _ => unreachable!("capture-helper request and result file must be paired"),
            };

            let fallback_snapshot = shm_snapshot.clone();
            let outcome = match isolated_outcome {
                super::capture_isolation::IsolatedCaptureOutcome::Completed(data, plugins) => {
                    // A successful waitpid proves the helper and all of its Mach
                    // rights are gone before the task is resumed.
                    finish_suspend(&mut suspend_guard);
                    let mut diagnostics = Diagnostics::new();
                    diagnostics.plugins = plugins;
                    let mut collected = super::CollectedData::default();
                    collected.raw.threads = data.threads;
                    collected.raw.images = data.images;
                    collected.raw.memory_map = data.memory_map;
                    collected.raw.heap = data.heap;
                    let payload = CapturePayload {
                        data: collected,
                        raw_shm: raw_shm_from_snapshot(
                            fallback_snapshot.as_deref(),
                            self.pipeline.collection_policy.persist_raw_shm,
                        ),
                        diagnostics,
                    };
                    CaptureOutcome::Captured(Box::new(CapturedEvent::with_report_context(
                        event,
                        report_context,
                        payload,
                    )))
                }
                super::capture_isolation::IsolatedCaptureOutcome::TimedOut => {
                    finish_suspend(&mut suspend_guard);
                    timed_out_capture_with_snapshot(
                        event,
                        report_context,
                        "capture helper exceeded absolute deadline",
                        fallback_snapshot.as_deref(),
                        self.pipeline.collection_policy.persist_raw_shm,
                    )
                }
                super::capture_isolation::IsolatedCaptureOutcome::Failed(error) => {
                    finish_suspend(&mut suspend_guard);
                    failed_capture_with_snapshot(
                        event,
                        report_context,
                        &error,
                        fallback_snapshot.as_deref(),
                        self.pipeline.collection_policy.persist_raw_shm,
                    )
                }
                super::capture_isolation::IsolatedCaptureOutcome::CleanupUnproven(error) => {
                    let reason = format!(
                        "capture-helper cleanup could not be proven; refusing to resume: {error}"
                    );
                    if let Some(guard) = suspend_guard.take() {
                        guard.contain_without_resume(reason.clone());
                    } else {
                        contain_task_without_resume(
                            &self.pipeline.platform,
                            task,
                            &failure_sink,
                            reason.clone(),
                        );
                    }
                    failed_capture_with_snapshot(
                        event,
                        report_context,
                        &reason,
                        fallback_snapshot.as_deref(),
                        self.pipeline.collection_policy.persist_raw_shm,
                    )
                }
            };
            let outcome = attach_owned_snapshot(outcome, fallback_snapshot);
            return with_task_control_diagnostics(
                outcome,
                suspend_error,
                snapshot_error,
                &failure_sink,
            );
        }

        let cancelled = Arc::new(AtomicBool::new(false));
        let (result_tx, result_rx) = sync_channel(1);
        let event_for_result = event.clone();
        let report_context_for_result = report_context.clone();
        // Keep a cheap Arc clone on the guard-owning thread. If the worker
        // handoff fails or times out, Stage 1 can still persist the immutable
        // bytes that were already copied while the task was suspended.
        let fallback_shm_snapshot = shm_snapshot.clone();
        let job = CaptureJob {
            event,
            report_context,
            cancelled: cancelled.clone(),
            shm_snapshot,
            result_tx,
        };

        let Some(sender) = &self.sender else {
            finish_suspend(&mut suspend_guard);
            return with_task_control_diagnostics(
                failed_capture_with_snapshot(
                    job.event,
                    job.report_context,
                    "capture worker is unavailable",
                    fallback_shm_snapshot.as_deref(),
                    self.pipeline.collection_policy.persist_raw_shm,
                ),
                suspend_error,
                snapshot_error,
                &failure_sink,
            );
        };

        if let Err(error) = sender.try_send(job) {
            let (event, report_context) = match error {
                TrySendError::Full(job) | TrySendError::Disconnected(job) => {
                    (job.event, job.report_context)
                }
            };
            finish_suspend(&mut suspend_guard);
            self.retire("capture worker queue unavailable");
            return with_task_control_diagnostics(
                failed_capture_with_snapshot(
                    event,
                    report_context,
                    "capture worker queue unavailable",
                    fallback_shm_snapshot.as_deref(),
                    self.pipeline.collection_policy.persist_raw_shm,
                ),
                suspend_error,
                snapshot_error,
                &failure_sink,
            );
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        let result = result_rx.recv_timeout(remaining);
        if result.is_err() {
            // Publish cancellation before resume so a collector that returns
            // at the deadline cannot start another task-facing collector.
            cancelled.store(true, Ordering::Release);
        }
        finish_suspend(&mut suspend_guard);

        let outcome = match result {
            Ok(payload) => CaptureOutcome::Captured(Box::new(CapturedEvent::with_report_context(
                event_for_result,
                report_context_for_result,
                payload,
            ))),
            Err(RecvTimeoutError::Timeout) => {
                self.retire("capture worker exceeded absolute deadline");
                timed_out_capture_with_snapshot(
                    event_for_result,
                    report_context_for_result,
                    "capture worker exceeded absolute deadline",
                    fallback_shm_snapshot.as_deref(),
                    self.pipeline.collection_policy.persist_raw_shm,
                )
            }
            Err(RecvTimeoutError::Disconnected) => {
                self.retire("capture worker disconnected");
                failed_capture_with_snapshot(
                    event_for_result,
                    report_context_for_result,
                    "capture worker disconnected",
                    fallback_shm_snapshot.as_deref(),
                    self.pipeline.collection_policy.persist_raw_shm,
                )
            }
        };
        with_task_control_diagnostics(outcome, suspend_error, snapshot_error, &failure_sink)
    }

    fn retire(&mut self, reason: &str) {
        self.sender.take();
        self.unavailable_reason = Some(reason.to_string());
    }

    pub(crate) fn shutdown(mut self, timeout: Duration) {
        self.sender.take();
        finish_thread(&self.done_rx, self.handle.take(), timeout);
    }

    pub(crate) fn detach(mut self) {
        self.sender.take();
        self.handle.take();
    }
}

fn finish_suspend(guard: &mut Option<TaskSuspendGuard>) {
    if let Some(guard) = guard.take() {
        guard.finish();
    }
}

fn attach_owned_snapshot(
    mut outcome: CaptureOutcome,
    snapshot: Option<Arc<crate::shm::OwnedShmSnapshot>>,
) -> CaptureOutcome {
    if let CaptureOutcome::Captured(captured) = &mut outcome {
        captured.attach_owned_shm_snapshot(snapshot);
    }
    outcome
}

fn defer_owned_if_isolated(
    outcome: CaptureOutcome,
    isolated: bool,
    snapshot: Option<Arc<crate::shm::OwnedShmSnapshot>>,
) -> CaptureOutcome {
    if isolated {
        attach_owned_snapshot(outcome, snapshot)
    } else {
        outcome
    }
}

fn with_task_control_diagnostics(
    mut outcome: CaptureOutcome,
    suspend_error: Option<String>,
    snapshot_error: Option<String>,
    failure_sink: &TaskControlFailureSink,
) -> CaptureOutcome {
    let diagnostics = match &mut outcome {
        CaptureOutcome::Captured(captured) => &mut captured.diagnostics,
        CaptureOutcome::Skipped(diagnostics) => diagnostics,
    };
    if let Some(error) = suspend_error {
        diagnostics.record_immediate("CaptureSuspend", PluginStatus::Error(error));
    }
    if let Some(error) = snapshot_error {
        diagnostics.record_immediate("ShmSnapshot", PluginStatus::Error(error));
    }
    failure_sink.drain_into(diagnostics);
    outcome
}

fn timed_out_capture(
    event: super::CrashEvent,
    report_context: Arc<super::ReportContext>,
    reason: &str,
) -> CaptureOutcome {
    minimal_capture(event, report_context, reason, PluginStatus::TimedOut, None)
}

fn timed_out_capture_with_snapshot(
    event: super::CrashEvent,
    report_context: Arc<super::ReportContext>,
    reason: &str,
    snapshot: Option<&crate::shm::OwnedShmSnapshot>,
    persist_raw_shm: bool,
) -> CaptureOutcome {
    minimal_capture(
        event,
        report_context,
        reason,
        PluginStatus::TimedOut,
        raw_shm_from_snapshot(snapshot, persist_raw_shm),
    )
}

fn failed_capture(
    event: super::CrashEvent,
    report_context: Arc<super::ReportContext>,
    reason: &str,
) -> CaptureOutcome {
    minimal_capture(
        event,
        report_context,
        reason,
        PluginStatus::Error(reason.to_string()),
        None,
    )
}

fn failed_capture_with_snapshot(
    event: super::CrashEvent,
    report_context: Arc<super::ReportContext>,
    reason: &str,
    snapshot: Option<&crate::shm::OwnedShmSnapshot>,
    persist_raw_shm: bool,
) -> CaptureOutcome {
    minimal_capture(
        event,
        report_context,
        reason,
        PluginStatus::Error(reason.to_string()),
        raw_shm_from_snapshot(snapshot, persist_raw_shm),
    )
}

fn raw_shm_from_snapshot(
    snapshot: Option<&crate::shm::OwnedShmSnapshot>,
    persist_raw_shm: bool,
) -> Option<super::RawShmSnapshot> {
    if !persist_raw_shm {
        return None;
    }
    snapshot.map(|snapshot| super::RawShmSnapshot {
        breadcrumbs: snapshot.raw_breadcrumb_bytes_owned(),
        context: snapshot.raw_context_bytes_owned(),
    })
}

fn minimal_capture(
    event: super::CrashEvent,
    report_context: Arc<super::ReportContext>,
    reason: &str,
    status: PluginStatus,
    raw_shm: Option<super::RawShmSnapshot>,
) -> CaptureOutcome {
    eprintln!("[monitor] {reason}; continuing with minimum capture payload");
    let mut diagnostics = Diagnostics::new();
    diagnostics.record_immediate("CaptureWorker", status);
    CaptureOutcome::Captured(Box::new(CapturedEvent::with_report_context(
        event,
        report_context,
        CapturePayload {
            data: super::CollectedData::default(),
            raw_shm,
            diagnostics,
        },
    )))
}

/// Bounded, non-blocking queue for manual snapshot and ANR finalization.
pub(crate) struct BackgroundFinalizeWorker {
    sender: Option<SyncSender<CapturedEvent>>,
    done_rx: Receiver<()>,
    handle: Option<JoinHandle<()>>,
}

impl BackgroundFinalizeWorker {
    pub(crate) fn start(pipeline: Arc<Pipeline>) -> Self {
        let (sender, receiver) = sync_channel(BACKGROUND_QUEUE_CAPACITY);
        let (done_tx, done_rx) = mpsc::channel();
        let spawn = thread::Builder::new()
            .name("crash-finalize-background".into())
            .spawn(move || {
                while let Ok(captured) = receiver.recv() {
                    let _ = pipeline.finalize_captured_for_worker(captured);
                }
                let _ = done_tx.send(());
            });

        match spawn {
            Ok(handle) => Self {
                sender: Some(sender),
                done_rx,
                handle: Some(handle),
            },
            Err(error) => {
                eprintln!("[monitor] failed to start background finalizer: {error}");
                Self {
                    sender: None,
                    done_rx,
                    handle: None,
                }
            }
        }
    }

    #[must_use]
    pub(crate) fn try_submit(&self, captured: CapturedEvent) -> bool {
        let Some(sender) = &self.sender else {
            eprintln!("[monitor] background finalizer unavailable; dropping event");
            return false;
        };
        match sender.try_send(captured) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                eprintln!("[monitor] background finalizer queue full; dropping event");
                false
            }
            Err(TrySendError::Disconnected(_)) => {
                eprintln!("[monitor] background finalizer disconnected; dropping event");
                false
            }
        }
    }

    pub(crate) fn shutdown(mut self, timeout: Duration) {
        self.sender.take();
        finish_thread(&self.done_rx, self.handle.take(), timeout);
    }

    pub(crate) fn detach(mut self) {
        self.sender.take();
        self.handle.take();
    }
}

/// Run task-independent exit/signal report work off the event-loop thread.
/// The caller may wait for this bounded result, but JSON/post-processors and
/// notifiers execute only on the worker.
pub(crate) fn finalize_terminated_child(
    pipeline: Arc<Pipeline>,
    event: super::CrashEvent,
    timeout: Duration,
) -> Option<Diagnostics> {
    if !pipeline.report_enabled(event.report_type) {
        return Some(Diagnostics::new());
    }
    let (result_tx, result_rx) = sync_channel(1);
    let spawn = thread::Builder::new()
        .name("crash-finalize-termination".into())
        .spawn(move || {
            let diagnostics = pipeline.finalize_termination_event_for_worker(&event);
            let _ = result_tx.send(diagnostics);
        });
    let handle = match spawn {
        Ok(handle) => handle,
        Err(error) => {
            eprintln!("[monitor] failed to start child-termination finalizer: {error}");
            return None;
        }
    };

    match result_rx.recv_timeout(timeout) {
        Ok(diagnostics) => {
            let _ = handle.join();
            Some(diagnostics)
        }
        Err(RecvTimeoutError::Disconnected) => {
            let _ = handle.join();
            None
        }
        Err(RecvTimeoutError::Timeout) => {
            // The pipeline thread is intentionally detached. Every plugin
            // expected to wait indefinitely must place that payload behind
            // the killable subprocess supervisor; audited cooperative work
            // can still be inside a non-preemptible synchronous OS call.
            None
        }
    }
}

/// Fatal reports wait on a one-shot termination handoff so JSON and ZIP are
/// born with the final wait status rather than being patched afterward.
pub enum CrashFinalization {
    Pending(CrashFinalizeTicket),
    Deferred(Box<CapturedEvent>),
}

impl CrashFinalization {
    pub(crate) fn start(pipeline: Arc<Pipeline>, captured: Box<CapturedEvent>) -> Self {
        match CrashFinalizeTicket::spawn(pipeline, *captured) {
            Ok(ticket) => Self::Pending(ticket),
            Err(captured) => Self::Deferred(captured),
        }
    }

    /// Complete fatal finalization after the supervisor has reaped the child.
    #[must_use]
    pub fn complete(
        self,
        pipeline: Arc<Pipeline>,
        reason: Option<TerminationReason>,
        timeout: Duration,
    ) -> Option<Diagnostics> {
        match self {
            Self::Pending(ticket) => ticket.complete(reason, timeout),
            Self::Deferred(mut captured) => {
                captured.diagnostics.record_immediate(
                    "CrashFinalizer",
                    PluginStatus::Error("initial worker spawn failed; retrying".into()),
                );
                let emergency_pipeline = pipeline.clone();
                match CrashFinalizeTicket::spawn(pipeline, *captured) {
                    Ok(ticket) => ticket.complete(reason, timeout),
                    Err(captured) => {
                        let mut captured = *captured;
                        captured.diagnostics.record_immediate(
                            "CrashFinalizer",
                            PluginStatus::Error("worker spawn retry failed".into()),
                        );
                        captured.set_termination(reason);
                        Some(emergency_finalize_captured(&emergency_pipeline, captured))
                    }
                }
            }
        }
    }
}

/// Last-resort fatal-event persistence when no finalizer thread can be
/// created. This deliberately runs no filters, processors, or notifiers: the
/// current supervisor thread writes only already-owned evidence through the
/// normal private, atomic artifact transaction.
fn emergency_finalize_captured(pipeline: &Pipeline, mut captured: CapturedEvent) -> Diagnostics {
    if !pipeline.report_enabled(captured.event.report_type) {
        return Diagnostics::new();
    }

    let report_context = match captured.report_context.take() {
        Some(report_context) => report_context,
        None => match pipeline.create_report_context(&captured.event) {
            Ok(report_context) => report_context,
            Err(error) => {
                captured
                    .diagnostics
                    .record_immediate("EmergencyArtifactBegin", PluginStatus::Error(error));
                return captured.diagnostics;
            }
        },
    };
    let transaction = match super::ArtifactTransaction::begin_shared(report_context) {
        Ok(transaction) => transaction,
        Err(error) => {
            captured
                .diagnostics
                .record_immediate("EmergencyArtifactBegin", PluginStatus::Error(error));
            return captured.diagnostics;
        }
    };

    let raw_path = match super::safety::write_raw_stage1(&transaction, &captured.data.raw.threads) {
        Ok(path) => {
            captured
                .diagnostics
                .record_immediate("EmergencyStage1Raw", PluginStatus::Ok);
            Some(path)
        }
        Err(error) => {
            captured
                .diagnostics
                .record_immediate("EmergencyStage1Raw", PluginStatus::Error(error));
            None
        }
    };

    if let Some(raw_shm) = &captured.raw_shm {
        let status = match super::safety::write_raw_shm_stage1(&transaction, raw_shm) {
            Ok(()) => PluginStatus::Ok,
            Err(error) => PluginStatus::Error(error),
        };
        captured
            .diagnostics
            .record_immediate("EmergencyStage1Shm", status);
    }

    // JSON is best-effort. It is produced without screenshot conversion or
    // any other extension point so resource exhaustion cannot enter another
    // unbounded plugin path.
    let json_path = {
        let formatted =
            crate::preprocessors::report_formatter::format(&captured.data, &captured.diagnostics);
        let mut report = super::report::build_report(
            &captured.event,
            formatted,
            captured.data.fingerprint.clone(),
        );
        match super::report::write_report(&transaction, &mut report, &[]) {
            Ok(path) => Some(path),
            Err(error) => {
                captured
                    .diagnostics
                    .record_immediate("EmergencyStage2Json", PluginStatus::Error(error));
                None
            }
        }
    };

    if transaction.artifact_paths().is_empty() {
        captured.diagnostics.record_immediate(
            "EmergencyArtifactCommit",
            PluginStatus::Error("no emergency artifacts could be written".into()),
        );
        return captured.diagnostics;
    }

    let staging_dir = transaction.staging_dir().to_path_buf();
    let committed = match transaction.commit() {
        Ok(committed) => committed,
        Err(error) => {
            captured
                .diagnostics
                .record_immediate("EmergencyArtifactCommit", PluginStatus::Error(error));
            return captured.diagnostics;
        }
    };
    for warning in &committed.durability_warnings {
        captured.diagnostics.record_immediate(
            "EmergencyArtifactDurability",
            PluginStatus::Error(warning.clone()),
        );
    }

    let committed_path = |staged_path: &std::path::Path| {
        staged_path
            .strip_prefix(&staging_dir)
            .ok()
            .map(|relative| committed.report_dir.join(relative))
    };
    captured.diagnostics.report_path = json_path
        .as_deref()
        .and_then(&committed_path)
        .or_else(|| raw_path.as_deref().and_then(committed_path));
    transaction.release_publication_lease();
    captured.diagnostics
}

pub struct CrashFinalizeTicket {
    termination_tx: SyncSender<Option<TerminationReason>>,
    result_rx: Receiver<Diagnostics>,
    done_rx: Receiver<()>,
    handle: Option<JoinHandle<()>>,
}

impl CrashFinalizeTicket {
    fn spawn(pipeline: Arc<Pipeline>, captured: CapturedEvent) -> Result<Self, Box<CapturedEvent>> {
        #[cfg(test)]
        if TEST_FATAL_SPAWN_FAILURES.with(|remaining| {
            let count = remaining.get();
            if count == 0 {
                false
            } else {
                remaining.set(count - 1);
                true
            }
        }) {
            eprintln!("[monitor] injected fatal finalizer spawn failure");
            return Err(Box::new(captured));
        }

        let (termination_tx, termination_rx) = sync_channel(1);
        let (result_tx, result_rx) = sync_channel(1);
        let (done_tx, done_rx) = mpsc::channel();
        let captured_slot = Arc::new(Mutex::new(Some(captured)));
        let worker_slot = captured_slot.clone();
        let spawn = thread::Builder::new()
            .name("crash-finalize-fatal".into())
            .spawn(move || {
                let mut captured = take_captured(&worker_slot)
                    .expect("fatal finalizer capture slot must contain one event");
                match termination_rx.recv() {
                    Ok(reason) => captured.set_termination(reason),
                    Err(error) => captured.diagnostics.record_immediate(
                        "TerminationHandoff",
                        PluginStatus::Error(format!("termination handoff failed: {error}")),
                    ),
                }
                let diagnostics = pipeline.finalize_captured_for_worker(captured);
                let _ = result_tx.send(diagnostics);
                let _ = done_tx.send(());
            });

        match spawn {
            Ok(handle) => Ok(Self {
                termination_tx,
                result_rx,
                done_rx,
                handle: Some(handle),
            }),
            Err(error) => {
                eprintln!("[monitor] failed to start fatal finalizer: {error}");
                Err(Box::new(take_captured(&captured_slot).expect(
                    "failed spawn must leave fatal capture in its slot",
                )))
            }
        }
    }

    fn complete(
        mut self,
        reason: Option<TerminationReason>,
        timeout: Duration,
    ) -> Option<Diagnostics> {
        let _ = self.termination_tx.send(reason);
        let result = self.result_rx.recv_timeout(timeout).ok();
        if result.is_some() {
            finish_thread(
                &self.done_rx,
                self.handle.take(),
                Duration::from_millis(100),
            );
        }
        result
    }
}

fn take_captured(slot: &Mutex<Option<CapturedEvent>>) -> Option<CapturedEvent> {
    match slot.lock() {
        Ok(mut guard) => guard.take(),
        Err(poisoned) => poisoned.into_inner().take(),
    }
}

fn finish_thread(done_rx: &Receiver<()>, handle: Option<JoinHandle<()>>, timeout: Duration) {
    let finished = match done_rx.recv_timeout(timeout) {
        Ok(()) | Err(RecvTimeoutError::Disconnected) => true,
        Err(RecvTimeoutError::Timeout) => false,
    };
    if finished && let Some(handle) = handle {
        let _ = handle.join();
    }
    // On timeout the JoinHandle is dropped and the isolated worker is detached.
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::{
        CollectedData, Collector, CrashEvent, Plugin, PluginContext, PluginExecution,
        PostProcessor, Priority, ReportResult, ReportType, TriggerPolicy,
    };
    use crate::platform::{PlatformOps, mock::MockPlatform};
    use nix::libc;
    use std::sync::Condvar;
    use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

    struct BlockingPostProcessor {
        entered_tx: SyncSender<()>,
        completed_tx: SyncSender<()>,
        release: Arc<(Mutex<bool>, Condvar)>,
        calls: Arc<AtomicUsize>,
    }

    impl Plugin for BlockingPostProcessor {
        fn name(&self) -> &'static str {
            "BlockingPostProcessor"
        }

        fn execution(&self) -> PluginExecution {
            PluginExecution::Cooperative
        }

        fn priority(&self) -> Priority {
            Priority::Low
        }
    }

    impl PostProcessor for BlockingPostProcessor {
        fn process(
            &self,
            _event: &CrashEvent,
            _result: &mut ReportResult,
            _context: &PluginContext,
        ) -> Result<(), String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let _ = self.entered_tx.send(());
            let (lock, condvar) = &*self.release;
            let guard = match lock.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            let wait = condvar
                .wait_timeout_while(guard, Duration::from_secs(5), |released| !*released)
                .map_err(|_| "release mutex poisoned".to_string())?;
            if wait.1.timed_out() {
                return Err("release timed out".into());
            }
            let _ = self.completed_tx.send(());
            Ok(())
        }
    }

    struct PostResumeSnapshotCollector {
        entered_tx: SyncSender<()>,
        observed_tx: SyncSender<String>,
        release: Arc<(Mutex<bool>, Condvar)>,
    }

    struct BlockingCaptureCollector {
        entered_tx: SyncSender<()>,
        release: Arc<(Mutex<bool>, Condvar)>,
    }

    struct CountingOwnedCollector(Arc<AtomicUsize>);

    impl Plugin for CountingOwnedCollector {
        fn name(&self) -> &'static str {
            "CountingOwnedCollector"
        }

        fn execution(&self) -> PluginExecution {
            PluginExecution::Cooperative
        }

        fn priority(&self) -> Priority {
            Priority::Low
        }
    }

    impl Collector for CountingOwnedCollector {
        fn collect(
            &self,
            _event: &CrashEvent,
            task: mach_port_t,
            _data: &mut CollectedData,
            _context: &PluginContext,
        ) -> Result<(), String> {
            assert_eq!(task, mach2::port::MACH_PORT_NULL);
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    impl Plugin for BlockingCaptureCollector {
        fn name(&self) -> &'static str {
            "BlockingCaptureCollector"
        }

        fn execution(&self) -> PluginExecution {
            PluginExecution::Cooperative
        }

        fn priority(&self) -> Priority {
            Priority::Critical
        }
    }

    impl Collector for BlockingCaptureCollector {
        fn collect(
            &self,
            _event: &CrashEvent,
            _task: mach_port_t,
            _data: &mut CollectedData,
            _context: &PluginContext,
        ) -> Result<(), String> {
            let _ = self.entered_tx.send(());
            let (lock, condvar) = &*self.release;
            let guard = match lock.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            let wait = condvar
                .wait_timeout_while(guard, Duration::from_secs(5), |released| !*released)
                .map_err(|_| "release mutex poisoned".to_string())?;
            if wait.1.timed_out() {
                return Err("release timed out".into());
            }
            Ok(())
        }
    }

    impl Plugin for PostResumeSnapshotCollector {
        fn name(&self) -> &'static str {
            "PostResumeSnapshotCollector"
        }

        fn execution(&self) -> PluginExecution {
            PluginExecution::Cooperative
        }

        fn priority(&self) -> Priority {
            Priority::Critical
        }
    }

    impl Collector for PostResumeSnapshotCollector {
        fn collect(
            &self,
            _event: &CrashEvent,
            _task: mach_port_t,
            _data: &mut CollectedData,
            context: &PluginContext,
        ) -> Result<(), String> {
            let _ = self.entered_tx.send(());
            let (lock, condvar) = &*self.release;
            let guard = match lock.lock() {
                Ok(guard) => guard,
                Err(poisoned) => poisoned.into_inner(),
            };
            let wait = condvar
                .wait_timeout_while(guard, Duration::from_secs(5), |released| !*released)
                .map_err(|_| "release mutex poisoned".to_string())?;
            if wait.1.timed_out() {
                return Err("release timed out".into());
            }

            let value = context
                .shm_snapshot()
                .and_then(crate::shm::OwnedShmSnapshot::read_context)
                .map(|snapshot| snapshot.app_version)
                .ok_or_else(|| "owned shared-memory context unavailable".to_string())?;
            let _ = self.observed_tx.send(value);
            Ok(())
        }
    }

    fn unique_shm_pid() -> u32 {
        static NEXT_PID: AtomicU32 = AtomicU32::new(1_800_000);
        NEXT_PID.fetch_add(1, Ordering::Relaxed)
    }

    fn store_shm_context_generation(shm: &crate::shm::SharedMemory, value: u32) {
        let offset = crate::shm::SECTION1_OFFSET
            + std::mem::offset_of!(crate::shm::ShmHeader, context_generation);
        // SAFETY: the schema guarantees natural alignment and the test keeps
        // the complete mapping alive while performing the atomic store.
        #[allow(clippy::cast_ptr_alignment)] // schema offset is compile-time aligned
        let generation = unsafe { &*shm.base_ptr().add(offset).cast::<AtomicU32>() };
        generation.store(value, Ordering::Release);
    }

    fn write_shm_app_version(shm: &crate::shm::SharedMemory, value: &str) {
        const FIELD_LEN: usize = 16;
        assert!(value.len() < FIELD_LEN);
        let offset = crate::shm::CONTEXT_OFFSET
            + std::mem::offset_of!(crate::shm::SutCrashContext, app_version);
        store_shm_context_generation(shm, 1);
        // SAFETY: this test writes one bounded schema field. The initial write
        // precedes capture; the second happens only after resume.
        unsafe {
            let field = shm.base_ptr().add(offset);
            std::ptr::write_bytes(field, 0, FIELD_LEN);
            std::ptr::copy_nonoverlapping(value.as_ptr(), field, value.len());
        }
        store_shm_context_generation(shm, 2);
    }

    fn write_shm_breadcrumb_marker(shm: &crate::shm::SharedMemory, value: u8) {
        let offset = crate::shm::SECTION2_OFFSET
            + std::mem::offset_of!(crate::shm::SutCrumbState, rings)
            + std::mem::offset_of!(crate::shm::SutCrumbRing, buf)
            + std::mem::offset_of!(crate::shm::SutBreadcrumb, message);
        // SAFETY: the marker is one byte inside the complete live mapping and
        // is written before capture starts.
        unsafe {
            *shm.base_ptr().add(offset) = value;
        }
    }

    fn assert_preserved_stage1_shm(
        captured: &CapturedEvent,
        expected_app_version: &str,
        expected_breadcrumb_marker: u8,
    ) {
        let raw_shm = captured
            .raw_shm
            .as_ref()
            .expect("fallback must retain the already-owned SHM bytes");
        assert_eq!(raw_shm.breadcrumbs.len(), crate::shm::SECTION2_SIZE);
        let marker_offset = std::mem::offset_of!(crate::shm::SutCrumbState, rings)
            + std::mem::offset_of!(crate::shm::SutCrumbRing, buf)
            + std::mem::offset_of!(crate::shm::SutBreadcrumb, message);
        assert_eq!(
            raw_shm.breadcrumbs[marker_offset],
            expected_breadcrumb_marker
        );

        let expected_context_len = std::mem::size_of::<crate::shm::SutCrashContext>()
            + std::mem::size_of::<crate::shm::SutCrashSettingsSnapshot>();
        assert_eq!(raw_shm.context.len(), expected_context_len);
        let app_version_offset = std::mem::offset_of!(crate::shm::SutCrashContext, app_version);
        assert_eq!(
            &raw_shm.context[app_version_offset..app_version_offset + expected_app_version.len()],
            expected_app_version.as_bytes()
        );
        assert_eq!(
            raw_shm.context[app_version_offset + expected_app_version.len()],
            0
        );
        assert!(
            captured
                .diagnostics
                .plugins
                .iter()
                .all(|entry| entry.name != "ShmSnapshot"),
            "a stable owned snapshot must not add a success diagnostic"
        );
    }

    struct WorkerFixture {
        pipeline: Arc<Pipeline>,
        entered_rx: Receiver<()>,
        completed_rx: Receiver<()>,
        release: Arc<(Mutex<bool>, Condvar)>,
        calls: Arc<AtomicUsize>,
        tempdir: tempfile::TempDir,
    }

    struct FatalSpawnFailureReset;

    impl Drop for FatalSpawnFailureReset {
        fn drop(&mut self) {
            TEST_FATAL_SPAWN_FAILURES.with(|remaining| remaining.set(0));
        }
    }

    fn inject_fatal_spawn_failures(count: usize) -> FatalSpawnFailureReset {
        TEST_FATAL_SPAWN_FAILURES.with(|remaining| remaining.set(count));
        FatalSpawnFailureReset
    }

    fn worker_fixture_with_enabled(enabled: bool) -> WorkerFixture {
        let tempdir = tempfile::tempdir().unwrap();
        let (entered_tx, entered_rx) = sync_channel(8);
        let (completed_tx, completed_rx) = sync_channel(8);
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let calls = Arc::new(AtomicUsize::new(0));
        let pipeline = Arc::new(Pipeline {
            enabled,
            triggers: TriggerPolicy::ALL_ENABLED,
            collection_policy: crate::config::CollectionPolicy::FULL,
            filters: vec![],
            collectors: vec![],
            pre_processors: vec![],
            post_processors: vec![Box::new(BlockingPostProcessor {
                entered_tx,
                completed_tx,
                release: release.clone(),
                calls: calls.clone(),
            })],
            notifiers: vec![],
            shm: None,
            platform: Arc::new(MockPlatform::default()),
            output_dir: Some(tempdir.path().to_path_buf()),
        });
        WorkerFixture {
            pipeline,
            entered_rx,
            completed_rx,
            release,
            calls,
            tempdir,
        }
    }

    fn worker_fixture() -> WorkerFixture {
        worker_fixture_with_enabled(true)
    }

    fn captured(pid: u32) -> CapturedEvent {
        CapturedEvent::new(
            CrashEvent {
                report_id: crate::pipeline::ReportId::default(),
                report_type: ReportType::Snapshot,
                termination: None,
                exception_type: None,
                exception_code: None,
                exception_subcode: None,
                exception_codes: Vec::new(),
                crashed_thread: None,
                bail_on_suspend_failure: false,
                pid,
                process_name: "test".into(),
                hang_duration_ms: None,
            },
            CapturePayload {
                data: CollectedData::default(),
                raw_shm: None,
                diagnostics: Diagnostics::new(),
            },
        )
    }

    fn release(fixture: &WorkerFixture) {
        let (lock, condvar) = &*fixture.release;
        let mut released = match lock.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        *released = true;
        condvar.notify_all();
    }

    #[test]
    fn expired_capture_worker_deadline_is_timed_out() {
        let fixture = worker_fixture();
        let mut worker = CaptureWorker::start(fixture.pipeline.clone(), 0);
        let event = captured(9).event;

        let outcome = worker.capture(event, 0, Instant::now());
        let CaptureOutcome::Captured(captured) = outcome else {
            panic!("an elapsed capture deadline should keep a minimum payload");
        };
        let status = captured
            .diagnostics
            .plugins
            .iter()
            .find(|entry| entry.name == "CaptureWorker")
            .map(|entry| &entry.status);
        assert!(matches!(status, Some(PluginStatus::TimedOut)));

        worker.shutdown(Duration::from_secs(1));
    }

    #[test]
    fn disabled_capture_worker_returns_before_task_suspend() {
        let tempdir = tempfile::tempdir().unwrap();
        let platform = Arc::new(MockPlatform::default());
        let pipeline = Arc::new(Pipeline {
            enabled: false,
            triggers: TriggerPolicy::ALL_ENABLED,
            collection_policy: crate::config::CollectionPolicy::FULL,
            filters: vec![],
            collectors: vec![],
            pre_processors: vec![],
            post_processors: vec![],
            notifiers: vec![],
            shm: None,
            platform: platform.clone(),
            output_dir: Some(tempdir.path().to_path_buf()),
        });
        let mut worker = CaptureWorker::start(pipeline, 123);

        let outcome = worker.capture(
            captured(9).event,
            123,
            Instant::now() + Duration::from_secs(1),
        );

        assert!(matches!(outcome, CaptureOutcome::Skipped(_)));
        assert_eq!(platform.suspend_count(), 0);
        assert_eq!(platform.resume_count(), 0);
        assert_eq!(platform.retain_task_port_count(), 0);
        assert_eq!(platform.deallocate_task_port_count(), 0);
        assert!(std::fs::read_dir(tempdir.path()).unwrap().next().is_none());
        worker.shutdown(Duration::from_secs(1));
    }

    #[test]
    fn isolated_capture_resumes_only_after_helper_completion() {
        let tempdir = tempfile::tempdir().unwrap();
        let platform = Arc::new(MockPlatform::default().with_capture_isolation());
        let owned_calls = Arc::new(AtomicUsize::new(0));
        let pipeline = Arc::new(Pipeline {
            enabled: true,
            triggers: TriggerPolicy::ALL_ENABLED,
            collection_policy: crate::config::CollectionPolicy::FULL,
            filters: vec![],
            collectors: vec![
                Box::new(crate::collectors::DylibCollector::new(platform.clone())),
                Box::new(CountingOwnedCollector(owned_calls.clone())),
            ],
            pre_processors: vec![],
            post_processors: vec![],
            notifiers: vec![],
            shm: None,
            platform: platform.clone(),
            output_dir: Some(tempdir.path().to_path_buf()),
        });
        let observed = platform.clone();
        let _reset = super::super::capture_isolation::inject_test_outcome(move || {
            assert_eq!(
                observed.resume_count(),
                0,
                "the task must remain suspended while the helper owns task rights"
            );
            super::super::capture_isolation::IsolatedCaptureOutcome::Completed(
                super::super::capture_isolation::TaskCaptureData::default(),
                Vec::new(),
            )
        });

        let mut worker = CaptureWorker::start(pipeline.clone(), 90);
        let outcome = worker.capture(
            captured(90).event,
            90,
            Instant::now() + Duration::from_secs(1),
        );

        let CaptureOutcome::Captured(captured) = outcome else {
            panic!("isolated capture should produce an owned event");
        };
        assert_eq!(owned_calls.load(Ordering::SeqCst), 0);
        assert_eq!(platform.suspend_count(), 1);
        assert_eq!(platform.resume_count(), 1);
        assert_eq!(platform.terminate_count(), 0);
        assert_eq!(platform.retain_task_port_count(), 0);
        let _ = pipeline.finalize_captured_for_worker(*captured);
        assert_eq!(owned_calls.load(Ordering::SeqCst), 1);
        worker.shutdown(Duration::from_secs(1));
    }

    #[test]
    fn unproven_helper_cleanup_terminates_without_resume() {
        let tempdir = tempfile::tempdir().unwrap();
        let platform = Arc::new(MockPlatform::default().with_capture_isolation());
        let pipeline = Arc::new(Pipeline {
            enabled: true,
            triggers: TriggerPolicy::ALL_ENABLED,
            collection_policy: crate::config::CollectionPolicy::FULL,
            filters: vec![],
            collectors: vec![Box::new(crate::collectors::DylibCollector::new(
                platform.clone(),
            ))],
            pre_processors: vec![],
            post_processors: vec![],
            notifiers: vec![],
            shm: None,
            platform: platform.clone(),
            output_dir: Some(tempdir.path().to_path_buf()),
        });
        let _reset = super::super::capture_isolation::inject_test_outcome(|| {
            super::super::capture_isolation::IsolatedCaptureOutcome::CleanupUnproven(
                "injected waitpid failure".into(),
            )
        });

        let mut worker = CaptureWorker::start(pipeline, 91);
        let outcome = worker.capture(
            captured(91).event,
            91,
            Instant::now() + Duration::from_secs(1),
        );
        let CaptureOutcome::Captured(captured) = outcome else {
            panic!("cleanup containment should retain a minimum payload");
        };

        assert_eq!(platform.suspend_count(), 1);
        assert_eq!(platform.resume_count(), 0);
        assert_eq!(platform.terminate_count(), 1);
        assert!(captured.diagnostics.plugins.iter().any(|entry| {
            entry.name == "TaskResume" && matches!(entry.status, PluginStatus::Error(_))
        }));
        assert!(!platform.supervisor_health().requires_escalation());
        worker.shutdown(Duration::from_secs(1));
    }

    #[test]
    fn detached_capture_worker_owns_task_send_right_until_thread_exit() {
        let tempdir = tempfile::tempdir().unwrap();
        let platform = Arc::new(MockPlatform::default());
        let (entered_tx, entered_rx) = sync_channel(1);
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let pipeline = Arc::new(Pipeline {
            enabled: true,
            triggers: TriggerPolicy::ALL_ENABLED,
            collection_policy: crate::config::CollectionPolicy::FULL,
            filters: vec![],
            collectors: vec![Box::new(BlockingCaptureCollector {
                entered_tx,
                release: release.clone(),
            })],
            pre_processors: vec![],
            post_processors: vec![],
            notifiers: vec![],
            shm: None,
            platform: platform.clone(),
            output_dir: Some(tempdir.path().to_path_buf()),
        });

        let mut worker = CaptureWorker::start(pipeline, 71);
        assert_eq!(platform.retain_task_port_count(), 1);
        assert_eq!(platform.deallocate_task_port_count(), 0);

        let outcome = worker.capture(
            captured(71).event,
            71,
            Instant::now() + Duration::from_millis(50),
        );
        assert!(matches!(outcome, CaptureOutcome::Captured(_)));
        entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("collector should remain in flight after capture timeout");
        assert_eq!(platform.resume_count(), 1);
        assert_eq!(platform.deallocate_task_port_count(), 0);

        worker.detach();
        assert_eq!(platform.deallocate_task_port_count(), 0);
        let (lock, condvar) = &*release;
        let mut released = match lock.lock() {
            Ok(released) => released,
            Err(poisoned) => poisoned.into_inner(),
        };
        *released = true;
        condvar.notify_all();
        drop(released);

        let started = Instant::now();
        while platform.deallocate_task_port_count() == 0 {
            assert!(
                started.elapsed() < Duration::from_secs(1),
                "detached worker did not release its retained task send right"
            );
            std::thread::yield_now();
        }
        assert_eq!(platform.deallocate_task_port_count(), 1);
    }

    #[test]
    fn capture_worker_diagnoses_and_retains_sanitized_torn_snapshot() {
        let tempdir = tempfile::tempdir().unwrap();
        let shm = Arc::new(
            crate::shm::SharedMemory::create(unique_shm_pid()).expect("create shared memory"),
        );
        write_shm_app_version(&shm, "published");
        // Model a producer that began the next context publication but did not
        // finish before the monitor suspended it.
        store_shm_context_generation(&shm, 3);

        let platform = Arc::new(MockPlatform::default());
        let pipeline = Arc::new(Pipeline {
            enabled: true,
            triggers: TriggerPolicy::ALL_ENABLED,
            collection_policy: crate::config::CollectionPolicy::FULL,
            filters: vec![],
            collectors: vec![],
            pre_processors: vec![],
            post_processors: vec![],
            notifiers: vec![],
            shm: Some(shm),
            platform: platform.clone(),
            output_dir: Some(tempdir.path().to_path_buf()),
        });
        let mut worker = CaptureWorker::start(pipeline, 123);

        let outcome = worker.capture(
            captured(10).event,
            123,
            Instant::now() + Duration::from_secs(2),
        );
        let CaptureOutcome::Captured(captured) = outcome else {
            panic!("worker should retain the sanitized snapshot");
        };
        assert_eq!(platform.resume_count(), 1);
        let snapshot_diagnostic = captured
            .diagnostics
            .plugins
            .iter()
            .find(|entry| entry.name == "ShmSnapshot")
            .expect("worker consistency diagnostic");
        assert!(matches!(
            &snapshot_diagnostic.status,
            PluginStatus::Error(error) if error.contains("Context") && error.contains("sanitized")
        ));

        let raw_context = &captured
            .raw_shm
            .as_ref()
            .expect("sanitized snapshot remains attached")
            .context;
        let context_len = std::mem::size_of::<crate::shm::SutCrashContext>();
        assert!(raw_context[..context_len].iter().all(|byte| *byte == 0));

        worker.shutdown(Duration::from_secs(1));
    }

    #[test]
    fn timed_out_worker_reads_owned_shm_after_resume() {
        const BREADCRUMB_MARKER: u8 = 0xA5;
        let tempdir = tempfile::tempdir().unwrap();
        let shm = Arc::new(
            crate::shm::SharedMemory::create(unique_shm_pid()).expect("create shared memory"),
        );
        write_shm_app_version(&shm, "before-resume");
        write_shm_breadcrumb_marker(&shm, BREADCRUMB_MARKER);

        let platform = Arc::new(MockPlatform::default());
        let (entered_tx, entered_rx) = sync_channel(1);
        let (observed_tx, observed_rx) = sync_channel(1);
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let pipeline = Arc::new(Pipeline {
            enabled: true,
            triggers: TriggerPolicy::ALL_ENABLED,
            collection_policy: crate::config::CollectionPolicy::FULL,
            filters: vec![],
            collectors: vec![Box::new(PostResumeSnapshotCollector {
                entered_tx,
                observed_tx,
                release: release.clone(),
            })],
            pre_processors: vec![],
            post_processors: vec![],
            notifiers: vec![],
            shm: Some(shm.clone()),
            platform: platform.clone(),
            output_dir: Some(tempdir.path().to_path_buf()),
        });

        let mutator_shm = shm.clone();
        let mutator_platform = platform.clone();
        let mutator_release = release.clone();
        let mutator = std::thread::spawn(move || {
            entered_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("collector entered before capture deadline");
            let started = Instant::now();
            while mutator_platform.resume_count() == 0 {
                assert!(
                    started.elapsed() < Duration::from_secs(3),
                    "capture did not resume within the test deadline"
                );
                std::thread::yield_now();
            }
            write_shm_app_version(&mutator_shm, "after-resume");
            let (lock, condvar) = &*mutator_release;
            let mut released = match lock.lock() {
                Ok(released) => released,
                Err(poisoned) => poisoned.into_inner(),
            };
            *released = true;
            condvar.notify_all();
        });

        let mut worker = CaptureWorker::start(pipeline, 7);
        let outcome = worker.capture(
            captured(44).event,
            7,
            Instant::now() + Duration::from_secs(1),
        );
        let CaptureOutcome::Captured(captured) = outcome else {
            panic!("a timed-out worker should return the Stage 1 fallback");
        };
        assert_preserved_stage1_shm(&captured, "before-resume", BREADCRUMB_MARKER);
        assert!(matches!(
            captured
                .diagnostics
                .plugins
                .iter()
                .find(|entry| entry.name == "CaptureWorker")
                .map(|entry| &entry.status),
            Some(PluginStatus::TimedOut)
        ));
        assert_eq!(platform.resume_count(), 1);
        assert_eq!(
            observed_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("collector observed owned snapshot after resume"),
            "before-resume"
        );

        mutator.join().expect("mutator thread");
        worker.shutdown(Duration::from_secs(1));
    }

    #[test]
    fn unavailable_sender_after_snapshot_preserves_owned_shm() {
        const BREADCRUMB_MARKER: u8 = 0x5A;
        let tempdir = tempfile::tempdir().unwrap();
        let shm = Arc::new(
            crate::shm::SharedMemory::create(unique_shm_pid()).expect("create shared memory"),
        );
        write_shm_app_version(&shm, "sender-missing");
        write_shm_breadcrumb_marker(&shm, BREADCRUMB_MARKER);

        let platform = Arc::new(MockPlatform::default());
        let pipeline = Arc::new(Pipeline {
            enabled: true,
            triggers: TriggerPolicy::ALL_ENABLED,
            collection_policy: crate::config::CollectionPolicy::FULL,
            filters: vec![],
            collectors: vec![],
            pre_processors: vec![],
            post_processors: vec![],
            notifiers: vec![],
            shm: Some(shm),
            platform: platform.clone(),
            output_dir: Some(tempdir.path().to_path_buf()),
        });
        let mut worker = CaptureWorker::start(pipeline, 8);
        worker.sender.take();

        let outcome = worker.capture(
            captured(45).event,
            8,
            Instant::now() + Duration::from_secs(2),
        );
        let CaptureOutcome::Captured(captured) = outcome else {
            panic!("an unavailable sender should return the Stage 1 fallback");
        };
        assert_preserved_stage1_shm(&captured, "sender-missing", BREADCRUMB_MARKER);
        assert!(matches!(
            captured
                .diagnostics
                .plugins
                .iter()
                .find(|entry| entry.name == "CaptureWorker")
                .map(|entry| &entry.status),
            Some(PluginStatus::Error(error)) if error == "capture worker is unavailable"
        ));
        assert_eq!(platform.suspend_count(), 1);
        assert_eq!(platform.resume_count(), 1);

        worker.shutdown(Duration::from_secs(1));
    }

    #[test]
    fn unavailable_sender_never_restores_raw_shm_when_policy_denies_persistence() {
        let tempdir = tempfile::tempdir().unwrap();
        let shm = Arc::new(
            crate::shm::SharedMemory::create(unique_shm_pid()).expect("create shared memory"),
        );
        write_shm_app_version(&shm, "private-marker");
        write_shm_breadcrumb_marker(&shm, 0xC3);

        let platform = Arc::new(MockPlatform::default());
        let pipeline = Arc::new(Pipeline {
            enabled: true,
            triggers: TriggerPolicy::ALL_ENABLED,
            collection_policy: crate::config::CollectionPolicy::MINIMAL,
            filters: vec![],
            collectors: vec![
                Box::new(crate::collectors::BreadcrumbCollector::new()),
                Box::new(crate::collectors::ContextCollector::new()),
            ],
            pre_processors: vec![],
            post_processors: vec![],
            notifiers: vec![],
            shm: Some(shm),
            platform: platform.clone(),
            output_dir: Some(tempdir.path().to_path_buf()),
        });
        let mut worker = CaptureWorker::start(pipeline, 9);
        worker.sender.take();

        let outcome = worker.capture(
            captured(46).event,
            9,
            Instant::now() + Duration::from_secs(2),
        );
        let CaptureOutcome::Captured(captured) = outcome else {
            panic!("an unavailable sender should return a minimum capture");
        };
        assert!(
            captured.raw_shm.is_none(),
            "the failure fallback must not bypass the raw SHM policy"
        );
        assert_eq!(platform.suspend_count(), 1);
        assert_eq!(platform.resume_count(), 1);

        worker.shutdown(Duration::from_secs(1));
    }

    #[test]
    fn disabled_finalization_workers_accept_no_bypass_artifacts() {
        let fixture = worker_fixture_with_enabled(false);
        let mut termination_event = captured(8).event;
        termination_event.report_type = ReportType::ExitFailure;
        termination_event.termination = Some(TerminationReason::Exited {
            exit_code: 8,
            runtime_ms: 10,
        });

        let diagnostics = finalize_terminated_child(
            fixture.pipeline.clone(),
            termination_event,
            Duration::from_secs(1),
        )
        .expect("disabled termination path returns an empty result");
        assert!(diagnostics.plugins.is_empty());
        assert!(diagnostics.report_path.is_none());

        let background = BackgroundFinalizeWorker::start(fixture.pipeline.clone());
        assert!(background.try_submit(captured(9)));
        background.shutdown(Duration::from_secs(1));

        assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
        assert!(fixture.entered_rx.try_recv().is_err());
        assert!(fixture.tempdir.path().read_dir().unwrap().next().is_none());
    }

    #[test]
    fn fatal_finalizer_double_spawn_failure_commits_emergency_evidence_without_plugins() {
        let fixture = worker_fixture();
        let mut event = captured(81);
        event.event.report_type = ReportType::Crash;
        event
            .data
            .raw
            .threads
            .push(crate::collectors::thread::RawThreadData {
                thread_port: 17,
                name: Some("crashed-thread".into()),
                crashed: true,
                ..Default::default()
            });
        event.raw_shm = Some(super::super::RawShmSnapshot {
            breadcrumbs: b"owned-breadcrumbs".to_vec(),
            context: b"owned-context".to_vec(),
        });

        let _reset = inject_fatal_spawn_failures(2);
        let finalization = CrashFinalization::start(fixture.pipeline.clone(), Box::new(event));
        assert!(matches!(&finalization, CrashFinalization::Deferred(_)));

        let diagnostics = finalization
            .complete(
                fixture.pipeline.clone(),
                Some(TerminationReason::Signaled {
                    signal: libc::SIGSEGV,
                    core_dumped: false,
                    runtime_ms: 42,
                }),
                Duration::from_secs(1),
            )
            .expect("emergency finalization returns diagnostics");

        assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
        assert!(fixture.entered_rx.try_recv().is_err());
        assert_eq!(
            diagnostics
                .plugins
                .iter()
                .filter(|entry| entry.name == "CrashFinalizer")
                .count(),
            2
        );

        let report_path = diagnostics
            .report_path
            .as_ref()
            .expect("best-effort JSON should be the emergency report path");
        assert!(report_path.is_file());
        let report_dir = report_path.parent().expect("report directory");
        assert_eq!(report_path.file_name().unwrap(), "report.json");
        assert!(report_dir.join("threads.txt").is_file());
        assert_eq!(
            std::fs::read(report_dir.join("breadcrumbs.bin")).unwrap(),
            b"owned-breadcrumbs"
        );
        assert_eq!(
            std::fs::read(report_dir.join("context.bin")).unwrap(),
            b"owned-context"
        );
        assert!(
            std::fs::read_to_string(report_dir.join("threads.txt"))
                .unwrap()
                .contains("crashed-thread")
        );

        let manifest = crate::pipeline::load_manifest(&report_dir.join("manifest.json")).unwrap();
        let artifact_names: Vec<_> = manifest
            .artifacts
            .iter()
            .map(|artifact| artifact.path.as_str())
            .collect();
        assert_eq!(
            artifact_names,
            vec![
                "breadcrumbs.bin",
                "context.bin",
                "report.json",
                "threads.txt"
            ]
        );
        let report = crate::pipeline::report::load_report(report_path).unwrap();
        assert_eq!(
            report.termination,
            Some(TerminationReason::Signaled {
                signal: libc::SIGSEGV,
                core_dumped: false,
                runtime_ms: 42,
            })
        );
        assert!(
            fixture
                .tempdir
                .path()
                .read_dir()
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with('.')),
            "the emergency transaction must be atomically published without a staging directory"
        );
    }

    #[test]
    fn background_queue_is_bounded_and_drops_when_full() {
        let fixture = worker_fixture();
        let worker = BackgroundFinalizeWorker::start(fixture.pipeline.clone());
        assert!(worker.try_submit(captured(1)));
        fixture
            .entered_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
        assert!(worker.try_submit(captured(2)));
        assert!(worker.try_submit(captured(3)));
        assert!(!worker.try_submit(captured(4)));

        release(&fixture);
        worker.shutdown(Duration::from_secs(2));
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn hung_background_worker_is_detached_after_bounded_drain() {
        let fixture = worker_fixture();
        let worker = BackgroundFinalizeWorker::start(fixture.pipeline.clone());
        assert!(worker.try_submit(captured(10)));
        fixture
            .entered_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap();

        let started = Instant::now();
        worker.shutdown(Duration::from_millis(50));
        assert!(started.elapsed() < Duration::from_millis(500));

        release(&fixture);
        fixture
            .completed_rx
            .recv_timeout(Duration::from_secs(1))
            .unwrap();
    }
}
