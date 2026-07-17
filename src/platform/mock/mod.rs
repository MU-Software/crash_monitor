//! Mock platform for unit testing collectors and pipeline without real Mach APIs.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use mach2::port::mach_port_t;

use crate::pipeline::PluginContext;

use super::{
    PlatformOps, SupervisorHealth, TaskControlFailure, TaskVmSummary, VmRegionEnumerationQuality,
    VmRegionInfo,
};

/// Mock thread data: port, optional name, register state [u32; 68].
// Cargo's self dev-dependency enables `test-support` for the binary during
// all-target linting even though production code never constructs mocks.
#[allow(dead_code)]
pub struct MockThread {
    pub port: mach_port_t,
    pub name: Option<String>,
    pub state: Vec<u32>,
}

/// Configurable mock platform. Tests set fields before passing to collectors.
///
/// Tracks invocations of key methods (suspend, resume, deallocate) so tests
/// can verify the pipeline called them correctly.
#[allow(dead_code)]
pub struct MockPlatform {
    pub threads: Vec<MockThread>,
    /// Memory map: address → bytes. `vm_read` returns matching range.
    pub memory: BTreeMap<u64, Vec<u8>>,
    pub regions: Vec<VmRegionInfo>,
    pub vm_region_quality: VmRegionEnumerationQuality,
    pub vm_info: Option<TaskVmSummary>,
    /// Task info responses: flavor → byte buffer.
    pub task_info_responses: BTreeMap<u32, Vec<u8>>,
    /// If true, `suspend_task` returns Err.
    pub suspend_fails: bool,
    /// If true, `resume_task` returns Err.
    pub resume_fails: bool,
    /// If true, `terminate_task` returns Err.
    pub terminate_fails: bool,

    // ── Invocation tracking ──
    suspend_count: AtomicUsize,
    resume_count: AtomicUsize,
    resume_failures_remaining: AtomicUsize,
    terminate_count: AtomicUsize,
    supervisor_health: Mutex<SupervisorHealth>,
    deallocated_ports: Mutex<Vec<mach_port_t>>,
}

#[allow(dead_code)]
impl MockPlatform {
    /// Number of times `suspend_task` was called.
    pub fn suspend_count(&self) -> usize {
        self.suspend_count.load(Ordering::SeqCst)
    }

    /// Number of times `resume_task` was called.
    pub fn resume_count(&self) -> usize {
        self.resume_count.load(Ordering::SeqCst)
    }

    /// Number of times `terminate_task` was called.
    pub fn terminate_count(&self) -> usize {
        self.terminate_count.load(Ordering::SeqCst)
    }

    /// Fail exactly the next `attempts` resume calls, then allow success.
    pub fn fail_next_resume_attempts(&self, attempts: usize) {
        self.resume_failures_remaining
            .store(attempts, Ordering::SeqCst);
    }

    /// Thread ports that were passed to `deallocate_thread_port`.
    ///
    /// # Panics
    /// Panics if the internal mutex is poisoned.
    pub fn deallocated_ports(&self) -> Vec<mach_port_t> {
        self.deallocated_ports.lock().unwrap().clone()
    }
}

impl Default for MockPlatform {
    fn default() -> Self {
        Self {
            threads: Vec::new(),
            memory: BTreeMap::new(),
            regions: Vec::new(),
            vm_region_quality: VmRegionEnumerationQuality::Complete,
            vm_info: None,
            task_info_responses: BTreeMap::new(),
            suspend_fails: false,
            resume_fails: false,
            terminate_fails: false,
            suspend_count: AtomicUsize::new(0),
            resume_count: AtomicUsize::new(0),
            resume_failures_remaining: AtomicUsize::new(0),
            terminate_count: AtomicUsize::new(0),
            supervisor_health: Mutex::new(SupervisorHealth::default()),
            deallocated_ports: Mutex::new(Vec::new()),
        }
    }
}

impl PlatformOps for MockPlatform {
    fn suspend_task(&self, _task: mach_port_t) -> Result<(), String> {
        self.suspend_count.fetch_add(1, Ordering::SeqCst);
        if self.suspend_fails {
            Err("mock: suspend_task failed".into())
        } else {
            Ok(())
        }
    }

    fn resume_task(&self, _task: mach_port_t) -> Result<(), String> {
        self.resume_count.fetch_add(1, Ordering::SeqCst);
        let configured_failure = self
            .resume_failures_remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok();
        if self.resume_fails || configured_failure {
            Err("mock: resume_task failed".into())
        } else {
            Ok(())
        }
    }

    fn terminate_task(&self, _task: mach_port_t) -> Result<(), String> {
        self.terminate_count.fetch_add(1, Ordering::SeqCst);
        if self.terminate_fails {
            Err("mock: terminate_task failed".into())
        } else {
            Ok(())
        }
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

    fn get_task_threads(&self, _task: mach_port_t) -> Result<Vec<mach_port_t>, String> {
        Ok(self.threads.iter().map(|t| t.port).collect())
    }

    fn get_thread_name(&self, thread: mach_port_t) -> Result<Option<String>, String> {
        Ok(self
            .threads
            .iter()
            .find(|t| t.port == thread)
            .and_then(|t| t.name.clone()))
    }

    fn get_thread_state(&self, thread: mach_port_t) -> Result<Vec<u32>, String> {
        self.threads
            .iter()
            .find(|t| t.port == thread)
            .map(|t| t.state.clone())
            .ok_or_else(|| format!("mock: thread {thread} not found"))
    }

    fn deallocate_thread_port(&self, thread: mach_port_t) {
        self.deallocated_ports.lock().unwrap().push(thread);
    }

    fn vm_read(&self, _task: mach_port_t, address: u64, size: usize) -> Result<Vec<u8>, String> {
        // Find the memory region that contains this address
        for (&base, data) in &self.memory {
            let Ok(diff) = usize::try_from(address.wrapping_sub(base)) else {
                continue;
            };
            if address >= base && diff + size <= data.len() {
                let offset = diff;
                return Ok(data[offset..offset + size].to_vec());
            }
        }
        Err(format!(
            "mock: vm_read at {address:#x} size {size} not found"
        ))
    }

    fn vm_region_query(&self, _task: mach_port_t, address: u64) -> Result<VmRegionInfo, String> {
        // Find first region at or after address
        self.regions
            .iter()
            .find(|r| r.address + r.size > address)
            .cloned()
            .ok_or_else(|| format!("mock: no region at {address:#x}"))
    }

    fn enumerate_vm_regions(
        &self,
        _task: mach_port_t,
        context: &PluginContext,
    ) -> Result<(Vec<VmRegionInfo>, VmRegionEnumerationQuality), String> {
        if context.checkpoint().is_err() {
            return Ok((Vec::new(), VmRegionEnumerationQuality::CaptureDeadline));
        }
        Ok((self.regions.clone(), self.vm_region_quality))
    }

    fn get_task_vm_info(&self, _task: mach_port_t) -> Result<TaskVmSummary, String> {
        self.vm_info
            .clone()
            .ok_or_else(|| "mock: no vm_info".into())
    }

    fn get_task_info_bytes(
        &self,
        _task: mach_port_t,
        flavor: u32,
        buf: &mut [u8],
    ) -> Result<(), String> {
        let data = self
            .task_info_responses
            .get(&flavor)
            .ok_or_else(|| format!("mock: no task_info for flavor {flavor}"))?;
        let len = buf.len().min(data.len());
        buf[..len].copy_from_slice(&data[..len]);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{MockPlatform, PlatformOps, VmRegionEnumerationQuality};
    use std::sync::Arc;
    use std::time::Duration;

    use crate::pipeline::PluginContext;

    #[test]
    fn test_mock_tracks_suspend_resume_counts() {
        let mock = Arc::new(MockPlatform::default());
        assert_eq!(mock.suspend_count(), 0);
        assert_eq!(mock.resume_count(), 0);

        mock.suspend_task(0).unwrap();
        mock.suspend_task(0).unwrap();
        mock.resume_task(0).unwrap();

        assert_eq!(mock.suspend_count(), 2);
        assert_eq!(mock.resume_count(), 1);
    }

    #[test]
    fn test_mock_tracks_deallocated_ports() {
        let mock = MockPlatform::default();
        assert!(mock.deallocated_ports().is_empty());

        mock.deallocate_thread_port(42);
        mock.deallocate_thread_port(99);
        mock.deallocate_thread_port(42);

        assert_eq!(mock.deallocated_ports(), vec![42, 99, 42]);
    }

    #[test]
    fn test_mock_suspend_failure() {
        let mock = MockPlatform {
            suspend_fails: true,
            ..MockPlatform::default()
        };

        let result = mock.suspend_task(0);
        assert!(result.is_err());
        // Count is still incremented even on failure
        assert_eq!(mock.suspend_count(), 1);
    }

    #[test]
    fn test_mock_resume_failure() {
        let mock = MockPlatform {
            resume_fails: true,
            ..MockPlatform::default()
        };

        let result = mock.resume_task(0);
        assert!(result.is_err());
        assert_eq!(mock.resume_count(), 1);
    }

    #[test]
    fn test_mock_vm_enumeration_honors_expired_context() {
        let mock = MockPlatform::default();
        let context = PluginContext::with_timeout(Duration::ZERO);

        let (regions, quality) = mock.enumerate_vm_regions(0, &context).unwrap();
        assert!(regions.is_empty());
        assert_eq!(quality, VmRegionEnumerationQuality::CaptureDeadline);
    }
}
