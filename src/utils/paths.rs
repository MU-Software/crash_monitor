//! Shared path utilities for crash reporter data directories.

use std::fs;
use std::path::PathBuf;

/// Environment variable to override the base data directory.
///
/// Set by `tools/crash_monitor/.cargo/config.toml` for `cargo test`/`cargo run`
/// invocations so that any code path which falls back to `pending_dir()`
/// (e.g. tests with `output_dir: None`) writes to a sandbox under `target/`
/// instead of polluting the user's `~/.modelblockbuilder/`.
///
/// Production builds (the codesigned binary launched by `make desktop-monitor-run`)
/// do NOT set this variable, so they continue to use `~/.modelblockbuilder/`.
const DATA_DIR_OVERRIDE_ENV: &str = "MBB_CRASH_DATA_DIR";

/// Base directory for crash reporter data: `~/.modelblockbuilder/` (or
/// `$MBB_CRASH_DATA_DIR` if set, e.g. during `cargo test`).
pub fn data_dir() -> Result<PathBuf, String> {
    let dir = if let Ok(override_path) = std::env::var(DATA_DIR_OVERRIDE_ENV) {
        if override_path.is_empty() {
            return Err(format!("{DATA_DIR_OVERRIDE_ENV} is set but empty"));
        }
        PathBuf::from(override_path)
    } else {
        let home = std::env::var("HOME").map_err(|_| "HOME not set".to_string())?;
        PathBuf::from(home).join(".modelblockbuilder")
    };
    fs::create_dir_all(&dir).map_err(|e| format!("Failed to create data dir: {e}"))?;
    Ok(dir)
}

/// Working directory for in-flight reports: `<data_dir>/crashes/pending/`.
/// The pipeline writes Stage 1 raw dumps, Stage 2 JSON, and intermediate
/// files here. The `MoveToSent` post-processor relocates finished reports
/// to `sent_dir()`.
pub fn pending_dir() -> Result<PathBuf, String> {
    let dir = data_dir()?.join("crashes").join("pending");
    fs::create_dir_all(&dir).map_err(|e| format!("Failed to create pending dir: {e}"))?;
    Ok(dir)
}

/// Archive directory for completed reports: `<data_dir>/crashes/sent/`.
/// `MoveToSent` populates it after the post-processor chain finishes, and
/// `RetentionManager` prunes it by count/size/age.
pub fn sent_dir() -> Result<PathBuf, String> {
    let dir = data_dir()?.join("crashes").join("sent");
    fs::create_dir_all(&dir).map_err(|e| format!("Failed to create sent dir: {e}"))?;
    Ok(dir)
}

/// Given a pending directory path, return the sibling sent directory:
/// `<parent>/sent/`. Used by `Pipeline.output_dir` overrides so tests can
/// substitute a tempdir-rooted layout without touching `data_dir()`.
#[must_use]
pub fn sent_dir_for(pending: &std::path::Path) -> PathBuf {
    pending
        .parent()
        .map_or_else(|| pending.join("sent"), |parent| parent.join("sent"))
}

#[cfg(test)]
#[path = "../../tests/unit/utils/paths_tests.rs"]
mod tests;
