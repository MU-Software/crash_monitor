//! Mock platform for unit testing collectors and pipeline without real Mach APIs.

use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use mach2::port::mach_port_t;

use crate::pipeline::PluginContext;

use super::{
    ArmThreadState64, PlatformOps, SupervisorHealth, TaskControlFailure, TaskVmSummary,
    VmReadError, VmRegionEnumerationQuality, VmRegionInfo, classify_vm_read,
};

/// Mock thread data: port, optional name, register state [u32; 68].
// Cargo's self dev-dependency enables `test-support` for the binary during
// all-target linting even though production code never constructs mocks.
#[allow(dead_code)]
pub struct MockThread {
    pub port: mach_port_t,
    pub stable_id: u64,
    pub name: Option<String>,
    pub state: Vec<u32>,
}

/// Configurable mock platform. Tests set fields before passing to collectors.
///
/// Tracks invocations of key methods (suspend, resume, deallocate) so tests
/// can verify the pipeline called them correctly.
#[allow(dead_code, clippy::struct_excessive_bools)]
pub struct MockPlatform {
    /// Route capture through the exec-isolation supervisor. This is disabled
    /// by default and exists for deterministic worker-boundary tests.
    pub capture_isolation: bool,
    pub threads: Vec<MockThread>,
    /// Memory map: address → bytes. `vm_read` returns matching range.
    pub memory: BTreeMap<u64, Vec<u8>>,
    pub regions: Vec<VmRegionInfo>,
    pub vm_region_quality: VmRegionEnumerationQuality,
    pub vm_info: Option<TaskVmSummary>,
    /// Task info responses: flavor → native-endian u32 words.
    pub task_info_responses: BTreeMap<u32, Vec<u32>>,
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
    retain_task_port_count: AtomicUsize,
    deallocate_task_port_count: AtomicUsize,
    supervisor_health: Mutex<SupervisorHealth>,
    deallocated_ports: Mutex<Vec<mach_port_t>>,
}

#[allow(dead_code)]
impl MockPlatform {
    /// Enable the production-style capture isolation supervisor for a test.
    #[must_use]
    pub fn with_capture_isolation(mut self) -> Self {
        self.capture_isolation = true;
        self
    }

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

    /// Number of independently owned task send-right references retained for
    /// capture workers.
    pub fn retain_task_port_count(&self) -> usize {
        self.retain_task_port_count.load(Ordering::SeqCst)
    }

    /// Number of retained task send-right references released by workers.
    pub fn deallocate_task_port_count(&self) -> usize {
        self.deallocate_task_port_count.load(Ordering::SeqCst)
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
            capture_isolation: false,
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
            retain_task_port_count: AtomicUsize::new(0),
            deallocate_task_port_count: AtomicUsize::new(0),
            supervisor_health: Mutex::new(SupervisorHealth::default()),
            deallocated_ports: Mutex::new(Vec::new()),
        }
    }
}

impl PlatformOps for MockPlatform {
    fn supports_capture_isolation(&self) -> bool {
        self.capture_isolation
    }

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

    fn retain_task_port(&self, _task: mach_port_t) -> Result<(), String> {
        self.retain_task_port_count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    fn deallocate_task_port(&self, _task: mach_port_t) {
        self.deallocate_task_port_count
            .fetch_add(1, Ordering::SeqCst);
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
        self.threads
            .iter()
            .find(|t| t.port == thread)
            .ok_or_else(|| format!("mock: thread_info for unknown thread {thread}"))
            .map(|thread| thread.name.clone())
    }

    fn get_thread_identifier(&self, thread: mach_port_t) -> Result<u64, String> {
        self.threads
            .iter()
            .find(|candidate| candidate.port == thread)
            .map(|candidate| candidate.stable_id)
            .ok_or_else(|| format!("mock: thread {thread} not found"))
    }

    fn get_thread_state(&self, thread: mach_port_t) -> Result<ArmThreadState64, String> {
        self.threads
            .iter()
            .find(|t| t.port == thread)
            .ok_or_else(|| format!("mock: thread {thread} not found"))
            .and_then(|thread| ArmThreadState64::try_from_words(&thread.state))
    }

    fn deallocate_thread_port(&self, thread: mach_port_t) {
        self.deallocated_ports.lock().unwrap().push(thread);
    }

    fn vm_read(
        &self,
        _task: mach_port_t,
        address: u64,
        size: usize,
    ) -> Result<Vec<u8>, VmReadError> {
        // Find the memory region that contains this address
        for (&base, data) in &self.memory {
            let Some(relative) = address.checked_sub(base) else {
                continue;
            };
            let Ok(diff) = usize::try_from(relative) else {
                continue;
            };
            if diff < data.len() {
                let available = data.len() - diff;
                let returned = size.min(available);
                return classify_vm_read(data[diff..diff + returned].to_vec(), size);
            }
        }
        Err(VmReadError::Platform(format!(
            "mock: vm_read at {address:#x} size {size} not found"
        )))
    }

    fn vm_region_query(&self, _task: mach_port_t, address: u64) -> Result<VmRegionInfo, String> {
        // mach_vm_region selects the lowest-address region containing or
        // following the query, independent of fixture insertion order.
        self.regions
            .iter()
            .filter(|region| {
                region
                    .address
                    .checked_add(region.size)
                    .is_some_and(|end| end > address)
            })
            .min_by_key(|region| region.address)
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

    fn get_task_info_words(
        &self,
        _task: mach_port_t,
        flavor: u32,
        words: &mut [u32],
    ) -> Result<usize, String> {
        let data = self
            .task_info_responses
            .get(&flavor)
            .ok_or_else(|| format!("mock: no task_info for flavor {flavor}"))?;
        if data.len() > words.len() {
            return Err(format!(
                "mock: task_info buffer has {} words, response requires {}",
                words.len(),
                data.len()
            ));
        }
        words[..data.len()].copy_from_slice(data);
        Ok(data.len())
    }
}

#[cfg(test)]
mod tests {
    use super::{MockPlatform, MockThread, PlatformOps, VmRegionEnumerationQuality};
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

    #[test]
    fn test_mock_task_info_reports_short_count_and_rejects_small_buffer() {
        let mut mock = MockPlatform::default();
        mock.task_info_responses.insert(17, vec![1, 2, 3, 4]);

        let mut ample = [0_u32; 5];
        assert_eq!(mock.get_task_info_words(0, 17, &mut ample).unwrap(), 4);
        assert_eq!(&ample[..4], &[1, 2, 3, 4]);

        let mut small = [0_u32; 3];
        assert!(
            mock.get_task_info_words(0, 17, &mut small)
                .unwrap_err()
                .contains("requires 4")
        );
    }

    #[test]
    fn test_mock_task_info_rejects_unknown_flavor() {
        let mock = MockPlatform::default();
        assert!(
            mock.get_task_info_words(0, 999, &mut [0; 4])
                .unwrap_err()
                .contains("no task_info for flavor 999")
        );
    }

    #[test]
    fn test_mock_thread_state_requires_all_consumer_words() {
        let mut mock = MockPlatform::default();
        mock.threads.push(MockThread {
            port: 10,
            stable_id: 10,
            name: None,
            state: vec![0; 66],
        });
        assert!(mock.get_thread_state(10).unwrap_err().contains("66 words"));

        mock.threads[0].state.push(123);
        let state = mock.get_thread_state(10).unwrap();
        assert_eq!(state[66], 123);
        assert_eq!(state.words().len(), 68);
    }

    #[test]
    fn test_mock_thread_name_distinguishes_unnamed_from_unknown() {
        let mut mock = MockPlatform::default();
        mock.threads.push(MockThread {
            port: 10,
            name: None,
            state: vec![0; 68],
        });

        assert_eq!(mock.get_thread_name(10).unwrap(), None);
        assert!(
            mock.get_thread_name(11)
                .unwrap_err()
                .contains("unknown thread 11")
        );
    }

    #[test]
    fn test_mock_vm_read_distinguishes_exact_short_and_empty_results() {
        let mut mock = MockPlatform::default();
        mock.memory.insert(0x1000, vec![1, 2, 3, 4]);

        assert_eq!(mock.vm_read(0, 0x1000, 4).unwrap(), vec![1, 2, 3, 4]);
        assert_eq!(
            mock.vm_read(0, 0x1000, 8).unwrap_err(),
            crate::platform::VmReadError::Partial {
                requested: 8,
                bytes: vec![1, 2, 3, 4],
            }
        );
        assert_eq!(
            crate::platform::classify_vm_read(Vec::new(), 1).unwrap_err(),
            crate::platform::VmReadError::Partial {
                requested: 1,
                bytes: Vec::new(),
            }
        );
    }

    #[test]
    fn test_mock_vm_region_query_selects_lowest_suitable_address() {
        let region = |address, size| crate::platform::VmRegionInfo {
            address,
            size,
            protection: 0,
            user_tag: 0,
            share_mode: 0,
            pages_resident: 0,
            pages_swapped_out: 0,
        };
        let mut mock = MockPlatform::default();
        mock.regions = vec![
            region(0x5000, 0x1000),
            region(u64::MAX - 3, 8),
            region(0x3000, 0x1000),
        ];

        assert_eq!(mock.vm_region_query(0, 0x1000).unwrap().address, 0x3000);
        assert_eq!(mock.vm_region_query(0, 0x3800).unwrap().address, 0x3000);
        assert_eq!(mock.vm_region_query(0, 0x4000).unwrap().address, 0x5000);
        assert!(mock.vm_region_query(0, 0x6000).is_err());
    }
}
