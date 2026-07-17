//! Owned task suspension and bounded recovery policy.

use mach2::port::mach_port_t;
use std::fmt;
use std::sync::{Arc, Mutex};

use super::PlatformOps;
use crate::pipeline::{Diagnostics, PluginStatus};

/// Total `resume_task` calls, including the initial attempt.
pub const RESUME_ATTEMPT_LIMIT: usize = 3;

/// Capture policy when the monitor cannot acquire a suspension count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuspendFailurePolicy {
    /// Do not inspect a running task when the trigger requires a stable view.
    SkipCapture,
    /// Preserve fatal evidence using best-effort reads without pretending the
    /// monitor owns a suspension count.
    BestEffort,
}

/// Terminal containment action following one or more resume failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskRecoveryAction {
    /// A later bounded retry resumed the task.
    Resumed,
    /// Every resume attempt failed and the target was terminated.
    Terminated,
    /// Resume and termination both failed; the outer supervisor must escalate.
    EscalationRequired,
}

/// Structured evidence for a task-control failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskControlFailure {
    pub task: mach_port_t,
    /// Total `resume_task` calls, including the call that eventually succeeded.
    pub resume_attempts: usize,
    pub resume_errors: Vec<String>,
    pub recovery: TaskRecoveryAction,
    pub termination_error: Option<String>,
}

impl TaskControlFailure {
    #[must_use]
    pub fn requires_escalation(&self) -> bool {
        self.recovery == TaskRecoveryAction::EscalationRequired
    }

    /// Whether capture must stop because normal execution could not resume.
    #[must_use]
    pub fn prevents_continued_monitoring(&self) -> bool {
        matches!(
            self.recovery,
            TaskRecoveryAction::Terminated | TaskRecoveryAction::EscalationRequired
        )
    }
}

impl fmt::Display for TaskControlFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "task {} resume attempted {} time(s); recovery={:?}",
            self.task, self.resume_attempts, self.recovery
        )?;
        if let Some(error) = self.resume_errors.last() {
            write!(f, "; last resume error: {error}")?;
        }
        if let Some(error) = &self.termination_error {
            write!(f, "; termination failed: {error}")?;
        }
        Ok(())
    }
}

/// Supervisor-visible health state. Failures remain available after the
/// per-capture diagnostics sink has been drained.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SupervisorHealth {
    pub task_control_failures: Vec<TaskControlFailure>,
}

impl SupervisorHealth {
    pub fn record(&mut self, failure: TaskControlFailure) {
        self.task_control_failures.push(failure);
    }

    #[must_use]
    pub fn requires_escalation(&self) -> bool {
        self.task_control_failures
            .iter()
            .any(TaskControlFailure::requires_escalation)
    }
}

/// Shared sink owned by both the guard and its capture caller. This makes a
/// resume failure observable even when `Drop` performs the release.
#[derive(Clone, Default)]
pub struct TaskControlFailureSink {
    failures: Arc<Mutex<Vec<TaskControlFailure>>>,
}

impl TaskControlFailureSink {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn record(&self, failure: TaskControlFailure) {
        match self.failures.lock() {
            Ok(mut failures) => failures.push(failure),
            Err(poisoned) => poisoned.into_inner().push(failure),
        }
    }

    /// Drain failures into the event diagnostics exactly once.
    pub fn drain_into(&self, diagnostics: &mut Diagnostics) {
        let failures = match self.failures.lock() {
            Ok(mut failures) => std::mem::take(&mut *failures),
            Err(poisoned) => std::mem::take(&mut *poisoned.into_inner()),
        };
        for failure in failures {
            diagnostics.record_immediate("TaskResume", PluginStatus::Error(failure.to_string()));
        }
    }
}

/// Owns exactly one successful `suspend_task` operation.
///
/// Construction is impossible after a failed suspend. `finish` and `Drop`
/// share the `active` bit, so only one of them can run the release policy.
pub struct TaskSuspendGuard {
    platform: Arc<dyn PlatformOps>,
    task: mach_port_t,
    failure_sink: TaskControlFailureSink,
    active: bool,
}

impl TaskSuspendGuard {
    /// Acquire one suspension count and return its unique owner.
    ///
    /// # Errors
    /// Returns the platform suspend error without constructing a guard.
    pub fn acquire(
        platform: Arc<dyn PlatformOps>,
        task: mach_port_t,
        failure_sink: TaskControlFailureSink,
    ) -> Result<Self, String> {
        platform.suspend_task(task)?;
        Ok(Self {
            platform,
            task,
            failure_sink,
            active: true,
        })
    }

    /// Release the owned suspension count before leaving the task-facing
    /// capture boundary. Dropping the consumed guard is then a no-op.
    pub fn finish(mut self) {
        self.release();
    }

    fn release(&mut self) {
        if !self.active {
            return;
        }
        // Terminal ownership transition happens before platform calls. Even
        // if a platform implementation panics, Drop cannot issue a duplicate
        // resume while unwinding.
        self.active = false;

        let mut resume_errors = Vec::with_capacity(RESUME_ATTEMPT_LIMIT);
        let mut resume_attempts = 0;
        for _ in 0..RESUME_ATTEMPT_LIMIT {
            resume_attempts += 1;
            match self.platform.resume_task(self.task) {
                Ok(()) => {
                    if !resume_errors.is_empty() {
                        self.record_failure(
                            resume_attempts,
                            resume_errors,
                            TaskRecoveryAction::Resumed,
                            None,
                        );
                    }
                    return;
                }
                Err(error) => resume_errors.push(error),
            }
        }

        match self.platform.terminate_task(self.task) {
            Ok(()) => self.record_failure(
                resume_attempts,
                resume_errors,
                TaskRecoveryAction::Terminated,
                None,
            ),
            Err(error) => self.record_failure(
                resume_attempts,
                resume_errors,
                TaskRecoveryAction::EscalationRequired,
                Some(error),
            ),
        }
    }

    fn record_failure(
        &self,
        resume_attempts: usize,
        resume_errors: Vec<String>,
        recovery: TaskRecoveryAction,
        termination_error: Option<String>,
    ) {
        let failure = TaskControlFailure {
            task: self.task,
            resume_attempts,
            resume_errors,
            recovery,
            termination_error,
        };
        eprintln!("[monitor] {failure}");
        self.failure_sink.record(failure.clone());
        self.platform.record_task_control_failure(failure);
    }
}

impl Drop for TaskSuspendGuard {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
mod tests {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use super::*;
    use crate::platform::mock::MockPlatform;

    fn acquire(platform: Arc<MockPlatform>, sink: TaskControlFailureSink) -> TaskSuspendGuard {
        let platform: Arc<dyn PlatformOps> = platform;
        TaskSuspendGuard::acquire(platform, 42, sink).expect("suspend succeeds")
    }

    #[test]
    fn explicit_finish_resumes_exactly_once() {
        let platform = Arc::new(MockPlatform::default());
        let guard = acquire(platform.clone(), TaskControlFailureSink::new());

        guard.finish();

        assert_eq!(platform.suspend_count(), 1);
        assert_eq!(platform.resume_count(), 1);
        assert_eq!(platform.terminate_count(), 0);
    }

    #[test]
    fn early_return_drop_resumes_exactly_once() {
        fn return_while_suspended(platform: Arc<MockPlatform>) {
            let _guard = acquire(platform, TaskControlFailureSink::new());
        }

        let platform = Arc::new(MockPlatform::default());
        return_while_suspended(platform.clone());

        assert_eq!(platform.suspend_count(), 1);
        assert_eq!(platform.resume_count(), 1);
    }

    #[test]
    fn panic_unwind_drop_resumes_exactly_once() {
        let platform = Arc::new(MockPlatform::default());
        let result = catch_unwind(AssertUnwindSafe({
            let platform = platform.clone();
            move || {
                let _guard = acquire(platform, TaskControlFailureSink::new());
                panic!("capture panic");
            }
        }));

        assert!(result.is_err());
        assert_eq!(platform.suspend_count(), 1);
        assert_eq!(platform.resume_count(), 1);
    }

    #[test]
    fn failed_suspend_never_constructs_owner_or_resumes() {
        let mut platform = MockPlatform::default();
        platform.suspend_fails = true;
        let platform = Arc::new(platform);
        let dyn_platform: Arc<dyn PlatformOps> = platform.clone();

        let result = TaskSuspendGuard::acquire(dyn_platform, 42, TaskControlFailureSink::new());

        assert!(result.is_err());
        assert_eq!(platform.suspend_count(), 1);
        assert_eq!(platform.resume_count(), 0);
        assert_eq!(platform.terminate_count(), 0);
    }

    #[test]
    fn drop_failure_is_drained_to_diagnostics_and_supervisor_health() {
        let mut platform = MockPlatform::default();
        platform.resume_fails = true;
        let platform = Arc::new(platform);
        let sink = TaskControlFailureSink::new();

        drop(acquire(platform.clone(), sink.clone()));
        let mut diagnostics = Diagnostics::new();
        sink.drain_into(&mut diagnostics);

        assert_eq!(platform.resume_count(), RESUME_ATTEMPT_LIMIT);
        assert_eq!(platform.terminate_count(), 1);
        assert!(diagnostics.plugins.iter().any(|entry| {
            entry.name == "TaskResume" && matches!(entry.status, PluginStatus::Error(_))
        }));
        let health = platform.supervisor_health();
        assert_eq!(health.task_control_failures.len(), 1);
        assert_eq!(
            health.task_control_failures[0].recovery,
            TaskRecoveryAction::Terminated
        );
        assert!(!health.requires_escalation());
    }

    #[test]
    fn terminate_failure_requires_supervisor_escalation() {
        let mut platform = MockPlatform::default();
        platform.resume_fails = true;
        platform.terminate_fails = true;
        let platform = Arc::new(platform);

        acquire(platform.clone(), TaskControlFailureSink::new()).finish();

        assert_eq!(platform.resume_count(), RESUME_ATTEMPT_LIMIT);
        assert_eq!(platform.terminate_count(), 1);
        let health = platform.supervisor_health();
        assert!(health.requires_escalation());
        assert_eq!(
            health.task_control_failures[0].recovery,
            TaskRecoveryAction::EscalationRequired
        );
    }

    #[test]
    fn retry_stops_at_first_success_and_never_terminates() {
        let platform = Arc::new(MockPlatform::default());
        platform.fail_next_resume_attempts(1);
        let sink = TaskControlFailureSink::new();

        acquire(platform.clone(), sink.clone()).finish();
        let mut diagnostics = Diagnostics::new();
        sink.drain_into(&mut diagnostics);

        assert_eq!(platform.resume_count(), 2);
        assert_eq!(platform.terminate_count(), 0);
        assert_eq!(diagnostics.plugins.len(), 1);
        let health = platform.supervisor_health();
        assert_eq!(health.task_control_failures.len(), 1);
        assert_eq!(health.task_control_failures[0].resume_attempts, 2);
        assert_eq!(
            health.task_control_failures[0].recovery,
            TaskRecoveryAction::Resumed
        );
    }
}
