//! `crash_monitor` library crate — exposes public modules for integration tests.

#[cfg(not(target_os = "macos"))]
compile_error!("crash_monitor requires macOS (Mach kernel APIs)");

pub mod cli;
mod collectors;
pub mod config;
pub mod event_loop;
pub mod event_source;
pub mod filters;
pub mod notifiers;
pub mod pipeline;
pub mod platform;
pub mod postprocessors;
mod preprocessors;
pub mod shm;
mod utils;
mod watchdog;
