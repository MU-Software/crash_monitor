//! Mock feedback dialog for E2E tests.
//!
//! Accepts the same CLI arguments as `crash_dialog_macos` but never shows
//! any UI. Outputs a fixed feedback string to stdout and exits 0, exercising
//! the full `FeedbackPostProcessor` pipeline (spawn → read stdout → patch JSON)
//! without user interaction.

use crash_dialog_contract::{DialogArgs, DialogOutcome, Parser};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args = DialogArgs::parse();
    if args.dry_run {
        return ExitCode::SUCCESS;
    }
    let feedback = args
        .mock_input
        .or_else(|| Some("e2e-mock-feedback".to_string()));
    DialogOutcome::from_optional_text(feedback).emit()
}
