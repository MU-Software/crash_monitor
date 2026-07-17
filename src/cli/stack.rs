//! `mbb_monitor stack <report.json> --thread <N>` — hex dump of stack memory.

use crate::pipeline::report;
use std::path::Path;

/// Maximum decoded stack size before truncation (16 MB).
const MAX_DECODED_SIZE: usize = 16 * 1024 * 1024;

/// Run the `stack` subcommand. Returns exit code (0 = success, 1 = error).
#[must_use]
pub fn run(report_path: &str, thread_index: u32) -> i32 {
    let report = match report::load_report(Path::new(report_path)) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    };

    let thread_count = report.threads.len();
    let Some(thread) = report.threads.get(thread_index as usize) else {
        eprintln!(
            "error: thread index {thread_index} out of range (report has {thread_count} threads: 0..{})",
            thread_count.saturating_sub(1)
        );
        return 1;
    };

    let Some(ref stack) = thread.stack_memory else {
        println!(
            "No stack memory captured for thread {thread_index} ({})",
            thread.name.as_deref().unwrap_or("unnamed")
        );
        return 0;
    };

    let bytes = match base64::engine::general_purpose::STANDARD.decode(&stack.hex_dump) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error: base64 decode failed: {e}");
            return 1;
        }
    };

    let truncated = bytes.len() > MAX_DECODED_SIZE;
    let display_bytes = if truncated {
        &bytes[..MAX_DECODED_SIZE]
    } else {
        &bytes
    };

    println!(
        "Stack memory for thread {thread_index} (SP: {}, {} bytes)",
        stack.sp, stack.size
    );
    println!();
    print_hexdump(display_bytes);

    if truncated {
        println!(
            "\n(truncated at {} bytes, total {} bytes)",
            MAX_DECODED_SIZE,
            bytes.len()
        );
    }

    0
}

/// Print bytes in classic hexdump format (16 bytes per line with ASCII sidebar).
pub fn print_hexdump(data: &[u8]) {
    let mut buf = String::new();
    format_hexdump(data, &mut buf);
    print!("{buf}");
}

/// Format bytes into a hexdump string.
pub fn format_hexdump(data: &[u8], out: &mut String) {
    use std::fmt::Write;

    for (offset, chunk) in data.chunks(16).enumerate() {
        // Offset
        let _ = write!(out, "{:08x}  ", offset * 16);

        // Hex bytes (two groups of 8)
        for (i, byte) in chunk.iter().enumerate() {
            let _ = write!(out, "{byte:02x} ");
            if i == 7 {
                out.push(' ');
            }
        }
        // Pad remaining columns if chunk < 16
        let missing = 16 - chunk.len();
        for i in 0..missing {
            out.push_str("   ");
            if chunk.len() + i == 7 {
                out.push(' ');
            }
        }

        // ASCII sidebar
        out.push_str(" |");
        for &byte in chunk {
            if (0x20..=0x7E).contains(&byte) {
                out.push(byte as char);
            } else {
                out.push('.');
            }
        }
        out.push('|');
        out.push('\n');
    }
}

use base64::Engine;

#[cfg(test)]
#[path = "../../tests/unit/cli/stack_tests.rs"]
mod tests;
