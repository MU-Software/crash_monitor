//! Deterministic process-boundary hooks for signed end-to-end tests.
//!
//! This module is compiled only with the `test-support` feature used by the
//! dedicated E2E build. Production packages never contain these pauses.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::time::Duration;

const PAUSE_STAGE_ENV: &str = "CRASH_MONITOR_TEST_PAUSE_STAGE";
const STAGE_MARKER_ENV: &str = "CRASH_MONITOR_TEST_STAGE_MARKER";

/// Publish that `stage` was reached, then wait for the E2E harness to kill the
/// monitor. A bounded fallback keeps an accidentally configured test build
/// from hanging forever.
pub(crate) fn pause_at(stage: &str) {
    if std::env::var(PAUSE_STAGE_ENV).as_deref() != Ok(stage) {
        return;
    }

    let marker = std::env::var_os(STAGE_MARKER_ENV).map_or_else(
        || panic!("{STAGE_MARKER_ENV} is required when pausing at {stage}"),
        PathBuf::from,
    );
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true).mode(0o600);
    let mut file = options.open(&marker).unwrap_or_else(|error| {
        panic!("cannot create stage marker '{}': {error}", marker.display())
    });
    writeln!(file, "{stage}\n{}", std::process::id()).unwrap_or_else(|error| {
        panic!("cannot write stage marker '{}': {error}", marker.display())
    });
    file.sync_all()
        .unwrap_or_else(|error| panic!("cannot sync stage marker '{}': {error}", marker.display()));

    std::thread::park_timeout(Duration::from_mins(1));
}
