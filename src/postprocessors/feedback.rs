//! Post-processor: launch the native feedback dialog as a separate process.
//!
//! The dialog binary (`mbb_crash_dialog_macos`) runs in its own process so that
//! any `AppKit` crash cannot take down the monitor. User feedback text is read from
//! the child's stdout and patched into the already-written report JSON.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::pipeline::report;
use crate::pipeline::{CrashEvent, Plugin, PostProcessor, Priority, ReportResult};

/// Default timeout waiting for the user to submit feedback (5 minutes).
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

/// How often to poll `try_wait` while the dialog process is running.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

pub struct FeedbackPostProcessor {
    dialog_binary: PathBuf,
    timeout: Duration,
}

impl FeedbackPostProcessor {
    #[must_use]
    pub fn new(dialog_binary: PathBuf) -> Self {
        Self {
            dialog_binary,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    #[cfg(test)]
    #[must_use]
    pub fn with_timeout(dialog_binary: PathBuf, timeout: Duration) -> Self {
        Self {
            dialog_binary,
            timeout,
        }
    }
}

impl Plugin for FeedbackPostProcessor {
    fn name(&self) -> &'static str {
        "FeedbackDialog"
    }

    fn priority(&self) -> Priority {
        Priority::Low
    }

    fn is_available(&self) -> bool {
        self.dialog_binary.exists()
    }

    /// Disable alarm-based timeout — this plugin manages its own timeout
    /// via `try_wait` polling with a deadline.
    fn timeout_secs(&self) -> u32 {
        0
    }
}

impl PostProcessor for FeedbackPostProcessor {
    fn process(&self, event: &CrashEvent, result: &mut ReportResult) -> Result<(), String> {
        let Some(json_path) = &result.json_path else {
            return Ok(());
        };

        let timestamp = &result
            .session
            .as_ref()
            .map_or_else(|| "unknown".to_string(), |s| s.start.clone());

        let mut child = Command::new(&self.dialog_binary)
            .arg("--type")
            .arg(event.report_type.as_str())
            .arg("--process")
            .arg(&event.process_name)
            .arg("--timestamp")
            .arg(timestamp)
            // Own process group (pgid == child pid) so a timeout can kill the
            // whole subtree, not just the direct child. Without this, a dialog
            // that spawned helpers would orphan them on kill.
            .process_group(0)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| format!("Failed to spawn dialog: {e}"))?;

        // Poll with timeout.
        let deadline = Instant::now() + self.timeout;
        let exit_status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        eprintln!("[monitor] FeedbackDialog timed out, killing");
                        // Kill the whole process group (child + any descendants).
                        // child.kill() alone would orphan grandchildren.
                        #[allow(clippy::cast_possible_wrap)] // PID always fits in i32
                        let pgid = nix::unistd::Pid::from_raw(child.id() as i32);
                        let _ = nix::sys::signal::killpg(pgid, nix::sys::signal::Signal::SIGKILL);
                        let _ = child.wait();
                        return Ok(());
                    }
                    std::thread::sleep(POLL_INTERVAL);
                }
                Err(e) => {
                    eprintln!("[monitor] FeedbackDialog wait error: {e}");
                    return Ok(());
                }
            }
        };

        if !exit_status.success() {
            // exit 1 = skip, anything else = dialog error/crash.
            let code = exit_status.code().unwrap_or(-1);
            if code == 1 {
                eprintln!("[monitor] Feedback skipped by user");
            } else {
                eprintln!("[monitor] FeedbackDialog exited with code {code}");
            }
            return Ok(());
        }

        // Read feedback text from stdout.
        let mut feedback = String::new();
        if let Some(mut stdout) = child.stdout.take()
            && let Err(e) = stdout.read_to_string(&mut feedback)
        {
            eprintln!("[monitor] Failed to read feedback: {e}");
            return Ok(());
        }

        let feedback = feedback.trim();
        if feedback.is_empty() {
            return Ok(());
        }

        // Patch report JSON with user feedback (atomic: write tmp → rename).
        match report::load_report(json_path) {
            Ok(mut crash_report) => {
                crash_report.user_feedback = Some(serde_json::json!({ "comment": feedback }));
                match serde_json::to_string_pretty(&crash_report) {
                    Ok(json) => {
                        let tmp = json_path.with_extension("json.tmp");
                        if let Err(e) = std::fs::write(&tmp, &json) {
                            eprintln!("[monitor] Failed to write feedback tmp: {e}");
                        } else if let Err(e) = std::fs::rename(&tmp, json_path) {
                            eprintln!("[monitor] Failed to rename feedback tmp: {e}");
                            // Fallback: try direct write.
                            let _ = std::fs::write(json_path, json);
                        } else {
                            eprintln!("[monitor] Feedback saved to report");
                        }
                    }
                    Err(e) => eprintln!("[monitor] Failed to serialize report: {e}"),
                }
            }
            Err(e) => eprintln!("[monitor] Failed to load report for feedback patch: {e}"),
        }

        Ok(())
    }
}

#[cfg(test)]
#[path = "../../tests/unit/postprocessors/feedback_tests.rs"]
mod tests;
