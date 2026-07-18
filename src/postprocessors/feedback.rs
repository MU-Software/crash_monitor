//! Post-processor: launch the native feedback dialog as a separate process.
//!
//! The dialog binary (`crash_dialog_macos`) runs in its own process so that
//! any `AppKit` crash cannot take down the monitor. User feedback text is read from
//! the child's stdout and patched into the already-written report JSON.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;
use std::{fs, os::unix::fs::MetadataExt, os::unix::fs::PermissionsExt, path::Path};

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
        let allowed_dir = std::env::current_exe()
            .ok()
            .and_then(|path| path.parent().map(Path::to_path_buf));
        let available = allowed_dir.is_some_and(|allowed_dir| {
            validate_dialog_binary(&dialog_binary, &allowed_dir, true).is_ok()
        });
        Self {
            dialog_binary,
            timeout: DEFAULT_TIMEOUT,
            available,
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn for_test(dialog_binary: PathBuf) -> Self {
        let available = dialog_binary.parent().is_some_and(|allowed_dir| {
            validate_dialog_binary(&dialog_binary, allowed_dir, false).is_ok()
        });
        Self {
            dialog_binary,
            timeout: DEFAULT_TIMEOUT,
            available,
        }
    }

    #[cfg(test)]
    #[must_use]
    pub fn with_timeout(dialog_binary: PathBuf, timeout: Duration) -> Self {
        let available = dialog_binary.parent().is_some_and(|allowed_dir| {
            validate_dialog_binary(&dialog_binary, allowed_dir, false).is_ok()
        });
        Self {
            dialog_binary,
            timeout,
            available,
        }
    }
}

fn validate_dialog_binary(
    path: &Path,
    allowed_dir: &Path,
    require_signature: bool,
) -> Result<(), String> {
    let metadata = fs::symlink_metadata(path).map_err(|error| {
        format!(
            "cannot inspect feedback dialog '{}': {error}",
            path.display()
        )
    })?;
    if !metadata.file_type().is_file() {
        return Err(format!(
            "feedback dialog is not a regular file: '{}'",
            path.display()
        ));
    }
    // SAFETY: `geteuid` has no preconditions and does not dereference memory.
    let effective_uid = unsafe { nix::libc::geteuid() };
    if metadata.uid() != effective_uid {
        return Err(format!(
            "feedback dialog is not owned by the effective user: '{}'",
            path.display()
        ));
    }
    let mode = metadata.permissions().mode();
    if mode & 0o111 == 0 || mode & 0o022 != 0 {
        return Err(format!(
            "feedback dialog has unsafe mode {:04o}: '{}'",
            mode & 0o7777,
            path.display()
        ));
    }
    let canonical = path
        .canonicalize()
        .map_err(|error| format!("cannot resolve feedback dialog: {error}"))?;
    let allowed = allowed_dir
        .canonicalize()
        .map_err(|error| format!("cannot resolve feedback dialog directory: {error}"))?;
    if canonical.parent() != Some(allowed.as_path()) {
        return Err(format!(
            "feedback dialog is outside the allowed directory: '{}'",
            canonical.display()
        ));
    }
    if require_signature {
        let output = Command::new("/usr/bin/codesign")
            .args(["--verify", "--strict", "--"])
            .arg(&canonical)
            .output()
            .map_err(|error| format!("cannot verify feedback dialog signature: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "feedback dialog signature verification failed: {}",
                crate::utils::terminal::escape_terminal(
                    String::from_utf8_lossy(&output.stderr).trim()
                )
            ));
        }
    }
    Ok(())
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

    fn order_after(&self) -> &'static [&'static str] {
        &["PNGConverter"]
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
                return Err(isolated_context
                    .subprocess_timeout_diagnostic()
                    .map_or_else(
                        || "feedback dialog timed out".to_string(),
                        |partial| format!("feedback dialog timed out; {partial}"),
                    ));
            }
            PluginRunResult::Failed(error) => return Err(error),
            PluginRunResult::Panicked(message) => {
                return Err(format!(
                    "feedback subprocess supervisor panicked: {message}"
                ));
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
                crate::utils::terminal::escape_terminal(stderr.trim())
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

        let mut crash_report = report::load_report(json_path)?;
        crash_report.user_feedback = Some(serde_json::json!({ "comment": feedback }));
        crate::preprocessors::Sanitizer::new().sanitize_serializable(&mut crash_report)?;
        let json = serde_json::to_vec_pretty(&crash_report)
            .map_err(|error| format!("failed to serialize feedback report: {error}"))?;
        report::atomic_replace(json_path, &json)?;
        eprintln!("[monitor] Feedback saved to report");

        Ok(())
    }
}

#[cfg(test)]
#[path = "../../tests/unit/postprocessors/feedback_tests.rs"]
mod tests;
