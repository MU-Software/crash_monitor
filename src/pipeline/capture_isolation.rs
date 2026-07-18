//! Killable process boundary for collectors that require a live Mach task port.

use bincode::Options;
use mach2::port::mach_port_t;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Seek, SeekFrom};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{
    CollectedData, Collector, CollectorAccess, CrashEvent, Diagnostics, Pipeline, PluginDiagnostic,
    PluginRunResult, PluginStatus,
};
use crate::collectors::dylib::RawImageData;
use crate::collectors::memory::RawHeapData;
use crate::collectors::thread::RawThreadData;
use crate::platform::macos::ffi::types::OwnedMachPort;
use crate::platform::{MacOsPlatform, PlatformOps, VmRegionInfo};

const CAPTURE_WIRE_VERSION: u32 = 1;
const MAX_CAPTURE_WIRE_BYTES: u64 = 64 * 1024 * 1024;
const HELPER_POLL_INTERVAL: Duration = Duration::from_millis(5);
const HELPER_REAP_GRACE: Duration = Duration::from_secs(2);
const DEFAULT_COLLECTOR_TIMEOUT_SECS: u32 = 5;

#[cfg(test)]
type TestOutcomeProvider = Box<dyn FnOnce() -> IsolatedCaptureOutcome>;

#[cfg(test)]
thread_local! {
    static TEST_OUTCOME_PROVIDER: std::cell::RefCell<Option<TestOutcomeProvider>> =
        const { std::cell::RefCell::new(None) };
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
enum TaskCollectorKind {
    Thread,
    Memory,
    Dylib,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct TaskCollectorSpec {
    kind: TaskCollectorKind,
    timeout_secs: u32,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct TaskCaptureRequest {
    version: u32,
    event: CrashEvent,
    capture_stack_memory: bool,
    collectors: Vec<TaskCollectorSpec>,
    expect_crashed_thread: bool,
}

#[derive(Default, Serialize, Deserialize)]
pub(crate) struct TaskCaptureData {
    pub threads: Vec<RawThreadData>,
    pub images: Vec<RawImageData>,
    pub memory_map: Vec<VmRegionInfo>,
    pub heap: RawHeapData,
}

#[derive(Serialize, Deserialize)]
struct TaskCaptureResult {
    version: u32,
    data: TaskCaptureData,
    diagnostics: Vec<PluginDiagnostic>,
}

pub(crate) enum IsolatedCaptureOutcome {
    Completed(TaskCaptureData, Vec<PluginDiagnostic>),
    TimedOut,
    Failed(String),
    CleanupUnproven(String),
}

#[cfg(test)]
pub(crate) struct TestOutcomeReset;

#[cfg(test)]
impl Drop for TestOutcomeReset {
    fn drop(&mut self) {
        TEST_OUTCOME_PROVIDER.with(|provider| provider.borrow_mut().take());
    }
}

#[cfg(test)]
pub(crate) fn inject_test_outcome(
    provider: impl FnOnce() -> IsolatedCaptureOutcome + 'static,
) -> TestOutcomeReset {
    TEST_OUTCOME_PROVIDER.with(|slot| {
        assert!(
            slot.borrow_mut().replace(Box::new(provider)).is_none(),
            "a capture-isolation test outcome is already installed"
        );
    });
    TestOutcomeReset
}

impl TaskCaptureRequest {
    pub(crate) fn from_pipeline(
        pipeline: &Pipeline,
        event: &CrashEvent,
    ) -> Result<Option<Self>, String> {
        let mut collectors = Vec::new();
        for collector in &pipeline.collectors {
            if collector.access() != CollectorAccess::IsolatedTask {
                continue;
            }
            let kind = match collector.name() {
                "ThreadCollector" => TaskCollectorKind::Thread,
                "MemoryCollector" => TaskCollectorKind::Memory,
                "DylibCollector" => TaskCollectorKind::Dylib,
                name => {
                    return Err(format!(
                        "task-facing collector '{name}' has no capture-helper implementation"
                    ));
                }
            };
            let timeout_secs = match collector.timeout_secs() {
                u32::MAX => DEFAULT_COLLECTOR_TIMEOUT_SECS,
                timeout => timeout,
            };
            collectors.push(TaskCollectorSpec { kind, timeout_secs });
        }
        if collectors.is_empty() {
            return Ok(None);
        }
        Ok(Some(Self {
            version: CAPTURE_WIRE_VERSION,
            event: event.clone(),
            capture_stack_memory: pipeline.collection_policy.capture_stack_memory,
            collectors,
            expect_crashed_thread: event.crashed_thread.is_some(),
        }))
    }

    fn encode_json(&self) -> Result<String, String> {
        serde_json::to_string(self)
            .map_err(|error| format!("cannot encode capture-helper request: {error}"))
    }
}

/// Execute one helper request. This entry point is called only by the hidden
/// helper subcommand in the freshly `exec`'d monitor binary.
///
/// # Errors
/// Returns an error when the request or inherited capability channel is
/// invalid, a collector result cannot be encoded, or the result cannot be
/// durably written to the inherited descriptor.
pub fn run_capture_helper(request_json: &str) -> Result<(), String> {
    let request: TaskCaptureRequest = serde_json::from_str(request_json)
        .map_err(|error| format!("invalid capture-helper request: {error}"))?;
    if request.version != CAPTURE_WIRE_VERSION {
        return Err(format!(
            "unsupported capture-helper wire version {}",
            request.version
        ));
    }
    let (task, crashed_thread) =
        crate::platform::macos::ffi::capture_spawn::inherited_capture_ports(
            request.expect_crashed_thread,
        )?;
    let platform: Arc<dyn PlatformOps> = Arc::new(MacOsPlatform::default());
    let mut data = CollectedData::default();
    let mut diagnostics = Diagnostics::new();
    let mut event = request.event;
    event.crashed_thread = crashed_thread.as_ref().map(OwnedMachPort::raw);

    for spec in request.collectors {
        let collector: Box<dyn Collector> = match spec.kind {
            TaskCollectorKind::Thread => Box::new(crate::collectors::ThreadCollector::new(
                platform.clone(),
                request.capture_stack_memory,
            )),
            TaskCollectorKind::Memory => {
                Box::new(crate::collectors::MemoryCollector::new(platform.clone()))
            }
            TaskCollectorKind::Dylib => {
                Box::new(crate::collectors::DylibCollector::new(platform.clone()))
            }
        };
        let timeout =
            (spec.timeout_secs != 0).then(|| Duration::from_secs(spec.timeout_secs.into()));
        let context = super::PluginContext::from_timeout(timeout);
        let started = Instant::now();
        let outcome = super::run_plugin_cooperative(collector.name(), &context, |context| {
            collector.collect(&event, task.raw(), &mut data, context)
        });
        diagnostics.record(collector.name(), plugin_status(&outcome), started.elapsed());
    }

    let thread_ports = data
        .raw
        .threads
        .iter()
        .map(|thread| thread.thread_port)
        .collect();
    drop(super::safety::PortGuard::new(thread_ports, platform));

    let result = TaskCaptureResult {
        version: CAPTURE_WIRE_VERSION,
        data: TaskCaptureData {
            threads: data.raw.threads,
            images: data.raw.images,
            memory_map: data.raw.memory_map,
            heap: data.raw.heap,
        },
        diagnostics: diagnostics.plugins,
    };
    let mut output = crate::platform::macos::ffi::capture_spawn::capture_result_file()?;
    wire_options()
        .serialize_into(&mut output, &result)
        .map_err(|error| format!("cannot encode capture-helper result: {error}"))?;
    output
        .sync_data()
        .map_err(|error| format!("cannot sync capture-helper result: {error}"))
}

pub(crate) fn run_isolated_capture(
    request: &TaskCaptureRequest,
    result_file: &mut File,
    task: mach_port_t,
    deadline: Instant,
) -> IsolatedCaptureOutcome {
    #[cfg(test)]
    if let Some(provider) = TEST_OUTCOME_PROVIDER.with(|slot| slot.borrow_mut().take()) {
        return provider();
    }

    if Instant::now() >= deadline {
        return IsolatedCaptureOutcome::TimedOut;
    }

    let executable = match std::env::current_exe() {
        Ok(executable) => executable,
        Err(error) => {
            return IsolatedCaptureOutcome::Failed(format!(
                "cannot locate capture-helper executable: {error}"
            ));
        }
    };
    let request_json = match request.encode_json() {
        Ok(request) => request,
        Err(error) => return IsolatedCaptureOutcome::Failed(error),
    };
    let helper = match crate::platform::macos::ffi::capture_spawn::spawn_capture_helper(
        &executable,
        &request_json,
        result_file,
        task,
        request.event.crashed_thread,
        deadline.saturating_duration_since(Instant::now()),
    ) {
        Ok(pid) => pid,
        Err(error) if error.cleanup_unproven() => {
            return IsolatedCaptureOutcome::CleanupUnproven(error.to_string());
        }
        Err(error) => return IsolatedCaptureOutcome::Failed(error.to_string()),
    };

    loop {
        match reap_helper_nonblocking(helper) {
            Ok(Some(status)) => {
                return if status == 0 {
                    decode_result(result_file)
                } else {
                    IsolatedCaptureOutcome::Failed(format!(
                        "capture helper exited with status {status}"
                    ))
                };
            }
            Ok(None) => {}
            Err(error) => {
                return IsolatedCaptureOutcome::CleanupUnproven(error.to_string());
            }
        }
        if Instant::now() >= deadline {
            return kill_and_reap_timed_out_helper(helper);
        }
        std::thread::sleep(
            HELPER_POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
        );
    }
}

fn decode_result(result_file: &mut File) -> IsolatedCaptureOutcome {
    let length = match result_file.metadata() {
        Ok(metadata) => metadata.len(),
        Err(error) => {
            return IsolatedCaptureOutcome::Failed(format!(
                "cannot inspect capture-helper result: {error}"
            ));
        }
    };
    if length == 0 || length > MAX_CAPTURE_WIRE_BYTES {
        return IsolatedCaptureOutcome::Failed(format!(
            "capture-helper result size is invalid: {length}"
        ));
    }
    if let Err(error) = result_file.seek(SeekFrom::Start(0)) {
        return IsolatedCaptureOutcome::Failed(format!(
            "cannot rewind capture-helper result: {error}"
        ));
    }
    let result: TaskCaptureResult = match wire_options().deserialize_from(result_file) {
        Ok(result) => result,
        Err(error) => {
            return IsolatedCaptureOutcome::Failed(format!(
                "cannot decode capture-helper result: {error}"
            ));
        }
    };
    if result.version != CAPTURE_WIRE_VERSION {
        return IsolatedCaptureOutcome::Failed(format!(
            "unsupported capture-helper result version {}",
            result.version
        ));
    }
    IsolatedCaptureOutcome::Completed(result.data, result.diagnostics)
}

fn kill_and_reap_timed_out_helper(helper: nix::unistd::Pid) -> IsolatedCaptureOutcome {
    if let Err(error) = nix::sys::signal::kill(helper, nix::sys::signal::Signal::SIGKILL)
        && error != nix::errno::Errno::ESRCH
    {
        return IsolatedCaptureOutcome::CleanupUnproven(format!(
            "cannot kill timed-out capture helper {helper}: {error}"
        ));
    }
    let cleanup_deadline = Instant::now() + HELPER_REAP_GRACE;
    loop {
        match reap_helper_nonblocking(helper) {
            Ok(Some(_)) | Err(nix::errno::Errno::ECHILD) => {
                return IsolatedCaptureOutcome::TimedOut;
            }
            Ok(None) => {}
            Err(error) => {
                return IsolatedCaptureOutcome::CleanupUnproven(format!(
                    "cannot reap timed-out capture helper {helper}: {error}"
                ));
            }
        }
        if Instant::now() >= cleanup_deadline {
            return IsolatedCaptureOutcome::CleanupUnproven(format!(
                "capture helper {helper} was not reaped after SIGKILL"
            ));
        }
        std::thread::sleep(HELPER_POLL_INTERVAL);
    }
}

fn reap_helper_nonblocking(pid: nix::unistd::Pid) -> Result<Option<i32>, nix::errno::Errno> {
    use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
    loop {
        return match waitpid(pid, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::Exited(_, status)) => Ok(Some(status)),
            Ok(WaitStatus::Signaled(_, signal, _)) => Ok(Some(128 + signal as i32)),
            Ok(_) => Ok(None),
            Err(nix::errno::Errno::EINTR) => continue,
            Err(error) => Err(error),
        };
    }
}

fn plugin_status<T>(outcome: &PluginRunResult<T>) -> PluginStatus {
    match outcome {
        PluginRunResult::Completed(_) => PluginStatus::Ok,
        PluginRunResult::Failed(error) => PluginStatus::Error(error.clone()),
        PluginRunResult::Panicked => PluginStatus::Error("plugin panicked".into()),
        PluginRunResult::TimedOut => PluginStatus::TimedOut,
    }
}

fn wire_options() -> impl Options {
    bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_limit(MAX_CAPTURE_WIRE_BYTES)
        .reject_trailing_bytes()
}
