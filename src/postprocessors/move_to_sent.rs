//! Post-processor: relocate finished reports from `pending/` to `sent/`.
//!
//! Runs near the end of the post-processor chain (after PNG conversion,
//! feedback dialog, ZIP archival). For each `result.json_path`, finds every
//! file in the same directory that shares the report's basename prefix and
//! moves them all to the sibling `sent/` directory. Mutates
//! `result.json_path` so downstream notifiers see the new path.

use std::fs;
use std::path::{Path, PathBuf};

use crate::pipeline::{CrashEvent, Plugin, PostProcessor, Priority, ReportResult};

#[derive(Default)]
pub struct MoveToSent {
    sent_dir_override: Option<PathBuf>,
}

impl MoveToSent {
    /// Use `paths::sent_dir()` at process time. Production default.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Force a specific sent directory. Used by tests with tempdir layouts.
    #[cfg(test)]
    #[must_use]
    pub fn with_dir(sent_dir: PathBuf) -> Self {
        Self {
            sent_dir_override: Some(sent_dir),
        }
    }

    fn resolve_sent_dir(&self, pending: &Path) -> Result<PathBuf, String> {
        if let Some(ref p) = self.sent_dir_override {
            fs::create_dir_all(p).map_err(|e| format!("create sent override: {e}"))?;
            return Ok(p.clone());
        }
        // Production: prefer the documented `sent_dir()` (under data_dir).
        if let Ok(p) = crate::utils::paths::sent_dir() {
            return Ok(p);
        }
        // Fallback: sibling of pending. Used when output_dir is overridden
        // (e.g., integration tests) but sent_dir_override isn't.
        let sibling = crate::utils::paths::sent_dir_for(pending);
        fs::create_dir_all(&sibling).map_err(|e| format!("create sent sibling: {e}"))?;
        Ok(sibling)
    }
}

impl Plugin for MoveToSent {
    fn name(&self) -> &'static str {
        "MoveToSent"
    }
    fn priority(&self) -> Priority {
        Priority::Low
    }
}

impl PostProcessor for MoveToSent {
    fn process(&self, _event: &CrashEvent, result: &mut ReportResult) -> Result<(), String> {
        let Some(json_path) = result.json_path.clone() else {
            return Ok(()); // Nothing written → nothing to move
        };
        let pending_dir = json_path
            .parent()
            .ok_or_else(|| "json_path has no parent".to_string())?
            .to_path_buf();

        let stem = json_path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| "json_path stem unreadable".to_string())?
            .to_string();

        let sent_dir = self.resolve_sent_dir(&pending_dir)?;

        let mut moved_json: Option<PathBuf> = None;
        let mut moved_raw: Option<PathBuf> = None;

        let read_dir = fs::read_dir(&pending_dir)
            .map_err(|e| format!("read_dir {}: {e}", pending_dir.display()))?;
        for entry in read_dir {
            let Ok(entry) = entry else { continue };
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !name.starts_with(&stem) {
                continue;
            }
            // Match either exact basename or basename followed by `.` / `_`.
            // Prevents accidental prefix collision between e.g. `crash_..._123`
            // and `crash_..._1234`.
            let suffix = &name[stem.len()..];
            if !suffix.is_empty() && !suffix.starts_with('.') && !suffix.starts_with('_') {
                continue;
            }

            let dest = sent_dir.join(name);
            if let Err(e) = move_file(&path, &dest) {
                eprintln!(
                    "[monitor] MoveToSent: failed to move {} → {}: {e}",
                    path.display(),
                    dest.display()
                );
                continue;
            }
            if let Some(ref jp) = result.json_path
                && *jp == path
            {
                moved_json = Some(dest.clone());
            }
            if let Some(ref rp) = result.raw_path
                && *rp == path
            {
                moved_raw = Some(dest);
            }
        }

        if let Some(jp) = moved_json {
            result.json_path = Some(jp);
        }
        if let Some(rp) = moved_raw {
            result.raw_path = Some(rp);
        }
        Ok(())
    }
}

/// Move a file from `src` to `dst`. Falls back to copy + delete when `rename`
/// fails with `EXDEV` (cross-filesystem) — relevant for some test sandboxes.
fn move_file(src: &Path, dst: &Path) -> Result<(), String> {
    if fs::rename(src, dst).is_ok() {
        return Ok(());
    }
    fs::copy(src, dst).map_err(|e| format!("copy: {e}"))?;
    fs::remove_file(src).map_err(|e| format!("remove src: {e}"))?;
    Ok(())
}

#[cfg(test)]
#[path = "../../tests/unit/postprocessors/move_to_sent_tests.rs"]
mod tests;
