//! Mock feedback dialog for E2E tests.
//!
//! Accepts the same CLI arguments as `mbb_crash_dialog_macos` but never shows
//! any UI. Outputs a fixed feedback string to stdout and exits 0, exercising
//! the full `FeedbackPostProcessor` pipeline (spawn → read stdout → patch JSON)
//! without user interaction.

use clap::Parser;

#[derive(Parser)]
#[command(name = "mbb_crash_dialog_mock")]
struct Args {
    #[arg(long, rename_all = "verbatim")]
    r#type: String,

    #[arg(long)]
    process: String,

    #[arg(long)]
    timestamp: String,
}

fn main() {
    let _args = Args::parse();
    print!("e2e-mock-feedback");
}
