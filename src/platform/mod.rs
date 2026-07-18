#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(any(test, feature = "test-support"))]
pub(crate) mod mock;

mod task_control;

#[allow(unused_imports)] // public API is also compiled into the binary crate
pub use task_control::{
    RESUME_ATTEMPT_LIMIT, RetainedTaskPort, SupervisorHealth, SuspendFailurePolicy,
    TaskControlFailure, TaskControlFailureSink, TaskRecoveryAction, TaskSuspendGuard,
    contain_task_without_resume,
};

use mach2::port::mach_port_t;
use std::sync::Mutex;

use crate::pipeline::PluginContext;

/// Opaque platform task capability used by orchestration code. Only the
/// macOS adapter converts it to a Mach port name at the FFI boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TaskHandle(u32);

impl TaskHandle {
    #[must_use]
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    #[must_use]
    pub(crate) const fn into_mach_port(self) -> mach_port_t {
        self.0
    }
}

/// ABI-aligned ARM64 register state returned by `thread_get_state`.
///
/// The kernel flavor has a 68-word capacity. Consumers currently access
/// through word 66; word 67 is ABI padding and may be absent from variable
/// mock/fixture inputs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArmThreadState64([u32; 68]);

impl ArmThreadState64 {
    pub const WORD_CAPACITY: usize = 68;
    pub const MIN_CONSUMER_WORDS: usize = 67;

    /// Convert a variable word response into the dedicated ABI type.
    ///
    /// # Errors
    /// Returns an error when the response cannot cover every word read by the
    /// register consumer or exceeds the kernel flavor capacity.
    pub fn try_from_words(words: &[u32]) -> Result<Self, String> {
        if !(Self::MIN_CONSUMER_WORDS..=Self::WORD_CAPACITY).contains(&words.len()) {
            return Err(format!(
                "ARM_THREAD_STATE64 returned {} words; expected {}..={}",
                words.len(),
                Self::MIN_CONSUMER_WORDS,
                Self::WORD_CAPACITY
            ));
        }
        let mut state = [0; Self::WORD_CAPACITY];
        state[..words.len()].copy_from_slice(words);
        Ok(Self(state))
    }

    #[must_use]
    pub const fn words(&self) -> &[u32; Self::WORD_CAPACITY] {
        &self.0
    }
}

impl std::ops::Index<usize> for ArmThreadState64 {
    type Output = u32;

    fn index(&self, index: usize) -> &Self::Output {
        &self.0[index]
    }
}

/// Failure to satisfy an exact remote-memory read request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VmReadError {
    /// The platform rejected the read before returning usable bytes.
    Platform(String),
    /// The kernel returned fewer bytes than requested.
    Partial { requested: usize, bytes: Vec<u8> },
}

impl VmReadError {
    #[must_use]
    pub fn partial_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Partial { bytes, .. } => Some(bytes),
            Self::Platform(_) => None,
        }
    }
}

impl std::fmt::Display for VmReadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Platform(error) => formatter.write_str(error),
            Self::Partial { requested, bytes } => write!(
                formatter,
                "partial VM read: requested {requested} bytes, received {}",
                bytes.len()
            ),
        }
    }
}

impl std::error::Error for VmReadError {}

pub(crate) fn classify_vm_read(bytes: Vec<u8>, requested: usize) -> Result<Vec<u8>, VmReadError> {
    if bytes.len() == requested {
        Ok(bytes)
    } else {
        Err(VmReadError::Partial { requested, bytes })
    }
}

/// Abstraction over Mach kernel APIs used by collectors and pipeline.
/// Enables mock-based unit testing without real child processes.
pub trait PlatformOps: Send + Sync {
    /// Whether production capture must use the exec-based task collector
    /// helper. Test platforms default to the in-process deterministic worker.
    fn supports_capture_isolation(&self) -> bool {
        false
    }

    // ── Task control ──

    /// Suspend all threads in the target task.
    ///
    /// # Errors
    /// Returns an error string if the platform call fails.
    fn suspend_task(&self, task: mach_port_t) -> Result<(), String>;

    /// Resume all threads in the target task.
    ///
    /// # Errors
    /// Returns an error string if the platform call fails.
    fn resume_task(&self, task: mach_port_t) -> Result<(), String>;

    /// Terminate a task that could not be resumed after the bounded retry
    /// policy. This is a containment action: leaving a child permanently
    /// suspended is never considered a recoverable capture outcome.
    ///
    /// # Errors
    /// Returns an error string if the platform cannot terminate the task.
    fn terminate_task(&self, task: mach_port_t) -> Result<(), String>;

    /// Retain one independently owned user reference to a task send right.
    ///
    /// A worker that can outlive its caller must own this reference instead of
    /// borrowing a raw Mach name from the supervisor.
    ///
    /// # Errors
    /// Returns an error if the platform cannot retain the send-right reference.
    fn retain_task_port(&self, task: mach_port_t) -> Result<(), String>;

    /// Release one task send-right user reference retained by
    /// [`Self::retain_task_port`].
    fn deallocate_task_port(&self, task: mach_port_t);

    /// Record a task-control failure in supervisor-visible health state.
    ///
    fn record_task_control_failure(&self, failure: TaskControlFailure);

    /// Return the supervisor-visible task-control health snapshot.
    #[must_use]
    fn supervisor_health(&self) -> SupervisorHealth;

    // ── Thread inspection ──

    /// Get thread ports for all threads in the target task.
    ///
    /// # Errors
    /// Returns an error string if the platform call fails.
    fn get_task_threads(&self, task: mach_port_t) -> Result<Vec<mach_port_t>, String>;

    /// Get the name of a thread. Returns `Ok(None)` if the thread has no name.
    ///
    /// # Errors
    /// Returns an error string if the platform call fails.
    fn get_thread_name(&self, thread: mach_port_t) -> Result<Option<String>, String>;

    /// Return the system-wide stable `THREAD_IDENTIFIER_INFO.thread_id`.
    ///
    /// # Errors
    /// Returns an error string if the platform cannot query a stable ID.
    fn get_thread_identifier(&self, thread: mach_port_t) -> Result<u64, String>;

    /// Returns ABI-aligned ARM64 register state.
    ///
    /// # Errors
    /// Returns an error string if the platform call fails.
    fn get_thread_state(&self, thread: mach_port_t) -> Result<ArmThreadState64, String>;

    /// Deallocate a thread port's send right.
    fn deallocate_thread_port(&self, thread: mach_port_t);

    // ── Memory reading ──

    /// Read bytes from a remote task's address space.
    ///
    /// # Errors
    /// Returns an error string if the platform call fails.
    fn vm_read(&self, task: mach_port_t, address: u64, size: usize)
    -> Result<Vec<u8>, VmReadError>;

    /// Query VM region info at or after `address`.
    ///
    /// # Errors
    /// Returns an error string if the platform call fails.
    fn vm_region_query(&self, task: mach_port_t, address: u64) -> Result<VmRegionInfo, String>;

    /// Enumerate VM regions within the bounded capture context.
    ///
    /// A budget or deadline stop returns the safely collected prefix together
    /// with a non-complete [`VmRegionEnumerationQuality`].
    ///
    /// # Errors
    /// Returns an error if enumeration cannot be initialized.
    fn enumerate_vm_regions(
        &self,
        task: mach_port_t,
        context: &PluginContext,
    ) -> Result<(Vec<VmRegionInfo>, VmRegionEnumerationQuality), String>;

    /// Get high-level VM statistics for a task.
    ///
    /// # Errors
    /// Returns an error string if the platform call fails.
    fn get_task_vm_info(&self, task: mach_port_t) -> Result<TaskVmSummary, String>;

    /// Read `task_info` words for a given flavor into an aligned buffer.
    /// Returns the word count actually written by the kernel.
    ///
    /// # Errors
    /// Returns an error string if the platform call fails or the buffer is too large.
    fn get_task_info_words(
        &self,
        task: mach_port_t,
        flavor: u32,
        words: &mut [u32],
    ) -> Result<usize, String>;
}

/// Real macOS implementation — delegates to the free functions in `macos::*`.
#[cfg(target_os = "macos")]
#[derive(Default)]
pub struct MacOsPlatform {
    supervisor_health: Mutex<SupervisorHealth>,
}

#[cfg(target_os = "macos")]
impl PlatformOps for MacOsPlatform {
    fn supports_capture_isolation(&self) -> bool {
        true
    }

    fn suspend_task(&self, task: mach_port_t) -> Result<(), String> {
        macos::suspend_task(task).map_err(|e| e.to_string())
    }

    fn resume_task(&self, task: mach_port_t) -> Result<(), String> {
        macos::resume_task(task).map_err(|e| e.to_string())
    }

    fn terminate_task(&self, task: mach_port_t) -> Result<(), String> {
        macos::terminate_task(task).map_err(|e| e.to_string())
    }

    fn retain_task_port(&self, task: mach_port_t) -> Result<(), String> {
        macos::retain_task_port(task).map_err(|e| e.to_string())
    }

    fn deallocate_task_port(&self, task: mach_port_t) {
        macos::deallocate_task_port(task);
    }

    fn record_task_control_failure(&self, failure: TaskControlFailure) {
        match self.supervisor_health.lock() {
            Ok(mut health) => health.record(failure),
            Err(poisoned) => poisoned.into_inner().record(failure),
        }
    }

    fn supervisor_health(&self) -> SupervisorHealth {
        match self.supervisor_health.lock() {
            Ok(health) => health.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    fn get_task_threads(&self, task: mach_port_t) -> Result<Vec<mach_port_t>, String> {
        macos::get_task_threads(task).map_err(|e| e.to_string())
    }

    fn get_thread_name(&self, thread: mach_port_t) -> Result<Option<String>, String> {
        macos::get_thread_name(thread).map_err(|e| e.to_string())
    }

    fn get_thread_identifier(&self, thread: mach_port_t) -> Result<u64, String> {
        macos::get_thread_identifier(thread).map_err(|e| e.to_string())
    }

    fn get_thread_state(&self, thread: mach_port_t) -> Result<ArmThreadState64, String> {
        macos::get_thread_state(thread).map_err(|e| e.to_string())
    }

    fn deallocate_thread_port(&self, thread: mach_port_t) {
        macos::deallocate_thread_port(thread);
    }

    fn vm_read(
        &self,
        task: mach_port_t,
        address: u64,
        size: usize,
    ) -> Result<Vec<u8>, VmReadError> {
        macos::vm_read(task, address, size)
    }

    fn vm_region_query(&self, task: mach_port_t, address: u64) -> Result<VmRegionInfo, String> {
        let mut addr = address;
        macos::vm_region_query(task, &mut addr).map_err(|e| e.to_string())
    }

    fn enumerate_vm_regions(
        &self,
        task: mach_port_t,
        context: &PluginContext,
    ) -> Result<(Vec<VmRegionInfo>, VmRegionEnumerationQuality), String> {
        macos::enumerate_vm_regions(task, || context.checkpoint())
    }

    fn get_task_vm_info(&self, task: mach_port_t) -> Result<TaskVmSummary, String> {
        macos::get_task_vm_info(task).map_err(|e| e.to_string())
    }

    fn get_task_info_words(
        &self,
        task: mach_port_t,
        flavor: u32,
        words: &mut [u32],
    ) -> Result<usize, String> {
        macos::get_task_info_words(task, flavor, words).map_err(|e| e.to_string())
    }
}
