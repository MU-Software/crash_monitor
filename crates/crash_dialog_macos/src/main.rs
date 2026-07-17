//! macOS native feedback dialog for crash/snapshot/ANR reports.
//!
//! Launched by `crash_monitor` as a separate process. Displays an NSAlert with
//! a text field for the tester to describe what they were doing. The feedback
//! text is written to **stdout** (nothing else). All logs go to stderr.
//!
//! Exit codes: 0 = feedback submitted, 1 = skipped/cancelled, 2 = error.

use clap::Parser;

mod dialog;

#[derive(Parser)]
#[command(name = "crash_dialog_macos")]
struct Args {
    /// Report type: "crash", "snapshot", or "anr".
    #[arg(long, rename_all = "verbatim")]
    r#type: String,

    /// Process name (e.g. "voxelcore_desktop").
    #[arg(long)]
    process: String,

    /// Timestamp string from the report header.
    #[arg(long)]
    timestamp: String,

    /// Test mode: supply feedback text directly without showing a dialog.
    #[arg(long)]
    mock_input: Option<String>,

    /// Test mode: validate arguments only, do not show dialog or produce output.
    #[arg(long)]
    dry_run: bool,
}

fn main() {
    let args = Args::parse();

    if args.dry_run {
        std::process::exit(0);
    }

    let feedback = if let Some(mock) = &args.mock_input {
        // Test mode: use provided text directly.
        if mock.trim().is_empty() {
            None
        } else {
            Some(mock.clone())
        }
    } else {
        // Production: show native dialog.
        match dialog::show_feedback_dialog(&args.r#type, &args.process, &args.timestamp) {
            Ok(text) => text,
            Err(e) => {
                eprintln!("[crash_dialog] {e}");
                std::process::exit(2);
            }
        }
    };

    match feedback {
        Some(text) if !text.trim().is_empty() => {
            print!("{text}");
            std::process::exit(0);
        }
        _ => {
            std::process::exit(1);
        }
    }
}
