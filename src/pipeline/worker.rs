//! Bounded worker boundary between live-task capture and report finalization.

use mach2::port::mach_port_t;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::{
    CaptureOutcome, CapturePayload, CapturedEvent, Diagnostics, Pipeline, PluginStatus,
    TerminationReason,
};

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
    task: mach_port_t,
    cancelled: Arc<AtomicBool>,
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
                while let Ok(job) = receiver.recv() {
                    let payload = worker_pipeline.collect_snapshot_for_worker(
                        &job.event,
                        job.task,
                        &job.cancelled,
                    );
                    let _ = job.result_tx.send(payload);
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
    pub(crate) fn capture(
        &mut self,
        event: super::CrashEvent,
        task: mach_port_t,
        deadline: Instant,
    ) -> CaptureOutcome {
        if Instant::now() >= deadline {
            return timed_out_capture(event, "absolute capture deadline already elapsed");
        }

        if let Some(reason) = &self.unavailable_reason {
            return failed_capture(event, reason);
        }

        let suspended = match self.pipeline.platform.suspend_task(task) {
            Ok(()) => true,
            Err(error) if event.bail_on_suspend_failure => {
                eprintln!("[monitor] {error}");
                let mut diagnostics = Diagnostics::new();
                diagnostics.record_immediate("CaptureSuspend", PluginStatus::Error(error.clone()));
                return CaptureOutcome::Skipped(diagnostics);
            }
            Err(error) => {
                eprintln!("[monitor] suspend_task failed (proceeding with best-effort): {error}");
                false
            }
        };

        if Instant::now() >= deadline {
            self.resume_if_needed(task, suspended);
            return timed_out_capture(event, "absolute capture deadline elapsed during suspend");
        }

        let cancelled = Arc::new(AtomicBool::new(false));
        let (result_tx, result_rx) = sync_channel(1);
        let event_for_result = event.clone();
        let job = CaptureJob {
            event,
            task,
            cancelled: cancelled.clone(),
            result_tx,
        };

        let Some(sender) = &self.sender else {
            self.resume_if_needed(task, suspended);
            return failed_capture(job.event, "capture worker is unavailable");
        };

        if let Err(error) = sender.try_send(job) {
            let event = match error {
                TrySendError::Full(job) | TrySendError::Disconnected(job) => job.event,
            };
            self.resume_if_needed(task, suspended);
            self.retire("capture worker queue unavailable");
            return failed_capture(event, "capture worker queue unavailable");
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        let result = result_rx.recv_timeout(remaining);
        if result.is_err() {
            // Publish cancellation before resume so a collector that returns
            // at the deadline cannot start another task-facing collector.
            cancelled.store(true, Ordering::Release);
        }
        self.resume_if_needed(task, suspended);

        match result {
            Ok(payload) => CaptureOutcome::Captured(CapturedEvent::new(event_for_result, payload)),
            Err(RecvTimeoutError::Timeout) => {
                self.retire("capture worker exceeded absolute deadline");
                timed_out_capture(
                    event_for_result,
                    "capture worker exceeded absolute deadline",
                )
            }
            Err(RecvTimeoutError::Disconnected) => {
                self.retire("capture worker disconnected");
                failed_capture(event_for_result, "capture worker disconnected")
            }
        }
    }

    fn resume_if_needed(&self, task: mach_port_t, suspended: bool) {
        if suspended && let Err(error) = self.pipeline.platform.resume_task(task) {
            eprintln!("[monitor] resume_task failed: {error}");
        }
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

fn timed_out_capture(event: super::CrashEvent, reason: &str) -> CaptureOutcome {
    minimal_capture(event, reason, PluginStatus::TimedOut)
}

fn failed_capture(event: super::CrashEvent, reason: &str) -> CaptureOutcome {
    minimal_capture(event, reason, PluginStatus::Error(reason.to_string()))
}

fn minimal_capture(event: super::CrashEvent, reason: &str, status: PluginStatus) -> CaptureOutcome {
    eprintln!("[monitor] {reason}; continuing with minimum capture payload");
    let mut diagnostics = Diagnostics::new();
    diagnostics.record_immediate("CaptureWorker", status);
    CaptureOutcome::Captured(CapturedEvent::new(
        event,
        CapturePayload {
            data: super::CollectedData::default(),
            raw_shm: None,
            diagnostics,
        },
    ))
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
    Deferred(CapturedEvent),
}

impl CrashFinalization {
    pub(crate) fn start(pipeline: Arc<Pipeline>, captured: CapturedEvent) -> Self {
        match CrashFinalizeTicket::spawn(pipeline, captured) {
            Ok(ticket) => Self::Pending(ticket),
            Err(captured) => Self::Deferred(*captured),
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
                match CrashFinalizeTicket::spawn(pipeline, captured) {
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
        CollectedData, CrashEvent, Plugin, PluginContext, PluginExecution, PostProcessor, Priority,
        ReportResult, ReportType,
    };
    use crate::platform::mock::MockPlatform;
    use std::sync::Condvar;
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    struct WorkerFixture {
        pipeline: Arc<Pipeline>,
        entered_rx: Receiver<()>,
        completed_rx: Receiver<()>,
        release: Arc<(Mutex<bool>, Condvar)>,
        calls: Arc<AtomicUsize>,
        _tempdir: tempfile::TempDir,
    }

    fn worker_fixture() -> WorkerFixture {
        let tempdir = tempfile::tempdir().unwrap();
        let (entered_tx, entered_rx) = sync_channel(8);
        let (completed_tx, completed_rx) = sync_channel(8);
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let calls = Arc::new(AtomicUsize::new(0));
        let pipeline = Arc::new(Pipeline {
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
            _tempdir: tempdir,
        }
    }

    fn captured(pid: u32) -> CapturedEvent {
        CapturedEvent::new(
            CrashEvent {
                report_type: ReportType::Snapshot,
                termination: None,
                exception_type: None,
                exception_code: None,
                exception_subcode: None,
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
