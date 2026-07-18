//! Operational tracing initialization.

use std::sync::OnceLock;
use tracing_subscriber::EnvFilter;

static INITIALIZED: OnceLock<()> = OnceLock::new();

/// Initialize JSON operational logs on stderr once per monitor process.
pub fn init() {
    INITIALIZED.get_or_init(|| {
        let filter = EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new("crash_monitor=info"));
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .with_target(true)
            .json()
            .try_init();
    });
}
