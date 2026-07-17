//! `mbb_monitor analyze <report.json>` — human-readable report summary.

use crate::pipeline::ReportType;
use crate::pipeline::report::{self, CrashReport};
use std::path::Path;

/// Maximum number of backtrace frames to display.
const MAX_FRAMES: usize = 20;
/// Maximum number of breadcrumbs to display.
const MAX_BREADCRUMBS: usize = 10;

/// Run the `analyze` subcommand. Returns exit code (0 = success, 1 = error).
#[must_use]
pub fn run(report_path: &str) -> i32 {
    let report = match report::load_report(Path::new(report_path)) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };

    print_summary(&report);
    0
}

fn print_summary(report: &CrashReport) {
    // 1. Header line
    print_header(report);

    // 2. Crash context
    print_crash_context(report);

    // 3. Session
    if let Some(ref session) = report.session {
        let duration = format_duration(session.duration_s);
        println!("Session: {duration} (id: {})", session.id);
    }

    // 4. Fingerprint
    if let Some(ref fp) = report.fingerprint {
        println!("Fingerprint: {fp}");
    }

    // 5. User feedback
    if let Some(ref fb) = report.user_feedback
        && let Some(comment) = fb.get("comment").and_then(serde_json::Value::as_str)
    {
        println!("User feedback: {comment}");
    }

    println!();

    // 6. Exception details
    print_exception(report);

    // 7. Crashed thread backtrace
    print_backtrace(report);

    // 8. Breadcrumbs
    print_breadcrumbs(report);

    // 9. Diagnostics
    print_diagnostics(report);
}

fn print_header(report: &CrashReport) {
    let h = &report.header;
    match h.report_type {
        ReportType::Crash => {
            let signal = report
                .exception
                .as_ref()
                .map_or("unknown signal", |e| e.signal.as_str());
            let fault = report
                .exception
                .as_ref()
                .map_or("unknown".into(), |e| e.fault_address.clone());
            println!(
                "Crash Report: {signal} at {fault}  (PID {}, {})",
                h.pid, h.process
            );
        }
        ReportType::Anr => {
            let dur = h.hang_duration_ms.unwrap_or(0);
            println!(
                "ANR Report: unresponsive for {dur}ms  (PID {}, {})",
                h.pid, h.process
            );
        }
        ReportType::Oom => {
            println!(
                "OOM Report: process terminated by OOM  (PID {}, {})",
                h.pid, h.process
            );
        }
        ReportType::Snapshot => {
            println!("Snapshot Report  (PID {}, {})", h.pid, h.process);
        }
    }
    println!("Time: {}", h.timestamp);
}

fn print_crash_context(report: &CrashReport) {
    let Some(ref ctx) = report.crash_context else {
        return;
    };
    let tool = ctx
        .get("active_tool")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let frame = ctx
        .get("frame_number")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let regions = ctx
        .get("region_count")
        .and_then(serde_json::Value::as_i64)
        .unwrap_or(0);
    println!("Context: tool={tool} | frame={frame} | regions={regions}");
}

fn print_exception(report: &CrashReport) {
    let Some(ref exc) = report.exception else {
        return;
    };
    println!("Exception:");
    println!("  Type:          {}", exc.exc_type);
    println!("  Code:          {}", exc.code);
    println!("  Subcode:       {}", exc.subcode);
    println!("  Signal:        {}", exc.signal);
    println!("  Fault address: {}", exc.fault_address);
    println!();
}

fn print_backtrace(report: &CrashReport) {
    // Find crashed thread, or fall back to thread 0
    let thread = report
        .threads
        .iter()
        .find(|t| t.crashed)
        .or_else(|| report.threads.first());

    let Some(thread) = thread else {
        println!("(no threads in report)");
        return;
    };

    let label = if thread.crashed { " [CRASHED]" } else { "" };
    let name = thread.name.as_deref().unwrap_or("unnamed");
    println!("Thread {} ({name}){label}:", thread.index);

    if thread.backtrace.is_empty() {
        println!("  (no backtrace)");
    }

    for (i, frame) in thread.backtrace.iter().take(MAX_FRAMES).enumerate() {
        let sym = frame.symbol.as_deref().unwrap_or(&frame.address);
        let offset_str = frame
            .offset
            .as_deref()
            .map_or(String::new(), |o| format!(" + {o}"));
        let image_str = frame.image.as_deref().map_or(String::new(), |img| {
            // Show only the filename, not the full path
            let short = img.rsplit('/').next().unwrap_or(img);
            format!("  [{short}]")
        });
        let source_str = match (&frame.file, frame.line) {
            (Some(f), Some(l)) => {
                let short = f.rsplit('/').next().unwrap_or(f);
                format!("  {short}:{l}")
            }
            _ => String::new(),
        };
        println!("  #{i:<3} {sym}{offset_str}{image_str}{source_str}");
    }

    let total = thread.backtrace.len();
    if total > MAX_FRAMES {
        println!("  ... ({} more frames)", total - MAX_FRAMES);
    }
    println!();
}

fn print_breadcrumbs(report: &CrashReport) {
    // Breadcrumb format (from report_formatter::format_breadcrumbs):
    //   { "time_ns", "thread", "cat", "sev", "file", "line", "msg" }
    let Some(ref crumbs_val) = report.breadcrumbs else {
        return;
    };
    let Some(crumbs) = crumbs_val.as_array() else {
        return;
    };
    if crumbs.is_empty() {
        return;
    }

    println!("Recent breadcrumbs:");

    // Show last N breadcrumbs
    let start = crumbs.len().saturating_sub(MAX_BREADCRUMBS);
    for crumb in crumbs.iter().skip(start) {
        let cat = crumb
            .get("cat")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("?");
        let file = crumb
            .get("file")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        let line = crumb
            .get("line")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        let msg = crumb
            .get("msg")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        if file.is_empty() {
            println!("  [{cat:<8}] {msg}");
        } else {
            println!("  [{cat:<8}] {file}:{line}  {msg}");
        }
    }
    println!();
}

fn print_diagnostics(report: &CrashReport) {
    // _diagnostics format (from report_formatter::build_diagnostics_json):
    //   { "pipeline_duration_ms": <u64>, "plugins": { "<name>": { "status", "duration_ms", ... } } }
    let Some(ref diag) = report.diagnostics else {
        return;
    };

    let total_ms = diag
        .get("pipeline_duration_ms")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);

    let plugins = diag.get("plugins").and_then(serde_json::Value::as_object);

    let mut ok_count = 0u32;
    let mut err_count = 0u32;
    let mut skip_count = 0u32;

    if let Some(plugins) = plugins {
        for (_name, entry) in plugins {
            match entry.get("status").and_then(serde_json::Value::as_str) {
                Some("ok") => ok_count += 1,
                Some("skipped") => skip_count += 1,
                Some("error") => err_count += 1,
                _ => {}
            }
        }
    }

    println!(
        "Pipeline: {ok_count} ok, {err_count} error, {skip_count} skipped  ({total_ms}ms total)"
    );
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
