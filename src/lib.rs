//! `crash_monitor` library crate — exposes public modules for integration tests.

macro_rules! eprintln {
    ($($arg:tt)*) => {
        tracing::event!(target: "crash_monitor::operational", tracing::Level::WARN, message = %format_args!($($arg)*))
    };
}

#[cfg(not(target_os = "macos"))]
compile_error!("crash_monitor requires macOS (Mach kernel APIs)");
#[cfg(not(target_arch = "aarch64"))]
compile_error!(
    "crash_monitor currently supports native arm64 macOS only; x86_64 and Rosetta are unsupported"
);

#[cfg(panic = "abort")]
compile_error!(
    "crash_monitor requires panic=unwind because cooperative plugin isolation uses catch_unwind"
);

pub mod cli;
mod collectors;
pub mod config;
pub mod errors;
pub mod event_loop;
pub mod event_source;
pub mod filters;
pub mod notifiers;
pub mod pipeline;
pub mod platform;
pub mod postprocessors;
mod preprocessors;
pub mod shm;
pub mod telemetry;
mod utils;
mod watchdog;
