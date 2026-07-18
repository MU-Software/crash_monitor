//! `crash_monitor` library crate — exposes public modules for integration tests.

#[cfg(not(target_os = "macos"))]
compile_error!("crash_monitor requires macOS (Mach kernel APIs)");
#[cfg(not(target_arch = "aarch64"))]
compile_error!(
    "crash_monitor currently supports native arm64 macOS only; x86_64 and Rosetta are unsupported"
);

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
