//! CLI analysis subcommands (Phase 5).
//!
//! Offline tools for inspecting crash/snapshot reports:
//! - `analyze`: human-readable summary
//! - `stack`: hex dump of a thread's stack memory
//! - `symbolicate`: resolve addresses to source `file:line` via dSYM

pub mod analyze;
pub mod stack;
pub mod symbolicate;
