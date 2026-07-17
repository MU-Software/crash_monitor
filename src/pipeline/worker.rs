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
use crate::platform::{TaskControlFailureSink, TaskSuspendGuard};

/// One absolute capture budget, measured from Mach request receipt for crashes.
pub const CAPTURE_DEADLINE: Duration = Duration::from_secs(5);
/// Maximum best-effort drain when the child exits normally.
pub const BACKGROUND_DRAIN_DEADLINE: Duration = Duration::from_secs(2);
/// Covers the five-minute feedback UI plus a small completion margin.
pub const CRASH_FINALIZE_WAIT: Duration = Duration::from_secs(310);

const CAPTURE_QUEUE_CAPACITY: usize = 1;
const BACKGROUND_QUEUE_CAPACITY: usize = 2;

struct CaptureJob {
    event: super::CrashEvent,
    report_context: Arc<super::ReportContext>,
    task: mach_port_t,
    cancelled: Arc<AtomicBool>,
    shm_snapshot: Option<Arc<crate::shm::OwnedShmSnapshot>>,
    result_tx: SyncSender<CapturePayload>,
}

/// Single-consumer capture worker. Once a capture times out, the worker is
/// retired rather than accepting another task while the old collector may
/// still be running.
pub(crate) struct CaptureWorker {
    pipeline: Arc<Pipeline>,
    sender: Option<SyncSender<CaptureJob>>,
    done_rx: Receiver<()>,
    handle: Option<JoinHandle<()>>,
    unavailable_reason: Option<String>,
}

impl CaptureWorker {
    pub(crate) fn start(pipeline: Arc<Pipeline>) -> Self {
        let (sender, receiver) = sync_channel::<CaptureJob>(CAPTURE_QUEUE_CAPACITY);
        let (done_tx, done_rx) = mpsc::channel();
        let worker_pipeline = pipeline.clone();
        let spawn = thread::Builder::new()
            .name("crash-capture".into())
            .spawn(move || {
                while let Ok(CaptureJob {
                    event,
                    report_context,
                    task,
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
                sender: Some(sender),
                done_rx,
                handle: Some(handle),
                unavailable_reason: None,
            },
            Err(error) => Self {
                pipeline,
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
            return timed_out_capture(
                event,
                report_context,
                "absolute capture deadline already elapsed",
            );
        }

        if let Some(reason) = &self.unavailable_reason {
            return failed_capture(event, report_context, reason);
        }

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

        if Instant::now() >= deadline {
            finish_suspend(&mut suspend_guard);
            return with_task_control_diagnostics(
                timed_out_capture(
                    event,
                    report_context,
                    "absolute capture deadline elapsed during suspend",
                ),
                suspend_error,
                None,
                &failure_sink,
            );
        }

        // The guard-owning event-loop thread snapshots the payload before the
        // capture job can run. A timed-out worker may continue parsing owned
        // bytes, but it can never reach back into the resumed task's mapping.
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
            return with_task_control_diagnostics(
                timed_out_capture_with_snapshot(
                    event,
                    report_context,
                    "absolute capture deadline elapsed during shm snapshot",
                    shm_snapshot.as_deref(),
                ),
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
            task,
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
                )
            }
            Err(RecvTimeoutError::Disconnected) => {
                self.retire("capture worker disconnected");
                failed_capture_with_snapshot(
                    event_for_result,
                    report_context_for_result,
                    "capture worker disconnected",
                    fallback_shm_snapshot.as_deref(),
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
) -> CaptureOutcome {
    minimal_capture(
        event,
        report_context,
        reason,
        PluginStatus::TimedOut,
        raw_shm_from_snapshot(snapshot),
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
) -> CaptureOutcome {
    minimal_capture(
        event,
        report_context,
        reason,
        PluginStatus::Error(reason.to_string()),
        raw_shm_from_snapshot(snapshot),
    )
}

fn raw_shm_from_snapshot(
    snapshot: Option<&crate::shm::OwnedShmSnapshot>,
) -> Option<super::RawShmSnapshot> {
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
    pub(crate) fn start(pipeline: Arc<Pipeline>, captured: CapturedEvent) -> Self {
        match CrashFinalizeTicket::spawn(pipeline, captured) {
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
                match CrashFinalizeTicket::spawn(pipeline, *captured) {
                    Ok(ticket) => ticket.complete(reason, timeout),
                    Err(captured) => {
                        let mut captured = *captured;
                        captured.diagnostics.record_immediate(
                            "CrashFinalizer",
                            PluginStatus::Error("worker spawn retry failed".into()),
                        );
                        Some(captured.diagnostics)
                    }
                }
            }
        }
    }
}

pub struct CrashFinalizeTicket {
    termination_tx: SyncSender<Option<TerminationReason>>,
    result_rx: Receiver<Diagnostics>,
    done_rx: Receiver<()>,
    handle: Option<JoinHandle<()>>,
}

impl CrashFinalizeTicket {
    fn spawn(pipeline: Arc<Pipeline>, captured: CapturedEvent) -> Result<Self, Box<CapturedEvent>> {
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
    use crate::platform::mock::MockPlatform;
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

    fn worker_fixture_with_enabled(enabled: bool) -> WorkerFixture {
        let tempdir = tempfile::tempdir().unwrap();
        let (entered_tx, entered_rx) = sync_channel(8);
        let (completed_tx, completed_rx) = sync_channel(8);
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let calls = Arc::new(AtomicUsize::new(0));
        let pipeline = Arc::new(Pipeline {
            enabled,
            triggers: TriggerPolicy::ALL_ENABLED,
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
                report_id: Default::default(),
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
        let mut worker = CaptureWorker::start(fixture.pipeline.clone());
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
            filters: vec![],
            collectors: vec![],
            pre_processors: vec![],
            post_processors: vec![],
            notifiers: vec![],
            shm: None,
            platform: platform.clone(),
            output_dir: Some(tempdir.path().to_path_buf()),
        });
        let mut worker = CaptureWorker::start(pipeline);

        let outcome = worker.capture(
            captured(9).event,
            123,
            Instant::now() + Duration::from_secs(1),
        );

        assert!(matches!(outcome, CaptureOutcome::Skipped(_)));
        assert_eq!(platform.suspend_count(), 0);
        assert_eq!(platform.resume_count(), 0);
        assert!(std::fs::read_dir(tempdir.path()).unwrap().next().is_none());
        worker.shutdown(Duration::from_secs(1));
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
            filters: vec![],
            collectors: vec![],
            pre_processors: vec![],
            post_processors: vec![],
            notifiers: vec![],
            shm: Some(shm),
            platform: platform.clone(),
            output_dir: Some(tempdir.path().to_path_buf()),
        });
        let mut worker = CaptureWorker::start(pipeline);

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

        let mut worker = CaptureWorker::start(pipeline);
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
            filters: vec![],
            collectors: vec![],
            pre_processors: vec![],
            post_processors: vec![],
            notifiers: vec![],
            shm: Some(shm),
            platform: platform.clone(),
            output_dir: Some(tempdir.path().to_path_buf()),
        });
        let mut worker = CaptureWorker::start(pipeline);
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
