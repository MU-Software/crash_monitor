//! Shared command-line, output, and exit-status contract for feedback helpers.

pub use clap::Parser;
use std::process::ExitCode;

pub const EXIT_SUBMITTED: u8 = 0;
pub const EXIT_SKIPPED: u8 = 1;
pub const EXIT_ERROR: u8 = 2;

/// Arguments accepted by both the `AppKit` helper and its headless test double.
#[derive(Debug, Clone, Parser, PartialEq, Eq)]
pub struct DialogArgs {
    /// Report type, for example `crash`, `snapshot`, or `anr`.
    #[arg(long, rename_all = "verbatim")]
    pub r#type: String,

    /// Name of the process that produced the report.
    #[arg(long)]
    pub process: String,

    /// Timestamp string from the report header.
    #[arg(long)]
    pub timestamp: String,

    /// Supply feedback without presenting UI. Intended for tests and automation.
    #[arg(long)]
    pub mock_input: Option<String>,

    /// Validate arguments only; write no stdout and do not present UI.
    #[arg(long)]
    pub dry_run: bool,
}

/// Result communicated to the monitor: UTF-8 text on stdout only when submitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogOutcome {
    Submitted(String),
    Skipped,
    Error(String),
}

impl DialogOutcome {
    #[must_use]
    pub fn from_optional_text(text: Option<String>) -> Self {
        match text {
            Some(text) if !text.trim().is_empty() => Self::Submitted(text),
            _ => Self::Skipped,
        }
    }

    /// Emit the stable helper protocol and return its documented exit status.
    #[must_use]
    pub fn emit(self) -> ExitCode {
        match self {
            Self::Submitted(text) => {
                print!("{text}");
                ExitCode::from(EXIT_SUBMITTED)
            }
            Self::Skipped => ExitCode::from(EXIT_SKIPPED),
            Self::Error(message) => {
                eprintln!("[crash_dialog] {message}");
                ExitCode::from(EXIT_ERROR)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_test_switches_are_part_of_the_shared_parser() {
        let args = DialogArgs::try_parse_from([
            "dialog",
            "--type",
            "crash",
            "--process",
            "demo",
            "--timestamp",
            "now",
            "--mock-input",
            "details",
            "--dry-run",
        ])
        .unwrap();
        assert_eq!(args.mock_input.as_deref(), Some("details"));
        assert!(args.dry_run);
    }

    #[test]
    fn blank_feedback_has_skip_semantics() {
        assert_eq!(
            DialogOutcome::from_optional_text(Some("  \n".into())),
            DialogOutcome::Skipped
        );
    }
}
