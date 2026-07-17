//! Shared path utilities for crash reporter data directories.

use std::fs;
use std::path::PathBuf;

/// Environment variable that a host project sets to choose the base data
/// directory (where crash reports, sessions, and locks live).
///
/// This is the project-configuration point: a project embedding the crash
/// monitor points it at its own namespace (e.g. `~/.myapp`) by exporting
/// `CRASH_MONITOR_DATA_DIR` before launching the monitor — the same value is
/// inherited by the monitored child, so the C reporter and the Rust monitor
/// agree on one location. It is also set by `tools/crash_monitor/.cargo/config.toml`
/// during `cargo test`/`cargo run` to a sandbox under `target/` so tests never
/// touch the real data directory.
///
/// When unset, both sides fall back to the tool default `~/.crash_monitor/`.
const DATA_DIR_OVERRIDE_ENV: &str = "CRASH_MONITOR_DATA_DIR";

/// Base directory name under `$HOME` when the override env is unset.
///
/// A host project bakes its own namespace here at build time by setting the
/// `CRASH_MONITOR_DATA_DIR_NAME` env when compiling (see `build.rs`, which marks
/// it as a rebuild trigger). When unbaked — the generic standalone tool build —
/// this is `.crash_monitor`. Only the dir *name* is baked; it resolves against
/// `$HOME` at runtime, so the binary carries no build-machine path and stays
/// safe to distribute.
const DEFAULT_DATA_DIR_NAME: &str = match option_env!("CRASH_MONITOR_DATA_DIR_NAME") {
    Some(name) => name,
    None => ".crash_monitor",
};

/// Base directory for crash reporter data: `$CRASH_MONITOR_DATA_DIR` if set,
/// else `~/.crash_monitor/`.
pub fn data_dir_path() -> Result<PathBuf, String> {
    let dir = if let Ok(override_path) = std::env::var(DATA_DIR_OVERRIDE_ENV) {
        if override_path.is_empty() {
            return Err(format!("{DATA_DIR_OVERRIDE_ENV} is set but empty"));
        }
        PathBuf::from(override_path)
    } else {
        let home = std::env::var("HOME").map_err(|_| "HOME not set".to_string())?;
        PathBuf::from(home).join(DEFAULT_DATA_DIR_NAME)
    };
    Ok(dir)
}

pub fn data_dir() -> Result<PathBuf, String> {
    let dir = data_dir_path()?;
    fs::create_dir_all(&dir).map_err(|e| format!("Failed to create data dir: {e}"))?;
    Ok(dir)
}

/// Resolve the pending report root without touching the filesystem. Capture
/// paths use this pure helper so directory I/O cannot extend task suspension
/// or the Mach exception reply deadline.
pub fn pending_dir_path() -> Result<PathBuf, String> {
    Ok(data_dir_path()?.join("crashes").join("pending"))
}

/// Working directory for in-flight reports: `<data_dir>/crashes/pending/`.
/// The pipeline writes Stage 1 raw dumps, Stage 2 JSON, and intermediate
/// files here. The `MoveToSent` post-processor relocates finished reports
/// to `sent_dir()`.
pub fn pending_dir() -> Result<PathBuf, String> {
    let dir = pending_dir_path()?;
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
