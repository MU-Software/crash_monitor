//! POSIX shared memory bridge between C app and Rust crash monitor.
//!
//! The monitor creates a shared memory region before spawning the child.
//! The child maps it and writes breadcrumbs, crash context, and screenshots.
//! On crash/snapshot, the monitor copies only privacy-authorized sections into
//! a fixed-layout immutable owned snapshot while the child is suspended.
//! Payload parsers and explicitly enabled Stage 1 persistence consume only
//! those owned bytes after that boundary;
//! only publication words plus the watchdog readiness/heartbeat handshake are
//! read from the live mapping, always through aligned acquire atomics.
//!
//! All C ABI structs are generated from `schema/crash_shm.h`. Layout
//! correctness is enforced by compile-time size/offset assertions and tests.

pub mod reader;
pub mod types;

pub use reader::*;
pub use types::*;
