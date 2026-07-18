//! Platform-neutral offline report CLI.

use base64::Engine as _;
use clap::{Parser, Subcommand};
use crash_report_core::{escape_terminal, load_report};
use std::io::Write;
use std::path::Path;

#[derive(Parser)]
#[command(name = "crash-report", about = "Inspect crash-monitor reports offline")]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    Analyze {
        report: String,
    },
    Stack {
        report: String,
        #[arg(long)]
        thread: usize,
    },
}

pub fn run_from(arguments: impl IntoIterator<Item = String>) -> i32 {
    let cli = match Cli::try_parse_from(arguments) {
        Ok(cli) => cli,
        Err(error) => {
            let _ = error.print();
            return 2;
        }
    };
    let result = match cli.command {
        Command::Analyze { report } => analyze(Path::new(&report), &mut std::io::stdout()),
        Command::Stack { report, thread } => {
            stack(Path::new(&report), thread, &mut std::io::stdout())
        }
    };
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("error: {error}");
            1
        }
    }
}

/// Print a compact report summary.
///
/// # Errors
/// Returns an error when the report cannot be loaded or output cannot be written.
pub fn analyze(path: &Path, output: &mut dyn Write) -> Result<(), String> {
    let report = load_report(path).map_err(|error| error.to_string())?;
    writeln!(
        output,
        "{} report: {} (PID {})",
        escape_terminal(report.report_type().unwrap_or("unknown")),
        escape_terminal(report.process().unwrap_or("unknown")),
        report.pid().unwrap_or(0)
    )
    .map_err(|error| error.to_string())
}

/// Stream a selected thread's captured stack bytes as a hex dump.
///
/// # Errors
/// Returns an error for invalid reports, thread indices, stack data, or output.
pub fn stack(path: &Path, thread: usize, output: &mut dyn Write) -> Result<(), String> {
    let report = load_report(path).map_err(|error| error.to_string())?;
    let thread = report
        .thread(thread)
        .ok_or_else(|| "thread index is out of range".to_string())?;
    let encoded = thread
        .pointer("/stack_memory/data")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "thread has no stack memory".to_string())?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|error| format!("invalid stack base64: {error}"))?;
    for (offset, chunk) in bytes.chunks(16).enumerate() {
        write!(output, "{:08x}:", offset * 16).map_err(|error| error.to_string())?;
        for byte in chunk {
            write!(output, " {byte:02x}").map_err(|error| error.to_string())?;
        }
        writeln!(output).map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> (tempfile::TempDir, std::path::PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("report.json");
        std::fs::write(
            &path,
            r#"{
                "header":{"type":"crash","pid":42,"process":"app"},
                "threads":[{"stack_memory":{"data":"AQID"}}]
            }"#,
        )
        .unwrap();
        (directory, path)
    }

    #[test]
    fn analyze_and_stack_use_only_portable_report_core() {
        let (_directory, path) = report();
        let mut summary = Vec::new();
        analyze(&path, &mut summary).unwrap();
        assert_eq!(
            String::from_utf8(summary).unwrap(),
            "crash report: app (PID 42)\n"
        );

        let mut dump = Vec::new();
        stack(&path, 0, &mut dump).unwrap();
        assert_eq!(String::from_utf8(dump).unwrap(), "00000000: 01 02 03\n");
    }

    #[test]
    fn analyze_escapes_untrusted_header_fields_without_forging_lines() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("report.json");
        std::fs::write(
            &path,
            r#"{
                "header":{
                    "type":"crash\nforged\tentry",
                    "pid":42,
                    "process":"app\u001b[2J\rspoof"
                },
                "threads":[]
            }"#,
        )
        .unwrap();

        let mut summary = Vec::new();
        analyze(&path, &mut summary).unwrap();

        assert_eq!(
            String::from_utf8(summary).unwrap(),
            "crash\\nforged\\tentry report: app\\x1b[2J\\rspoof (PID 42)\n"
        );
    }
}
