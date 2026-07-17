//! Post-processor: launch the native feedback dialog as a separate process.
//!
//! The dialog binary (`crash_dialog_macos`) runs in its own process so that
//! any `AppKit` crash cannot take down the monitor. User feedback text is read from
//! the child's stdout and patched into the already-written report JSON.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use crate::pipeline::report;
use crate::pipeline::{
    CrashEvent, Plugin, PluginContext, PluginExecution, PluginRunResult, PostProcessor, Priority,
    ReportResult, run_plugin_subprocess,
};

/// Default timeout waiting for the user to submit feedback (5 minutes).
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(300);

pub struct FeedbackPostProcessor {
    dialog_binary: PathBuf,
    timeout: Duration,
    available: bool,
}

impl FeedbackPostProcessor {
    #[must_use]
    pub fn new(dialog_binary: PathBuf) -> Self {
        let available = dialog_binary.is_file();
        Self {
            dialog_binary,
            timeout: DEFAULT_TIMEOUT,
            available,
        }
    }

    #[cfg(test)]
    #[must_use]
    pub fn with_timeout(dialog_binary: PathBuf, timeout: Duration) -> Self {
        let available = dialog_binary.is_file();
        Self {
            dialog_binary,
            timeout,
            available,
        }
    }
}

impl Plugin for FeedbackPostProcessor {
    fn name(&self) -> &'static str {
        "FeedbackDialog"
    }

    fn execution(&self) -> PluginExecution {
        PluginExecution::Subprocess
    }

    fn priority(&self) -> Priority {
        Priority::Low
    }

    fn is_available(&self) -> bool {
        self.available
    }

    fn timeout_secs(&self) -> u32 {
        let rounded = self
            .timeout
            .as_secs()
            .saturating_add(u64::from(self.timeout.subsec_nanos() != 0));
        u32::try_from(rounded).unwrap_or(u32::MAX - 1)
    }
}

impl PostProcessor for FeedbackPostProcessor {
    fn process(
        &self,
        event: &CrashEvent,
        result: &mut ReportResult,
        context: &PluginContext,
    ) -> Result<(), String> {
        let Some(json_path) = &result.json_path else {
            // This invocation has no external work to perform. Explicitly
            // satisfy the declared boundary so the pipeline can distinguish
            // an intentional no-op from a Subprocess plugin that forgot to
            // use the supervisor.
            context.mark_subprocess_not_required();
            return Ok(());
        };

        let timestamp = &result
            .session
            .as_ref()
            .map_or_else(|| "unknown".to_string(), |s| s.start.clone());

        let mut command = Command::new(&self.dialog_binary);
        command
            .arg("--type")
            .arg(event.report_type.as_str())
            .arg("--process")
            .arg(&event.process_name)
            .arg("--timestamp")
            .arg(timestamp);

        let isolated_context = context.bounded_by(self.timeout);
        let output = match run_plugin_subprocess(self.name(), &mut command, &isolated_context) {
            PluginRunResult::Completed(output) => output,
            PluginRunResult::TimedOut => {
                // Propagate the nested helper deadline to the outer plugin
                // runner so diagnostics record `TimedOut`, not `Error`.
                context.cancellation_token().cancel();
                return Err("feedback dialog timed out".to_string());
            }
            PluginRunResult::Failed(error) => return Err(error),
            PluginRunResult::Panicked => {
                return Err("feedback subprocess supervisor panicked".to_string());
            }
        };

        if !output.status.success() {
            // exit 1 = skip, anything else = dialog error/crash.
            let code = output.status.code().unwrap_or(-1);
            if code == 1 {
                eprintln!("[monitor] Feedback skipped by user");
                return Ok(());
            }
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "FeedbackDialog exited with {}: {}",
                output.status,
                stderr.trim()
            ));
        }

        if output.stdout_truncated {
            return Err("feedback output exceeded 1 MiB".to_string());
        }
        let feedback = String::from_utf8(output.stdout)
            .map_err(|error| format!("feedback output was not UTF-8: {error}"))?;
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
