//! `crash_monitor analyze <report.json>` — human-readable report summary.

use crate::pipeline::report::{self, CrashReport};
use crate::pipeline::{ReportType, TerminationReason};
use crate::utils::terminal::escape_terminal;
use std::io::{self, Write};
use std::path::Path;

/// Maximum number of backtrace frames to display.
const MAX_FRAMES: usize = 20;
/// Maximum number of breadcrumbs to display.
const MAX_BREADCRUMBS: usize = 10;

/// Run the `analyze` subcommand. Returns exit code (0 = success, 1 = error).
#[must_use]
pub fn run(report_path: &str) -> i32 {
    let stdout = io::stdout();
    let stderr = io::stderr();
    run_with_writers(report_path, &mut stdout.lock(), &mut stderr.lock())
}

fn run_with_writers(report_path: &str, output: &mut dyn Write, errors: &mut dyn Write) -> i32 {
    let report = match report::load_report(Path::new(report_path)) {
        Ok(r) => r,
        Err(e) => {
            let _ = writeln!(errors, "error: {e}");
            return 1;
        }
    };

    match write_summary(&report, output) {
        Ok(()) => 0,
        Err(error) => {
            let _ = writeln!(errors, "error: failed to write analysis: {error}");
            1
        }
    }
}

fn write_summary(report: &CrashReport, output: &mut dyn Write) -> io::Result<()> {
    // 1. Header line
    print_header(report, output)?;

    // 2. Crash context
    print_crash_context(report, output)?;

    // 3. Session
    if let Some(ref session) = report.session {
        let duration = format_duration(session.duration_s);
        writeln!(
            output,
            "Session: {duration} (id: {})",
            escape_terminal(&session.id)
        )?;
    }

    // 4. Fingerprint
    if let Some(ref fp) = report.fingerprint {
        writeln!(output, "Fingerprint: {}", escape_terminal(fp))?;
    }

    // 5. User feedback
    if let Some(ref feedback) = report.user_feedback {
        let comment = render_json_field(feedback.get("comment"));
        writeln!(output, "User feedback: {comment}")?;
    }

    writeln!(output)?;

    // 6. Exception details
    print_exception(report, output)?;

    // 7. Crashed thread backtrace
    print_backtrace(report, output)?;

    // 8. Breadcrumbs
    print_breadcrumbs(report, output)?;

    // 9. Diagnostics
    print_diagnostics(report, output)?;
    Ok(())
}

fn print_header(report: &CrashReport, output: &mut dyn Write) -> io::Result<()> {
    writeln!(output, "{}", header_summary(report))?;
    writeln!(
        output,
        "Time: {}",
        escape_terminal(&report.header.timestamp)
    )
}

fn header_summary(report: &CrashReport) -> String {
    let h = &report.header;
    let process = escape_terminal(&h.process);
    match h.report_type {
        ReportType::Crash => {
            let signal = report
                .exception
                .as_ref()
                .map_or("unknown signal", |e| e.signal.as_str());
            let fault = report
                .exception
                .as_ref()
                .and_then(|e| e.fault_address.clone())
                .unwrap_or_else(|| "not applicable".into());
            format!(
                "Crash Report: {signal} at {fault}  (PID {}, {})",
                h.pid, process
            )
        }
        ReportType::Anr => {
            let dur = h.hang_duration_ms.unwrap_or(0);
            format!(
                "ANR Report: unresponsive for {dur}ms  (PID {}, {})",
                h.pid, process
            )
        }
        ReportType::Oom => format!(
            "Possible OOM Report: SIGKILL observed without confirming memory-pressure evidence  (PID {}, {})",
            h.pid, process
        ),
        ReportType::Snapshot => format!("Snapshot Report  (PID {}, {process})", h.pid),
        ReportType::ExitFailure => match report.termination {
            Some(TerminationReason::Exited {
                exit_code,
                runtime_ms,
            }) => format!(
                "Exit Failure Report: exit code {exit_code} after {runtime_ms}ms  (PID {}, {})",
                h.pid, process
            ),
            _ => format!("Exit Failure Report  (PID {}, {process})", h.pid),
        },
        ReportType::SignalFailure => match report.termination {
            Some(TerminationReason::Signaled {
                signal,
                core_dumped,
                runtime_ms,
            }) => {
                let core = if core_dumped { ", core dumped" } else { "" };
                let cause = if signal == 9 {
                    ", cause unknown (SIGKILL is not proof of OOM)"
                } else {
                    ""
                };
                format!(
                    "Signal Failure Report: signal {signal}{core}{cause} after {runtime_ms}ms  (PID {}, {})",
                    h.pid, process
                )
            }
            _ => format!("Signal Failure Report  (PID {}, {process})", h.pid),
        },
    }
}

fn print_crash_context(report: &CrashReport, output: &mut dyn Write) -> io::Result<()> {
    let Some(ref ctx) = report.crash_context else {
        writeln!(output, "Context: <missing>")?;
        return Ok(());
    };
    // App state is a generic annotation map — print it verbatim (no app-specific
    // field names, so the tool stays domain-agnostic).
    if let Some(session_id) = &ctx.session_id {
        let start = ctx
            .session_start_ns
            .map_or_else(|| "unknown".to_string(), |value| value.to_string());
        writeln!(
            output,
            "Producer session: {} (start_ns: {start}, heartbeat: {})",
            escape_terminal(session_id),
            ctx.heartbeat_counter
        )?;
    } else {
        writeln!(output, "Producer heartbeat: {}", ctx.heartbeat_counter)?;
    }

    if ctx.annotations.is_empty() {
        writeln!(output, "Context annotations: <missing>")?;
        return Ok(());
    }
    let joined = ctx
        .annotations
        .iter()
        .map(|(key, value)| {
            format!(
                "{}={}",
                escape_terminal(key),
                render_json_field(Some(value))
            )
        })
        .collect::<Vec<_>>()
        .join(" | ");
    writeln!(output, "Context: {joined}")
}

fn render_json_field(value: Option<&serde_json::Value>) -> String {
    match value {
        None => "<missing>".to_string(),
        Some(serde_json::Value::String(value)) => escape_terminal(value),
        Some(value) => escape_terminal(&value.to_string()),
    }
}

fn print_exception(report: &CrashReport, output: &mut dyn Write) -> io::Result<()> {
    let Some(ref exc) = report.exception else {
        return Ok(());
    };
    writeln!(output, "Exception:")?;
    writeln!(
        output,
        "  Type:          {}",
        escape_terminal(&exc.exc_type)
    )?;
    writeln!(output, "  Code:          {}", escape_terminal(&exc.code))?;
    writeln!(output, "  Subcode:       {}", escape_terminal(&exc.subcode))?;
    writeln!(output, "  Signal:        {}", escape_terminal(&exc.signal))?;
    if let Some(fault_address) = &exc.fault_address {
        writeln!(
            output,
            "  Fault address: {}",
            escape_terminal(fault_address)
        )?;
    }
    if exc.signal_is_approximate {
        writeln!(output, "  Signal mapping: approximate")?;
    }
    writeln!(output)
}

fn print_backtrace(report: &CrashReport, output: &mut dyn Write) -> io::Result<()> {
    // Find crashed thread, or fall back to thread 0
    let thread = report
        .threads
        .iter()
        .find(|t| t.crashed)
        .or_else(|| report.threads.first());

    let Some(thread) = thread else {
        writeln!(output, "(no threads in report)")?;
        return Ok(());
    };

    let label = if thread.crashed { " [CRASHED]" } else { "" };
    let name = escape_terminal(thread.name.as_deref().unwrap_or("unnamed"));
    writeln!(output, "Thread {} ({name}){label}:", thread.index)?;

    if thread.backtrace.is_empty() {
        writeln!(output, "  (no backtrace)")?;
    }

    for (i, frame) in thread.backtrace.iter().take(MAX_FRAMES).enumerate() {
        let sym = escape_terminal(frame.symbol.as_deref().unwrap_or(&frame.address));
        let offset_str = frame
            .offset
            .as_deref()
            .map_or(String::new(), |o| format!(" + {}", escape_terminal(o)));
        let image_str = frame.image.as_deref().map_or(String::new(), |img| {
            // Show only the filename, not the full path
            let short = img.rsplit('/').next().unwrap_or(img);
            format!("  [{}]", escape_terminal(short))
        });
        let source_str = match (&frame.file, frame.line) {
            (Some(f), Some(l)) => {
                let short = f.rsplit('/').next().unwrap_or(f);
                format!("  {}:{l}", escape_terminal(short))
            }
            _ => String::new(),
        };
        writeln!(output, "  #{i:<3} {sym}{offset_str}{image_str}{source_str}")?;
    }

    let total = thread.backtrace.len();
    if total > MAX_FRAMES {
        writeln!(output, "  ... ({} more frames)", total - MAX_FRAMES)?;
    }
    writeln!(output)
}

fn print_breadcrumbs(report: &CrashReport, output: &mut dyn Write) -> io::Result<()> {
    // Breadcrumb format (from report_formatter::format_breadcrumbs):
    //   { "time_ns", "thread", "cat", "sev", "file", "line", "msg" }
    let Some(ref crumbs) = report.breadcrumbs else {
        return Ok(());
    };
    if crumbs.is_empty() {
        return Ok(());
    }

    writeln!(output, "Recent breadcrumbs:")?;

    // Show last N breadcrumbs
    let start = crumbs.len().saturating_sub(MAX_BREADCRUMBS);
    for crumb in crumbs.iter().skip(start) {
        let cat = escape_terminal(&crumb.cat);
        let file = escape_terminal(&crumb.file);
        let msg = escape_terminal(&crumb.msg);
        if file.is_empty() {
            writeln!(output, "  [{cat:<8}] {msg}")?;
        } else {
            writeln!(output, "  [{:<8}] {}:{}  {}", cat, file, crumb.line, msg)?;
        }
    }
    writeln!(output)
}

fn print_diagnostics(report: &CrashReport, output: &mut dyn Write) -> io::Result<()> {
    // _diagnostics format (from report_formatter::build_diagnostics_json):
    //   { "pipeline_duration_ms": <u64>, "plugins": { "<name>": { "status", "duration_ms", ... } } }
    if let Some(summary) = diagnostics_summary(report) {
        writeln!(output, "{summary}")?;
    }
    Ok(())
}

fn diagnostics_summary(report: &CrashReport) -> Option<String> {
    let diag = report.diagnostics.as_ref()?;

    let total_ms = diag
        .get("pipeline_duration_ms")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);

    let plugins = diag.get("plugins").and_then(serde_json::Value::as_object);

    let mut ok_count = 0u32;
    let mut err_count = 0u32;
    let mut timeout_count = 0u32;
    let mut skip_count = 0u32;

    if let Some(plugins) = plugins {
        for (_name, entry) in plugins {
            match entry.get("status").and_then(serde_json::Value::as_str) {
                Some("ok") => ok_count += 1,
                Some("skipped") => skip_count += 1,
                Some("error") => err_count += 1,
                Some("timed_out") => timeout_count += 1,
                _ => {}
            }
        }
    }

    Some(format!(
        "Pipeline: {ok_count} ok, {err_count} error, {timeout_count} timed out, {skip_count} skipped  ({total_ms}ms total)"
    ))
}

fn format_duration(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}h {m:02}m")
    } else if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

#[cfg(test)]
#[path = "../../tests/unit/cli/analyze_tests.rs"]
mod tests;
