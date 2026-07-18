//! Killable process boundary for collectors that require a live Mach task port.

use mach2::port::mach_port_t;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Cursor, Read, Seek, SeekFrom, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{
    CollectedData, Collector, CollectorAccess, CrashEvent, Diagnostics, Pipeline, PluginDiagnostic,
    PluginRunResult, PluginStatus, ReportId,
};
use crate::collectors::dylib::RawImageData;
use crate::collectors::memory::RawHeapData;
use crate::collectors::thread::RawThreadData;
use crate::platform::macos::ffi::capture_spawn::{CaptureHelperProcess, CaptureHelperReap};
use crate::platform::macos::ffi::types::OwnedThreadPort;
use crate::platform::{MacOsPlatform, PlatformOps, VmRegionInfo};

const CAPTURE_WIRE_VERSION: u32 = 2;
const MAX_CAPTURE_WIRE_BYTES: usize = 64 * 1024 * 1024;
const HELPER_POLL_INTERVAL: Duration = Duration::from_millis(5);
const HELPER_REAP_GRACE: Duration = Duration::from_secs(2);
const DEFAULT_COLLECTOR_TIMEOUT: Duration = Duration::from_secs(5);

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
    timeout: Option<Duration>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskCaptureRequest {
    version: u32,
    event: CrashEvent,
    capture_stack_memory: bool,
    collectors: Vec<TaskCollectorSpec>,
    expect_crashed_thread: bool,
    /// Integration-only assertion that an arbitrary parent descriptor was
    /// closed by the capture-helper spawn allowlist.
    #[cfg(any(test, feature = "test-support"))]
    #[serde(default)]
    inherited_fd_must_be_closed: Option<i32>,
    /// Integration-only delay used to exercise supervisor timeout kill/reap.
    #[cfg(any(test, feature = "test-support"))]
    #[serde(default)]
    hold_after_handoff_ms: Option<u64>,
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
    diagnostics: Vec<TaskCaptureDiagnostic>,
}

/// Fixed-layout binary representation of plugin diagnostics.
///
/// The report-facing `PluginDiagnostic` intentionally omits a missing
/// `report_id` in JSON. Bincode is not self-describing, so applying that
/// conditional omission to the helper wire format would shift every
/// following field and make the payload undecodable.
#[derive(Serialize, Deserialize)]
struct TaskCaptureDiagnostic {
    name: String,
    status: PluginStatus,
    duration_ms: u64,
    report_id: Option<ReportId>,
    started_offset_ms: u64,
    finished_offset_ms: u64,
}

impl From<PluginDiagnostic> for TaskCaptureDiagnostic {
    fn from(diagnostic: PluginDiagnostic) -> Self {
        Self {
            name: diagnostic.name,
            status: diagnostic.status,
            duration_ms: diagnostic.duration_ms,
            report_id: diagnostic.report_id,
            started_offset_ms: diagnostic.started_offset_ms,
            finished_offset_ms: diagnostic.finished_offset_ms,
        }
    }
}

impl From<TaskCaptureDiagnostic> for PluginDiagnostic {
    fn from(diagnostic: TaskCaptureDiagnostic) -> Self {
        Self {
            name: diagnostic.name,
            status: diagnostic.status,
            duration_ms: diagnostic.duration_ms,
            report_id: diagnostic.report_id,
            started_offset_ms: diagnostic.started_offset_ms,
            finished_offset_ms: diagnostic.finished_offset_ms,
        }
    }
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
            let timeout = collector.timeout().resolve(DEFAULT_COLLECTOR_TIMEOUT);
            collectors.push(TaskCollectorSpec { kind, timeout });
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
            #[cfg(any(test, feature = "test-support"))]
            inherited_fd_must_be_closed: None,
            #[cfg(any(test, feature = "test-support"))]
            hold_after_handoff_ms: None,
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
    #[cfg(any(test, feature = "test-support"))]
    if let Some(fd) = request.inherited_fd_must_be_closed {
        verify_descriptor_is_closed(fd)?;
    }
    let (task, crashed_thread) =
        crate::platform::macos::ffi::capture_spawn::inherited_capture_ports(
            request.expect_crashed_thread,
        )?;
    #[cfg(any(test, feature = "test-support"))]
    if let Some(delay_ms) = request.hold_after_handoff_ms {
        std::thread::sleep(Duration::from_millis(delay_ms.min(30_000)));
    }
    let platform: Arc<dyn PlatformOps> = Arc::new(MacOsPlatform::default());
    let mut data = CollectedData::default();
    let mut diagnostics = Diagnostics::new();
    let mut event = request.event;
    event.crashed_thread = crashed_thread.as_ref().map(OwnedThreadPort::raw);

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
        let context = super::PluginContext::from_timeout(spec.timeout);
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
        diagnostics: diagnostics.plugins.into_iter().map(Into::into).collect(),
    };
    let mut output = crate::platform::macos::ffi::capture_spawn::capture_result_file()?;
    let encoded = rmp_serde::to_vec(&result)
        .map_err(|error| format!("cannot encode capture-helper result: {error}"))?;
    if encoded.len() > MAX_CAPTURE_WIRE_BYTES {
        return Err(format!(
            "capture-helper result exceeds {MAX_CAPTURE_WIRE_BYTES} bytes"
        ));
    }
    output
        .write_all(&encoded)
        .map_err(|error| format!("cannot write capture-helper result: {error}"))?;
    output
        .sync_data()
        .map_err(|error| format!("cannot sync capture-helper result: {error}"))
}

#[cfg(any(test, feature = "test-support"))]
fn verify_descriptor_is_closed(fd: i32) -> Result<(), String> {
    if fd < 0 || fd == crate::platform::macos::ffi::capture_spawn::CAPTURE_HELPER_RESULT_FD {
        return Err(format!(
            "invalid capture-helper closed-descriptor probe {fd}"
        ));
    }
    // SAFETY: F_GETFD only queries the helper descriptor table.
    let result = unsafe { nix::libc::fcntl(fd, nix::libc::F_GETFD) };
    if result >= 0 {
        return Err(format!(
            "capture helper unexpectedly inherited parent descriptor {fd}"
        ));
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(nix::libc::EBADF) {
        Ok(())
    } else {
        Err(format!(
            "capture helper could not verify descriptor {fd} closure: {error}"
        ))
    }
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
    let mut helper = match crate::platform::macos::ffi::capture_spawn::spawn_capture_helper(
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
        match helper.poll_reap() {
            Ok(CaptureHelperReap::Exited(status)) => {
                return if status == 0 {
                    decode_result(result_file)
                } else {
                    IsolatedCaptureOutcome::Failed(format!(
                        "capture helper exited with status {status}"
                    ))
                };
            }
            Ok(CaptureHelperReap::Signaled {
                signal,
                core_dumped,
            }) => {
                return IsolatedCaptureOutcome::Failed(format!(
                    "capture helper terminated by signal {signal} (core_dumped={core_dumped})"
                ));
            }
            Ok(CaptureHelperReap::StillRunning) => {}
            Ok(CaptureHelperReap::OwnershipLost) => {
                return IsolatedCaptureOutcome::CleanupUnproven(format!(
                    "capture helper {} wait ownership was lost (ECHILD)",
                    helper.pid()
                ));
            }
            Err(error) => {
                return IsolatedCaptureOutcome::CleanupUnproven(error);
            }
        }
        if Instant::now() >= deadline {
            return kill_and_reap_timed_out_helper(&mut helper);
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
    if length == 0 || length > MAX_CAPTURE_WIRE_BYTES as u64 {
        return IsolatedCaptureOutcome::Failed(format!(
            "capture-helper result size is invalid: {length}"
        ));
    }
    if let Err(error) = result_file.seek(SeekFrom::Start(0)) {
        return IsolatedCaptureOutcome::Failed(format!(
            "cannot rewind capture-helper result: {error}"
        ));
    }
    let Ok(capacity) = usize::try_from(length) else {
        return IsolatedCaptureOutcome::Failed(format!(
            "capture-helper result size is not addressable: {length}"
        ));
    };
    let mut encoded = Vec::with_capacity(capacity);
    if let Err(error) = result_file.read_to_end(&mut encoded) {
        return IsolatedCaptureOutcome::Failed(format!(
            "cannot read capture-helper result: {error}"
        ));
    }
    let mut decoder = rmp_serde::Deserializer::new(Cursor::new(&encoded));
    let result = match TaskCaptureResult::deserialize(&mut decoder) {
        Ok(result) => result,
        Err(error) => {
            return IsolatedCaptureOutcome::Failed(format!(
                "cannot decode {length}-byte capture-helper result: {error}"
            ));
        }
    };
    let Ok(consumed) = usize::try_from(decoder.position()) else {
        return IsolatedCaptureOutcome::Failed(
            "capture-helper decoder position is not addressable".to_string(),
        );
    };
    if consumed != encoded.len() {
        return IsolatedCaptureOutcome::Failed(format!(
            "capture-helper decoder consumed {consumed} of {} bytes",
            encoded.len()
        ));
    }
    if result.version != CAPTURE_WIRE_VERSION {
        return IsolatedCaptureOutcome::Failed(format!(
            "unsupported capture-helper result version {}",
            result.version
        ));
    }
    IsolatedCaptureOutcome::Completed(
        result.data,
        result.diagnostics.into_iter().map(Into::into).collect(),
    )
}

fn kill_and_reap_timed_out_helper(helper: &mut CaptureHelperProcess) -> IsolatedCaptureOutcome {
    if let Err(error) = nix::sys::signal::kill(helper.pid(), nix::sys::signal::Signal::SIGKILL)
        && error != nix::errno::Errno::ESRCH
    {
        return IsolatedCaptureOutcome::CleanupUnproven(format!(
            "cannot kill timed-out capture helper {}: {error}",
            helper.pid()
        ));
    }
    let cleanup_deadline = Instant::now() + HELPER_REAP_GRACE;
    loop {
        match helper.poll_reap() {
            Ok(CaptureHelperReap::Exited(_) | CaptureHelperReap::Signaled { .. }) => {
                return IsolatedCaptureOutcome::TimedOut;
            }
            Ok(CaptureHelperReap::StillRunning) => {}
            Ok(CaptureHelperReap::OwnershipLost) => {
                return IsolatedCaptureOutcome::CleanupUnproven(format!(
                    "timed-out capture helper {} wait ownership was lost (ECHILD)",
                    helper.pid()
                ));
            }
            Err(error) => {
                return IsolatedCaptureOutcome::CleanupUnproven(format!(
                    "cannot reap timed-out capture helper {}: {error}",
                    helper.pid()
                ));
            }
        }
        if Instant::now() >= cleanup_deadline {
            return IsolatedCaptureOutcome::CleanupUnproven(format!(
                "capture helper {} was not reaped after SIGKILL",
                helper.pid()
            ));
        }
        std::thread::sleep(HELPER_POLL_INTERVAL);
    }
}

fn plugin_status<T>(outcome: &PluginRunResult<T>) -> PluginStatus {
    match outcome {
        PluginRunResult::Completed(_) => PluginStatus::Ok,
        PluginRunResult::Failed(error) => PluginStatus::Error(error.to_string()),
        PluginRunResult::Panicked(message) => PluginStatus::Panic(message.clone()),
        PluginRunResult::TimedOut => PluginStatus::TimedOut,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_wire_round_trip_consumes_exact_message() {
        let result = TaskCaptureResult {
            version: CAPTURE_WIRE_VERSION,
            data: TaskCaptureData::default(),
            diagnostics: vec![TaskCaptureDiagnostic {
                name: "ThreadCollector".to_string(),
                status: PluginStatus::Ok,
                duration_ms: 12,
                report_id: None,
                started_offset_ms: 3,
                finished_offset_ms: 15,
            }],
        };
        let encoded = rmp_serde::to_vec(&result).expect("encode result");
        let mut deserializer = rmp_serde::Deserializer::new(Cursor::new(&encoded));
        let decoded = TaskCaptureResult::deserialize(&mut deserializer).expect("decode result");
        let consumed = usize::try_from(deserializer.position()).expect("wire length fits usize");
        assert_eq!(consumed, encoded.len());
        assert_eq!(decoded.version, CAPTURE_WIRE_VERSION);
        assert_eq!(decoded.diagnostics.len(), 1);
        assert!(decoded.diagnostics[0].report_id.is_none());
        assert_eq!(decoded.diagnostics[0].finished_offset_ms, 15);

        let mut result_file = tempfile::tempfile().expect("create wire result");
        result_file.write_all(&encoded).expect("write wire result");
        result_file.write_all(&[0]).expect("append trailing byte");
        let IsolatedCaptureOutcome::Failed(error) = decode_result(&mut result_file) else {
            panic!("trailing wire bytes must be rejected");
        };
        assert!(error.contains("decoder consumed"));
    }
}
