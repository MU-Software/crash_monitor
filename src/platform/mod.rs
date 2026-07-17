#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "macos")]
pub use macos::*;

#[cfg(any(test, feature = "test-support"))]
pub mod mock;

mod task_control;

#[allow(unused_imports)] // public API is also compiled into the binary crate
pub use task_control::{
    RESUME_ATTEMPT_LIMIT, SupervisorHealth, SuspendFailurePolicy, TaskControlFailure,
    TaskControlFailureSink, TaskRecoveryAction, TaskSuspendGuard,
};

use mach2::port::mach_port_t;
use std::sync::Mutex;

use crate::pipeline::PluginContext;

/// Abstraction over Mach kernel APIs used by collectors and pipeline.
/// Enables mock-based unit testing without real child processes.
pub trait PlatformOps: Send + Sync {
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

    /// Returns ARM64 register state as `[u32; 68]`.
    ///
    /// # Errors
    /// Returns an error string if the platform call fails.
    fn get_thread_state(&self, thread: mach_port_t) -> Result<Vec<u32>, String>;

    /// Deallocate a thread port's send right.
    fn deallocate_thread_port(&self, thread: mach_port_t);

    // ── Memory reading ──

    /// Read bytes from a remote task's address space.
    ///
    /// # Errors
    /// Returns an error string if the platform call fails.
    fn vm_read(&self, task: mach_port_t, address: u64, size: usize) -> Result<Vec<u8>, String>;

    /// Query VM region info at or after `address`.
    ///
    /// # Errors
    /// Returns an error string if the platform call fails.
    fn vm_region_query(&self, task: mach_port_t, address: u64) -> Result<VmRegionInfo, String>;

    /// Enumerate all VM regions in the target task's address space.
    ///
    /// # Errors
    /// Returns an error when the plugin deadline or cancellation token fires.
    fn enumerate_vm_regions(
        &self,
        task: mach_port_t,
        context: &PluginContext,
    ) -> Result<(Vec<VmRegionInfo>, bool), String>;

    /// Get high-level VM statistics for a task.
    ///
    /// # Errors
    /// Returns an error string if the platform call fails.
    fn get_task_vm_info(&self, task: mach_port_t) -> Result<TaskVmSummary, String>;

    /// Read `task_info` for a given flavor into a byte buffer.
    /// The caller is responsible for interpreting the bytes as the correct type.
    ///
    /// # Errors
    /// Returns an error string if the platform call fails or the buffer is too large.
    fn get_task_info_bytes(
        &self,
        task: mach_port_t,
        flavor: u32,
        buf: &mut [u8],
    ) -> Result<(), String>;
}

/// Real macOS implementation — delegates to the free functions in `macos::*`.
#[cfg(target_os = "macos")]
#[derive(Default)]
pub struct MacOsPlatform {
    supervisor_health: Mutex<SupervisorHealth>,
}

#[cfg(target_os = "macos")]
impl PlatformOps for MacOsPlatform {
    fn suspend_task(&self, task: mach_port_t) -> Result<(), String> {
        macos::suspend_task(task).map_err(|e| e.to_string())
    }

    fn resume_task(&self, task: mach_port_t) -> Result<(), String> {
        macos::resume_task(task).map_err(|e| e.to_string())
    }

    fn terminate_task(&self, task: mach_port_t) -> Result<(), String> {
        macos::terminate_task(task).map_err(|e| e.to_string())
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

    fn get_thread_state(&self, thread: mach_port_t) -> Result<Vec<u32>, String> {
        macos::get_thread_state(thread)
            .map(|arr| arr.to_vec())
            .map_err(|e| e.to_string())
    }

    fn deallocate_thread_port(&self, thread: mach_port_t) {
        macos::deallocate_thread_port(thread);
    }

    fn vm_read(&self, task: mach_port_t, address: u64, size: usize) -> Result<Vec<u8>, String> {
        macos::vm_read(task, address, size).map_err(|e| e.to_string())
    }

    fn vm_region_query(&self, task: mach_port_t, address: u64) -> Result<VmRegionInfo, String> {
        let mut addr = address;
        macos::vm_region_query(task, &mut addr).map_err(|e| e.to_string())
    }

    fn enumerate_vm_regions(
        &self,
        task: mach_port_t,
        context: &PluginContext,
    ) -> Result<(Vec<VmRegionInfo>, bool), String> {
        macos::enumerate_vm_regions(task, || context.checkpoint())
    }

    fn get_task_vm_info(&self, task: mach_port_t) -> Result<TaskVmSummary, String> {
        macos::get_task_vm_info(task).map_err(|e| e.to_string())
    }

    fn get_task_info_bytes(
        &self,
        task: mach_port_t,
        flavor: u32,
        buf: &mut [u8],
    ) -> Result<(), String> {
        // SAFETY: We reinterpret the byte buffer as the appropriate struct type.
        // The caller ensures the buffer is correctly sized.
        // We use get_task_info internally which handles alignment via kernel.
        #[repr(C)]
        struct RawBuf([u8; 256]);

        if buf.len() > 256 {
            return Err("task_info buffer too large".into());
        }

        let mut raw = RawBuf([0u8; 256]);
        raw.0[..buf.len()].copy_from_slice(buf);
        macos::get_task_info(task, flavor, &mut raw).map_err(|e| e.to_string())?;
        buf.copy_from_slice(&raw.0[..buf.len()]);
        Ok(())
    }
}
