//! macOS native feedback dialog for crash/snapshot/ANR reports.
//!
//! Launched by `crash_monitor` as a separate process. Displays an `NSAlert` with
//! a text field for the tester to describe what they were doing. The feedback
//! text is written to **stdout** (nothing else). All logs go to stderr.
//!
//! Exit codes: 0 = feedback submitted, 1 = skipped/cancelled, 2 = error.

use crash_dialog_contract::{DialogArgs, DialogOutcome, Parser};
use std::process::ExitCode;

mod dialog;

fn main() -> ExitCode {
    let args = DialogArgs::parse();

    if args.dry_run {
        return ExitCode::SUCCESS;
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
            Err(error) => return DialogOutcome::Error(error).emit(),
        }
    };

    DialogOutcome::from_optional_text(feedback).emit()
}
