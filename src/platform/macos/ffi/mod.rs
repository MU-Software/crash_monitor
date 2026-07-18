//! FFI bindings for macOS Mach kernel APIs.
//!
//! All `unsafe` Mach API calls are confined to this module.
//! The rest of the codebase calls these safe wrappers exclusively.
//!
//! **Dependency rule**: Code in this module may only import from:
//! - `crate::platform::macos::*` (parent: testable types and pure functions)
//! - `crate::platform::{...}` (shared typed platform contracts)
//! - `crate::shm::types::*` (generated SHM ABI constants/types, in `shm.rs` only)
//! - `super::*` (sibling FFI modules within ffi/)
//! - External crates (`std`, `mach2`, `nix`, `libc`, `uuid`)
//!
//! Importing from `crate::pipeline`, `crate::collectors`, etc. is
//! forbidden. This is enforced by `test_ffi_import_boundary` in the test suite.

pub mod capture_spawn;
pub mod exceptions;
pub mod memory;
pub mod shm;
pub mod spawn;
pub mod task;
pub mod thread;
pub mod types;

#[cfg(test)]
mod tests {
    /// Verify that FFI files only import from allowed sources.
    ///
    /// Allowed `use` patterns:
    /// - `use std::` / `use core::` — standard library
    /// - `use mach2::` / `use nix::` / `use libc` / `use uuid::` — external crates
    /// - `use crate::platform::macos::` — parent testable module
    /// - `use crate::platform::{` — shared typed platform contracts
    /// - `use crate::shm::types::` — generated SHM ABI used by `ffi/shm.rs`
    /// - `use super::` — sibling FFI modules
    ///
    /// Forbidden: any other `use crate::` path (pipeline, collectors, shm, etc.)
    #[test]
    fn test_ffi_import_boundary() {
        let ffi_dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/platform/macos/ffi");

        let mut violations = Vec::new();

        for entry in std::fs::read_dir(&ffi_dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            let filename = path.file_name().unwrap().to_str().unwrap();
            let content = std::fs::read_to_string(&path).unwrap();

            for (line_no, line) in content.lines().enumerate() {
                let trimmed = line.trim();

                // Skip non-use lines and comments
                if !trimmed.starts_with("use ") {
                    continue;
                }

                // Allowed patterns
                if trimmed.starts_with("use std::")
                    || trimmed.starts_with("use core::")
                    || trimmed.starts_with("use mach2::")
                    || trimmed.starts_with("use nix::")
                    || trimmed.starts_with("use libc")
                    || trimmed.starts_with("use uuid::")
                    || trimmed.starts_with("use super::")
                    || trimmed.starts_with("use crate::platform::macos::")
                    || trimmed.starts_with("use crate::platform::{")
                    || trimmed.starts_with("use crate::platform::ArmThreadState64")
                    || trimmed.starts_with("use crate::shm::types::")
                {
                    continue;
                }

                violations.push(format!("  ffi/{filename}:{}: {trimmed}", line_no + 1));
            }
        }

        assert!(
            violations.is_empty(),
            "FFI import boundary violated — ffi/ files may only import from \
             std/core, mach2/nix/libc/uuid, super::, crate::platform::macos::, \
             shared crate::platform contracts, or crate::shm::types::\n\
             Violations:\n{}",
            violations.join("\n")
        );
    }
}
