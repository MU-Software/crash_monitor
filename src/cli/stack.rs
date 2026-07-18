//! `crash_monitor stack <report.json> --thread <N>` — hex dump of stack memory.

use crate::pipeline::report;
use crate::utils::terminal::escape_terminal;
use base64::Engine;
use std::io::{self, Write};
use std::path::Path;

/// Maximum decoded stack size before truncation (16 MB).
const MAX_DECODED_SIZE: usize = 16 * 1024 * 1024;

/// Run the `stack` subcommand. Returns exit code (0 = success, 1 = error).
#[must_use]
pub fn run(report_path: &str, thread_index: u32) -> i32 {
    let stdout = io::stdout();
    let stderr = io::stderr();
    run_with_writers(
        report_path,
        thread_index,
        &mut stdout.lock(),
        &mut stderr.lock(),
    )
}

fn run_with_writers(
    report_path: &str,
    thread_index: u32,
    output: &mut dyn Write,
    errors: &mut dyn Write,
) -> i32 {
    let report = match report::load_report(Path::new(report_path)) {
        Ok(r) => r,
        Err(e) => {
            let _ = writeln!(errors, "error: {e}");
            return 1;
        }
    };

    let thread_count = report.threads.len();
    if thread_count == 0 {
        let _ = writeln!(errors, "error: report contains no threads");
        return 1;
    }
    let Some(thread) = report.threads.get(thread_index as usize) else {
        let _ = writeln!(
            errors,
            "error: thread index {thread_index} out of range (report has {thread_count} threads: 0..{})",
            thread_count.saturating_sub(1)
        );
        return 1;
    };

    let Some(ref stack) = thread.stack_memory else {
        let _ = writeln!(
            output,
            "No stack memory captured for thread {thread_index} ({})",
            escape_terminal(thread.name.as_deref().unwrap_or("unnamed"))
        );
        return 0;
    };

    let bytes = match base64::engine::general_purpose::STANDARD.decode(&stack.hex_dump) {
        Ok(b) => b,
        Err(e) => {
            let _ = writeln!(errors, "error: base64 decode failed: {e}");
            return 1;
        }
    };

    let truncated = bytes.len() > MAX_DECODED_SIZE;
    let display_bytes = if truncated {
        &bytes[..MAX_DECODED_SIZE]
    } else {
        &bytes
    };

    if writeln!(
        output,
        "Stack memory for thread {thread_index} (SP: {}, {} decoded bytes)",
        escape_terminal(&stack.sp),
        bytes.len()
    )
    .and_then(|()| {
        if stack.size == bytes.len() as u64 {
            Ok(())
        } else {
            writeln!(
                output,
                "warning: declared stack size {} does not match decoded length {}",
                stack.size,
                bytes.len()
            )
        }
    })
    .and_then(|()| writeln!(output))
    .and_then(|()| print_hexdump(display_bytes, output))
    .is_err()
    {
        let _ = writeln!(errors, "error: failed to write stack output");
        return 1;
    }

    if truncated {
        let _ = writeln!(
            output,
            "\n(truncated at {} bytes, total {} bytes)",
            MAX_DECODED_SIZE,
            bytes.len()
        );
    }

    0
}

/// Print bytes in classic hexdump format (16 bytes per line with ASCII sidebar).
///
/// # Errors
///
/// Returns the sink error when the output cannot be written.
pub fn print_hexdump(data: &[u8], output: &mut dyn Write) -> io::Result<()> {
    format_hexdump(data, output)
}

/// Stream bytes in classic hexdump format without materializing the full dump.
///
/// # Errors
///
/// Returns the sink error when any hexdump line cannot be written.
pub fn format_hexdump(data: &[u8], out: &mut dyn Write) -> io::Result<()> {
    for (offset, chunk) in data.chunks(16).enumerate() {
        // Offset
        write!(out, "{:08x}  ", offset * 16)?;

        // Hex bytes (two groups of 8)
        for (i, byte) in chunk.iter().enumerate() {
            write!(out, "{byte:02x} ")?;
            if i == 7 {
                write!(out, " ")?;
            }
        }
        // Pad remaining columns if chunk < 16
        let missing = 16 - chunk.len();
        for i in 0..missing {
            write!(out, "   ")?;
            if chunk.len() + i == 7 {
                write!(out, " ")?;
            }
        }

        // ASCII sidebar
        write!(out, " |")?;
        for &byte in chunk {
            if (0x20..=0x7E).contains(&byte) {
                write!(out, "{}", byte as char)?;
            } else {
                write!(out, ".")?;
            }
        }
        writeln!(out, "|")?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/unit/cli/stack_tests.rs"]
mod tests;
