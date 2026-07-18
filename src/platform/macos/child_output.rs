//! Bounded, continuously-drained child stdout/stderr capture.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::io::Read;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Default number of bytes retained independently for stdout and stderr.
pub const DEFAULT_CHILD_OUTPUT_TAIL_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChildStreamTail {
    pub tail: String,
    pub truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChildOutputSnapshot {
    pub stdout: ChildStreamTail,
    pub stderr: ChildStreamTail,
}

#[derive(Default)]
struct StreamState {
    bytes: VecDeque<u8>,
    truncated: bool,
    read_error: Option<String>,
    finished: bool,
}

struct StreamCapture {
    state: Mutex<StreamState>,
    finished: Condvar,
}

impl StreamCapture {
    fn new() -> Self {
        Self {
            state: Mutex::new(StreamState::default()),
            finished: Condvar::new(),
        }
    }

    fn push(&self, bytes: &[u8], capacity: usize) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if bytes.len() >= capacity {
            state.bytes.clear();
            state.bytes.extend(
                bytes[bytes.len().saturating_sub(capacity)..]
                    .iter()
                    .copied(),
            );
            state.truncated = true;
            return;
        }
        let overflow = state
            .bytes
            .len()
            .saturating_add(bytes.len())
            .saturating_sub(capacity);
        if overflow != 0 {
            state.bytes.drain(..overflow);
            state.truncated = true;
        }
        state.bytes.extend(bytes.iter().copied());
    }

    fn finish(&self, error: Option<String>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.read_error = error;
        state.finished = true;
        self.finished.notify_all();
    }

    fn snapshot(&self, wait_until: Option<Instant>) -> ChildStreamTail {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !state.finished {
            let Some(deadline) = wait_until else {
                break;
            };
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let result = self
                .finished
                .wait_timeout(state, deadline.saturating_duration_since(now));
            let (next, timeout) = result.unwrap_or_else(std::sync::PoisonError::into_inner);
            state = next;
            if timeout.timed_out() {
                break;
            }
        }
        let bytes: Vec<u8> = state.bytes.iter().copied().collect();
        ChildStreamTail {
            tail: String::from_utf8_lossy(&bytes).into_owned(),
            truncated: state.truncated,
            read_error: state.read_error.clone(),
        }
    }
}

/// Shared capture state populated by two dedicated pipe-drain threads.
pub struct ChildOutputCapture {
    capacity: usize,
    stdout: Arc<StreamCapture>,
    stderr: Arc<StreamCapture>,
}

impl ChildOutputCapture {
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            stdout: Arc::new(StreamCapture::new()),
            stderr: Arc::new(StreamCapture::new()),
        }
    }

    pub(crate) fn attach_stdout<R: Read + Send + 'static>(&self, reader: R) {
        Self::start_drain(reader, &self.stdout, self.capacity, "stdout");
    }

    pub(crate) fn attach_stderr<R: Read + Send + 'static>(&self, reader: R) {
        Self::start_drain(reader, &self.stderr, self.capacity, "stderr");
    }

    fn start_drain<R: Read + Send + 'static>(
        mut reader: R,
        stream: &Arc<StreamCapture>,
        capacity: usize,
        name: &'static str,
    ) {
        let drain_stream = stream.clone();
        let spawn = std::thread::Builder::new()
            .name(format!("child-{name}-drain"))
            .spawn(move || {
                let mut buffer = [0_u8; 8192];
                loop {
                    match reader.read(&mut buffer) {
                        Ok(0) => {
                            drain_stream.finish(None);
                            break;
                        }
                        Ok(read) => drain_stream.push(&buffer[..read], capacity),
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(error) => {
                            drain_stream.finish(Some(error.to_string()));
                            break;
                        }
                    }
                }
            });
        if let Err(error) = spawn {
            stream.finish(Some(format!("failed to start drain thread: {error}")));
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> ChildOutputSnapshot {
        ChildOutputSnapshot {
            stdout: self.stdout.snapshot(None),
            stderr: self.stderr.snapshot(None),
        }
    }

    /// Wait a bounded amount for pipe EOF after terminal waitpid, then return
    /// whatever tail is available. A stuck descriptor can never delay report
    /// finalization beyond `timeout`.
    #[must_use]
    pub fn snapshot_after_eof(&self, timeout: Duration) -> ChildOutputSnapshot {
        let deadline = Instant::now() + timeout;
        ChildOutputSnapshot {
            stdout: self.stdout.snapshot(Some(deadline)),
            stderr: self.stderr.snapshot(Some(deadline)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_only_the_bounded_tail() {
        let capture = ChildOutputCapture::new(5);
        capture.attach_stdout(std::io::Cursor::new(b"0123456789"));
        capture.attach_stderr(std::io::Cursor::new(Vec::<u8>::new()));
        let snapshot = capture.snapshot_after_eof(Duration::from_secs(1));
        assert_eq!(snapshot.stdout.tail, "56789");
        assert!(snapshot.stdout.truncated);
        assert!(snapshot.stdout.read_error.is_none());
    }
}
