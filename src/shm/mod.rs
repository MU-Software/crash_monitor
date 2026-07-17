//! POSIX shared memory bridge between C app and Rust crash monitor.
//!
//! The monitor creates a shared memory region before spawning the child.
//! The child maps it and writes breadcrumbs, crash context, and screenshots.
//! On crash/snapshot, the monitor copies the complete mapped region into an
//! immutable owned snapshot while the child is suspended. Payload parsers and
//! Stage 1 persistence consume only those owned bytes after that boundary;
//! only the watchdog heartbeat remains a live acquire-atomic read.
//!
//! All `#[repr(C)]` structs mirror the C definitions in `sut_crash_reporter.h`.
//! Layout correctness is enforced by compile-time size assertions and CI tests.

pub mod reader;
pub mod types;

pub use reader::*;
pub use types::*;
