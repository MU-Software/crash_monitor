//! macOS capture engine used by the thin `crash_monitor` binary.

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
pub mod event_loop;
pub mod event_source;
mod filters;
mod notifiers;
pub mod pipeline;
pub mod platform;
mod postprocessors;
mod preprocessors;
pub mod shm;
mod utils;
mod watchdog;

pub use collectors::ChildEnvironmentSnapshot;

/// Feature-gated fixtures for external integration tests.
///
/// These APIs are not a supported capture surface and are absent from normal
/// production builds.
#[cfg(feature = "test-support")]
#[doc(hidden)]
pub mod test_support {
    pub use crate::platform::mock::MockPlatform;
    pub use crate::postprocessors::{FeedbackPostProcessor, ZIPArchiver};

    pub mod capture {
        pub use crate::platform::macos::ffi::capture_spawn::spawn_capture_helper;
        pub use crate::platform::macos::ffi::types::OwnedThreadPort;
    }
}
