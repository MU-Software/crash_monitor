//! Configuration system for crash reporter plugins (opt-out design).
//!
//! All plugins are enabled by default. The config file (`crash_reporter.json`)
//! is only needed to disable specific plugins or adjust parameters.
//! Missing file or parse errors silently fall back to defaults.

use serde::Deserialize;
use std::fs;
use std::path::Path;

use crate::utils::paths;

// ═══════════════════════════════════════════════════
//  Top-level config
// ═══════════════════════════════════════════════════

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct CrashReporterConfig {
    pub enabled: bool,
    pub report_dir: Option<String>,
    pub triggers: TriggersConfig,
    pub filters: FilterConfig,
    pub collectors: CollectorConfig,
    pub pre_processors: PreProcessorConfig,
    pub post_processors: PostProcessorConfig,
    pub notifiers: NotifierConfig,
}

impl Default for CrashReporterConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            report_dir: None,
            triggers: TriggersConfig::default(),
            filters: FilterConfig::default(),
            collectors: CollectorConfig::default(),
            pre_processors: PreProcessorConfig::default(),
            post_processors: PostProcessorConfig::default(),
            notifiers: NotifierConfig::default(),
        }
    }
}

// ═══════════════════════════════════════════════════
//  Triggers config
// ═══════════════════════════════════════════════════

/// Toggles for inline triggers. Mach exception and SIGUSR1 snapshot are
/// always-on (they have no opt-out). ANR is configured via its own
/// `AnrConfig` passed to `event_loop`. Only OOM detection is toggleable here.
/// When Phase 8 extracts triggers into proper plugins this section will grow.
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct TriggersConfig {
    pub enabled: bool,
    pub oom_detection: PluginToggle,
}

impl Default for TriggersConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            oom_detection: PluginToggle::default(),
        }
    }
}

// ═══════════════════════════════════════════════════
//  Filter config
// ═══════════════════════════════════════════════════

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct FilterConfig {
    pub enabled: bool,
    pub disk_space: DiskSpaceConfig,
    pub rate_limiter: RateLimiterConfig,
}

impl Default for FilterConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            disk_space: DiskSpaceConfig::default(),
            rate_limiter: RateLimiterConfig::default(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct DiskSpaceConfig {
    pub enabled: bool,
    pub min_free_mb: u64,
}

impl Default for DiskSpaceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_free_mb: 100,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct RateLimiterConfig {
    pub enabled: bool,
    pub max_events: usize,
    pub window_secs: u64,
}

impl Default for RateLimiterConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_events: 3,
            window_secs: 60,
        }
    }
}

// ═══════════════════════════════════════════════════
//  Collector config
// ═══════════════════════════════════════════════════

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct CollectorConfig {
    pub enabled: bool,
    pub thread: PluginToggle,
    pub breadcrumb: PluginToggle,
    pub context: PluginToggle,
    pub memory: PluginToggle,
    pub dylib: PluginToggle,
    pub screenshot: PluginToggle,
    pub attachment: PluginToggle,
    pub environment: PluginToggle,
}

impl Default for CollectorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            thread: PluginToggle::default(),
            breadcrumb: PluginToggle::default(),
            context: PluginToggle::default(),
            memory: PluginToggle::default(),
            dylib: PluginToggle::default(),
            screenshot: PluginToggle::default(),
            attachment: PluginToggle::default(),
            environment: PluginToggle::default(),
        }
    }
}

// ═══════════════════════════════════════════════════
//  Pre-processor config
// ═══════════════════════════════════════════════════

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct PreProcessorConfig {
    pub enabled: bool,
    pub session: PluginToggle,
    pub symbolizer: PluginToggle,
    pub fingerprint: FingerprintConfig,
    pub build_info: PluginToggle,
    pub duplicate: DuplicateConfig,
    pub sanitizer: PluginToggle,
}

impl Default for PreProcessorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            session: PluginToggle::default(),
            symbolizer: PluginToggle::default(),
            fingerprint: FingerprintConfig::default(),
            build_info: PluginToggle::default(),
            duplicate: DuplicateConfig::default(),
            sanitizer: PluginToggle::default(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct FingerprintConfig {
    pub enabled: bool,
    pub top_frames: usize,
}

impl Default for FingerprintConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            top_frames: 8,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct DuplicateConfig {
    pub enabled: bool,
    pub window_secs: u64,
}

impl Default for DuplicateConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            window_secs: 60,
        }
    }
}

// ═══════════════════════════════════════════════════
//  Post-processor config
// ═══════════════════════════════════════════════════

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct PostProcessorConfig {
    pub enabled: bool,
    pub raw_cleanup: PluginToggle,
    pub session_recorder: PluginToggle,
    pub png_converter: PluginToggle,
    pub feedback_dialog: PluginToggle,
    pub zip_archiver: PluginToggle,
    pub move_to_sent: PluginToggle,
    pub log_rotator: LogRotatorConfig,
    pub retention: RetentionConfig,
}

impl Default for PostProcessorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            raw_cleanup: PluginToggle::default(),
            session_recorder: PluginToggle::default(),
            png_converter: PluginToggle::default(),
            feedback_dialog: PluginToggle::default(),
            zip_archiver: PluginToggle::default(),
            move_to_sent: PluginToggle::default(),
            log_rotator: LogRotatorConfig::default(),
            retention: RetentionConfig::default(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct LogRotatorConfig {
    pub enabled: bool,
    pub max_size_mb: u64,
}

impl Default for LogRotatorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_size_mb: 1,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct RetentionConfig {
    pub enabled: bool,
    pub max_reports: usize,
    pub max_size_mb: u64,
    pub max_age_days: u64,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_reports: 64,
            max_size_mb: 256,
            max_age_days: 15,
        }
    }
}

// ═══════════════════════════════════════════════════
//  Notifier config
// ═══════════════════════════════════════════════════

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct NotifierConfig {
    pub enabled: bool,
    pub console: PluginToggle,
    pub system_notification: SystemNotificationConfig,
}

impl Default for NotifierConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            console: PluginToggle::default(),
            system_notification: SystemNotificationConfig::default(),
        }
    }
}

/// Only plugin disabled by default (`bool` defaults to `false`).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct SystemNotificationConfig {
    pub enabled: bool,
}

// ═══════════════════════════════════════════════════
//  Shared toggle (enabled-only plugins)
// ═══════════════════════════════════════════════════

#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct PluginToggle {
    pub enabled: bool,
}

impl Default for PluginToggle {
    fn default() -> Self {
        Self { enabled: true }
    }
}

// ═══════════════════════════════════════════════════
//  Helpers
// ═══════════════════════════════════════════════════

/// Three-level AND check: global → category → plugin.
#[must_use]
pub fn is_enabled(global: bool, category: bool, plugin: bool) -> bool {
    global && category && plugin
}

// ═══════════════════════════════════════════════════
//  Loading
// ═══════════════════════════════════════════════════

const CONFIG_FILENAME: &str = "crash_reporter.json";

/// Load config from `<data_dir>/crash_reporter.json`.
///
/// Returns `Default::default()` on missing file, read error, or parse error.
#[must_use]
pub fn load_config() -> CrashReporterConfig {
    load_config_from_data_dir().unwrap_or_default()
}

fn load_config_from_data_dir() -> Option<CrashReporterConfig> {
    let data_dir = paths::data_dir().ok()?;
    let path = data_dir.join(CONFIG_FILENAME);
    load_config_from_path(&path)
}

fn load_config_from_path(path: &Path) -> Option<CrashReporterConfig> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
#[path = "../tests/unit/config_tests.rs"]
mod tests;
