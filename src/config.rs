//! Configuration system for crash reporter plugins.
//!
//! Report triggers and non-sensitive plugins are enabled by default. Collection
//! of raw stack bytes, memory diagnostics, environment data, process output,
//! screenshots, attachments, and raw shared-memory dumps is fail-closed behind an explicit
//! privacy profile, consent declaration, and evidence-specific opt-in. A missing
//! file selects the minimal profile; an existing unreadable, unsafe, or malformed
//! file fails startup before the monitored child is spawned.

use serde::Deserialize;
use std::collections::{BTreeSet, HashMap};
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::pipeline::types::{DependencyKind, PluginCategory, PluginId};
use crate::utils::paths;

// ═══════════════════════════════════════════════════
//  Top-level config
// ═══════════════════════════════════════════════════

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CrashReporterConfig {
    pub enabled: bool,
    pub report_dir: Option<String>,
    pub privacy: PrivacyConfig,
    pub triggers: TriggersConfig,
    pub watchdog: WatchdogConfig,
    pub filters: FilterConfig,
    pub collectors: CollectorConfig,
    pub pre_processors: PreProcessorConfig,
    pub post_processors: PostProcessorConfig,
    pub notifiers: NotifierConfig,
}

/// Privacy boundary applied before sensitive collector toggles are resolved.
///
/// `level` is an upper bound, `consent` is a separate mandatory gate, and an
/// individual collector toggle can only narrow the resulting set. The config
/// file is a deployment-time consent declaration; it does not display or
/// substitute for an application UI.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PrivacyConfig {
    pub level: PrivacyLevel,
    pub consent: ConsentState,
    pub encryption: EncryptionPolicy,
    /// Persist fail-safe breadcrumb/context wire snapshots before formatting.
    /// This raw evidence is full-profile only and remains independently opt-in.
    pub raw_shm: bool,
}

impl Default for PrivacyConfig {
    fn default() -> Self {
        Self {
            level: PrivacyLevel::Minimal,
            consent: ConsentState::NotGranted,
            encryption: EncryptionPolicy::None,
            raw_shm: false,
        }
    }
}

/// Immutable sensitive-data decisions consumed by the capture pipeline.
///
/// Collector registration, live task reads, shared-memory copying, and raw
/// persistence all derive from this one normalized value. Individual toggles
/// can narrow a privacy profile but can never widen it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[allow(clippy::struct_excessive_bools)] // one bit per independent evidence boundary
pub struct CollectionPolicy {
    pub capture_stack_memory: bool,
    pub capture_shm_screenshots: bool,
    pub capture_shm_attachments: bool,
    pub persist_raw_shm: bool,
}

impl CollectionPolicy {
    /// No optional memory or raw shared-memory evidence.
    pub const MINIMAL: Self = Self {
        capture_stack_memory: false,
        capture_shm_screenshots: false,
        capture_shm_attachments: false,
        persist_raw_shm: false,
    };

    /// Historical all-on behavior for focused tests and explicit embedders.
    pub const FULL: Self = Self {
        capture_stack_memory: true,
        capture_shm_screenshots: true,
        capture_shm_attachments: true,
        persist_raw_shm: true,
    };
}

/// Maximum class of sensitive evidence that may be collected.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrivacyLevel {
    /// Do not collect any optional sensitive evidence.
    #[default]
    Minimal,
    /// Permit stack bytes and memory diagnostics, when consent is also granted.
    Diagnostic,
    /// Permit every sensitive evidence class, when consent is also granted.
    Full,
}

/// Deployment-time assertion that sensitive collection has been authorized.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConsentState {
    #[default]
    NotGranted,
    Granted,
}

/// Application-layer encryption requirement for report artifacts.
///
/// The current writer does not implement application-layer encryption. `None`
/// states that honestly; `Required` makes startup fail closed instead of
/// silently writing plaintext. Filesystem permissions and external encrypted
/// volumes are separate controls and are not reported as encryption here.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EncryptionPolicy {
    #[default]
    None,
    Required,
}

/// Configuration after global/category/plugin enablement has been resolved.
///
/// This normalization step gives the pipeline one immutable global kill switch,
/// one explicit policy bit per report-producing trigger, and a validated plugin
/// registry closed over hard dependencies. Non-fatal policy decisions are kept
/// as structured startup diagnostics.
#[derive(Debug)]
pub struct ValidatedConfig {
    /// Global report-generation kill switch.
    pub enabled: bool,
    /// Trigger policy with the `triggers.enabled` category switch resolved.
    pub triggers: ValidatedTriggersConfig,
    diagnostics: Vec<ConfigValidationDiagnostic>,
    enabled_plugins: BTreeSet<PluginId>,
    collection_policy: CollectionPolicy,
    config: CrashReporterConfig,
}

impl ValidatedConfig {
    /// The normalized source configuration used to construct plugin lists.
    #[must_use]
    pub fn config(&self) -> &CrashReporterConfig {
        &self.config
    }

    /// Non-fatal policy decisions made while closing the dependency graph.
    #[must_use]
    pub fn diagnostics(&self) -> &[ConfigValidationDiagnostic] {
        &self.diagnostics
    }

    /// Effective plugin enablement after category switches and hard
    /// dependency closure have been applied.
    #[must_use]
    pub(crate) fn plugin_enabled(&self, plugin_id: impl AsRef<str>) -> bool {
        self.enabled
            && self
                .enabled_plugins
                .iter()
                .any(|enabled| enabled.as_str() == plugin_id.as_ref())
    }

    /// Effective immutable sensitive-data policy for capture and persistence.
    #[must_use]
    pub const fn collection_policy(&self) -> CollectionPolicy {
        self.collection_policy
    }

    /// Validated watchdog timings from the immutable startup config.
    #[must_use]
    pub const fn watchdog(&self) -> WatchdogConfig {
        self.config.watchdog
    }
}

/// A non-fatal configuration decision worth surfacing at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigValidationDiagnostic {
    /// The dependent was requested, but a required data provider was
    /// explicitly disabled. Explicit opt-out wins over default-on behavior.
    DependentDisabled {
        category: PluginCategory,
        plugin_id: String,
        dependency: String,
    },
    /// A sensitive collector was requested, but the privacy profile or consent
    /// declaration did not authorize it.
    SensitiveCollectorDisabled {
        plugin_id: String,
        level: PrivacyLevel,
        consent: ConsentState,
    },
    /// Sensitive evidence outside a standalone collector was requested but
    /// denied by the active privacy level or consent state.
    SensitiveEvidenceDenied {
        evidence: &'static str,
        level: PrivacyLevel,
        consent: ConsentState,
    },
}

impl std::fmt::Display for ConfigValidationDiagnostic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DependentDisabled {
                category,
                plugin_id,
                dependency,
            } => write!(
                f,
                "{category} plugin '{plugin_id}' disabled because hard dependency '{dependency}' is disabled"
            ),
            Self::SensitiveCollectorDisabled {
                plugin_id,
                level,
                consent,
            } => write!(
                f,
                "sensitive collector '{plugin_id}' disabled by privacy level {level:?} with consent {consent:?}"
            ),
            Self::SensitiveEvidenceDenied {
                evidence,
                level,
                consent,
            } => write!(
                f,
                "sensitive evidence '{evidence}' disabled by privacy level {level:?} with consent {consent:?}"
            ),
        }
    }
}

/// Fatal privacy or plugin-graph validation failures. These are returned to
/// startup; invalid policy and plugin combinations never use `panic!` as
/// control flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigValidationError {
    /// Application-layer artifact encryption was required, but this build has
    /// no encryption writer. Startup fails before any report is captured.
    ApplicationEncryptionUnavailable,
    RetentionMaxReportsZero,
    InvalidNumericRange {
        field: &'static str,
        requirement: &'static str,
    },
    DuplicatePluginId {
        plugin_id: String,
        first_category: PluginCategory,
        second_category: PluginCategory,
    },
    MissingDependency {
        category: PluginCategory,
        plugin_id: String,
        dependency: String,
        kind: DependencyKind,
    },
    DependencyCycle {
        category: PluginCategory,
        cycle: Vec<String>,
    },
    InvalidDependencyOrder {
        category: PluginCategory,
        plugin_id: String,
        dependency: String,
        kind: DependencyKind,
    },
}

impl std::fmt::Display for ConfigValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ApplicationEncryptionUnavailable => f.write_str(
                "privacy.encryption='required' cannot be satisfied: application-layer report encryption is not implemented",
            ),
            Self::RetentionMaxReportsZero => f.write_str(
                "post_processors.retention.max_reports must be greater than zero when retention is enabled",
            ),
            Self::InvalidNumericRange { field, requirement } => {
                write!(f, "{field} {requirement}")
            }
            Self::DuplicatePluginId {
                plugin_id,
                first_category,
                second_category,
            } => write!(
                f,
                "duplicate plugin ID '{plugin_id}' in {first_category} and {second_category} categories"
            ),
            Self::MissingDependency {
                category,
                plugin_id,
                dependency,
                kind,
            } => write!(
                f,
                "{category} plugin '{plugin_id}' has missing {kind} dependency '{dependency}'"
            ),
            Self::DependencyCycle { category, cycle } => write!(
                f,
                "{category} plugin dependency cycle: {}",
                cycle.join(" -> ")
            ),
            Self::InvalidDependencyOrder {
                category,
                plugin_id,
                dependency,
                kind,
            } => write!(
                f,
                "{category} plugin '{plugin_id}' is registered before its {kind} dependency '{dependency}'"
            ),
        }
    }
}

impl std::error::Error for ConfigValidationError {}

/// Failure to resolve, read, parse, or validate the startup configuration.
///
/// A genuinely missing file is not an error and selects the private minimal
/// defaults. Every other existing-file failure is explicit so an encryption
/// requirement can never disappear through fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigLoadError {
    DataDirectory(crate::errors::PathError),
    Read { path: PathBuf, error: String },
    UnsafeFile { path: PathBuf, reason: String },
    Parse { path: PathBuf, error: String },
    Validation(ConfigValidationError),
}

impl std::fmt::Display for ConfigLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DataDirectory(error) => {
                write!(f, "cannot resolve configuration directory: {error}")
            }
            Self::Read { path, error } => {
                write!(f, "cannot read configuration '{}': {error}", path.display())
            }
            Self::UnsafeFile { path, reason } => write!(
                f,
                "unsafe configuration file '{}': {reason}",
                path.display()
            ),
            Self::Parse { path, error } => {
                write!(
                    f,
                    "cannot parse configuration '{}': {error}",
                    path.display()
                )
            }
            Self::Validation(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for ConfigLoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::DataDirectory(error) => Some(error),
            Self::Validation(error) => Some(error),
            Self::Read { .. } | Self::UnsafeFile { .. } | Self::Parse { .. } => None,
        }
    }
}

impl From<ConfigValidationError> for ConfigLoadError {
    fn from(error: ConfigValidationError) -> Self {
        Self::Validation(error)
    }
}

/// Explicit enablement for each event that can produce a report.
///
/// These values already include the `triggers.enabled` category switch, but
/// deliberately do not absorb the global switch. Keeping the global state
/// separate makes it impossible to mistake a per-trigger option for the
/// process-wide kill switch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // one explicit switch per external trigger
pub struct ValidatedTriggersConfig {
    pub crash: bool,
    pub exit_failure: bool,
    pub signal_failure: bool,
    pub probable_oom: bool,
    pub anr: bool,
    pub snapshot: bool,
}

impl CrashReporterConfig {
    /// Resolve hierarchical enablement into a validated runtime configuration.
    ///
    /// # Errors
    /// Returns a structured error when required application-layer encryption
    /// is unavailable, or when the built-in plugin registry contains duplicate
    /// IDs, a missing dependency declaration, or a dependency cycle/order
    /// violation.
    pub fn validate(self) -> Result<ValidatedConfig, ConfigValidationError> {
        // The global kill switch produces no artifacts, so an unavailable
        // encryption requirement is irrelevant while reporting is disabled.
        if self.enabled && self.privacy.encryption == EncryptionPolicy::Required {
            return Err(ConfigValidationError::ApplicationEncryptionUnavailable);
        }
        if self.post_processors.retention.enabled && self.post_processors.retention.max_reports == 0
        {
            return Err(ConfigValidationError::RetentionMaxReportsZero);
        }
        for (invalid, field) in [
            (
                self.filters.rate_limiter.enabled && self.filters.rate_limiter.window_secs == 0,
                "filters.rate_limiter.window_secs",
            ),
            (
                self.pre_processors.fingerprint.enabled
                    && self.pre_processors.fingerprint.top_frames == 0,
                "pre_processors.fingerprint.top_frames",
            ),
            (
                self.pre_processors.duplicate.enabled
                    && self.pre_processors.duplicate.window_secs == 0,
                "pre_processors.duplicate.window_secs",
            ),
            (
                self.post_processors.log_rotator.enabled
                    && self.post_processors.log_rotator.max_size_mb == 0,
                "post_processors.log_rotator.max_size_mb",
            ),
        ] {
            if invalid {
                return Err(ConfigValidationError::InvalidNumericRange {
                    field,
                    requirement: "must be greater than zero when enabled",
                });
            }
        }
        self.watchdog.validate()?;
        let trigger_category_enabled = self.triggers.enabled;
        let triggers = ValidatedTriggersConfig {
            crash: trigger_category_enabled && self.triggers.crash.enabled,
            exit_failure: trigger_category_enabled && self.triggers.exit_failure.enabled,
            signal_failure: trigger_category_enabled && self.triggers.signal_failure.enabled,
            probable_oom: trigger_category_enabled && self.triggers.oom_detection.enabled,
            anr: trigger_category_enabled && self.triggers.anr.enabled,
            snapshot: trigger_category_enabled && self.triggers.snapshot.enabled,
        };
        let (enabled_plugins, mut diagnostics) = resolve_plugin_enablement(&self)?;
        diagnostics.extend(sensitive_evidence_diagnostics(&self));
        let collection_policy = resolve_collection_policy(&self, &enabled_plugins);
        Ok(ValidatedConfig {
            enabled: self.enabled,
            triggers,
            diagnostics,
            enabled_plugins,
            collection_policy,
            config: self,
        })
    }
}

impl Default for CrashReporterConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            report_dir: None,
            privacy: PrivacyConfig::default(),
            triggers: TriggersConfig::default(),
            watchdog: WatchdogConfig::default(),
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

/// Toggles for every event source that can create a report.
///
/// Semantics are intentionally independent:
/// - `crash`: fatal Mach exception capture; this primary trigger owns the
///   incident even after the child is reaped, so its termination metadata does
///   not fire a second exit/signal report;
/// - `exit_failure`: non-zero process exit observed without a prior Mach crash;
/// - `signal_failure`: signal termination observed without a prior Mach crash
///   and not classified as probable OOM;
/// - `oom_detection`: classify a primary SIGKILL child-termination event as
///   probable OOM instead of signal failure;
/// - `anr`: watchdog-generated unresponsive report;
/// - `snapshot`: manual SIGUSR1 snapshot.
///
/// `enabled=false` disables the whole trigger category. The top-level global
/// switch remains authoritative and has no implicit emergency-evidence
/// exception.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TriggersConfig {
    pub enabled: bool,
    pub crash: PluginToggle,
    pub exit_failure: PluginToggle,
    pub signal_failure: PluginToggle,
    pub oom_detection: PluginToggle,
    pub anr: PluginToggle,
    pub snapshot: PluginToggle,
}

/// ANR watchdog timing configuration in milliseconds.
///
/// JSON is authoritative. Environment overrides are applied by the binary
/// only when the operations/test override gate is explicitly enabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WatchdogConfig {
    pub warmup_ms: u64,
    pub threshold_ms: u64,
    pub check_interval_ms: u64,
    pub cooldown_ms: u64,
}

impl Default for WatchdogConfig {
    fn default() -> Self {
        Self {
            warmup_ms: 10_000,
            threshold_ms: 5_000,
            check_interval_ms: 2_000,
            cooldown_ms: 60_000,
        }
    }
}

impl WatchdogConfig {
    /// Validate watchdog timings after every source of configuration,
    /// including explicitly enabled environment overrides.
    ///
    /// # Errors
    /// Returns [`ConfigValidationError::InvalidNumericRange`] when a timing is
    /// zero or the polling interval exceeds the ANR threshold.
    pub fn validate(self) -> Result<Self, ConfigValidationError> {
        for (value, field) in [
            (self.warmup_ms, "watchdog.warmup_ms"),
            (self.threshold_ms, "watchdog.threshold_ms"),
            (self.check_interval_ms, "watchdog.check_interval_ms"),
            (self.cooldown_ms, "watchdog.cooldown_ms"),
        ] {
            if value == 0 {
                return Err(ConfigValidationError::InvalidNumericRange {
                    field,
                    requirement: "must be greater than zero",
                });
            }
        }
        if self.check_interval_ms > self.threshold_ms {
            return Err(ConfigValidationError::InvalidNumericRange {
                field: "watchdog.check_interval_ms",
                requirement: "must not exceed watchdog.threshold_ms",
            });
        }
        Ok(self)
    }
}

impl Default for TriggersConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            crash: PluginToggle::default(),
            exit_failure: PluginToggle::default(),
            signal_failure: PluginToggle::default(),
            // SIGKILL alone is weak OOM evidence, so this classification is an
            // explicit opt-in rather than a default-on trigger.
            oom_detection: PluginToggle::disabled(),
            anr: PluginToggle::default(),
            snapshot: PluginToggle::default(),
        }
    }
}

// ═══════════════════════════════════════════════════
//  Filter config
// ═══════════════════════════════════════════════════

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
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
#[serde(default, deny_unknown_fields)]
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
#[serde(default, deny_unknown_fields)]
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
#[serde(default, deny_unknown_fields)]
pub struct CollectorConfig {
    pub enabled: bool,
    pub thread: ThreadCollectorConfig,
    pub breadcrumb: PluginToggle,
    pub context: PluginToggle,
    pub memory: PluginToggle,
    pub dylib: PluginToggle,
    pub screenshot: PluginToggle,
    pub attachment: PluginToggle,
    pub environment: PluginToggle,
    pub process_output: PluginToggle,
}

impl Default for CollectorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            thread: ThreadCollectorConfig::default(),
            breadcrumb: PluginToggle::default(),
            context: PluginToggle::default(),
            memory: PluginToggle::disabled(),
            dylib: PluginToggle::default(),
            screenshot: PluginToggle::disabled(),
            attachment: PluginToggle::disabled(),
            environment: PluginToggle::disabled(),
            process_output: PluginToggle::disabled(),
        }
    }
}

/// Thread-state collection with a separate opt-in for raw stack bytes.
/// Registers and backtraces remain the minimal diagnostic baseline.
#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ThreadCollectorConfig {
    pub enabled: bool,
    pub stack_memory: bool,
}

impl Default for ThreadCollectorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            stack_memory: false,
        }
    }
}

// ═══════════════════════════════════════════════════
//  Pre-processor config
// ═══════════════════════════════════════════════════

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
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
#[serde(default, deny_unknown_fields)]
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
#[serde(default, deny_unknown_fields)]
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
#[serde(default, deny_unknown_fields)]
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
#[serde(default, deny_unknown_fields)]
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
#[serde(default, deny_unknown_fields)]
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
            max_reports: 16,
            max_size_mb: 64,
            max_age_days: 7,
        }
    }
}

// ═══════════════════════════════════════════════════
//  Notifier config
// ═══════════════════════════════════════════════════

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
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
#[serde(default, deny_unknown_fields)]
pub struct SystemNotificationConfig {
    pub enabled: bool,
}

// ═══════════════════════════════════════════════════
//  Shared toggle (enabled-only plugins)
// ═══════════════════════════════════════════════════

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PluginToggle {
    pub enabled: bool,
}

impl PluginToggle {
    const fn disabled() -> Self {
        Self { enabled: false }
    }
}

impl Default for PluginToggle {
    fn default() -> Self {
        Self { enabled: true }
    }
}

// ═══════════════════════════════════════════════
//  Plugin dependency graph
// ═══════════════════════════════════════════════

#[derive(Clone, Copy)]
struct PluginSpec {
    category: PluginCategory,
    id: &'static str,
    hard_dependencies: &'static [&'static str],
    order_dependencies: &'static [&'static str],
}

const NO_DEPS: &[&str] = &[];

// Configuration-side registry. Runtime instances are validated again by the
// pipeline factory, which catches registration drift independently.
const PLUGIN_SPECS: &[PluginSpec] = &[
    PluginSpec {
        category: PluginCategory::Filter,
        id: "DiskSpaceFilter",
        hard_dependencies: NO_DEPS,
        order_dependencies: NO_DEPS,
    },
    PluginSpec {
        category: PluginCategory::Filter,
        id: "RateLimiter",
        hard_dependencies: NO_DEPS,
        order_dependencies: &["DiskSpaceFilter"],
    },
    PluginSpec {
        category: PluginCategory::Collector,
        id: "ThreadCollector",
        hard_dependencies: NO_DEPS,
        order_dependencies: NO_DEPS,
    },
    PluginSpec {
        category: PluginCategory::Collector,
        id: "BreadcrumbCollector",
        hard_dependencies: NO_DEPS,
        order_dependencies: NO_DEPS,
    },
    PluginSpec {
        category: PluginCategory::Collector,
        id: "ContextCollector",
        hard_dependencies: NO_DEPS,
        order_dependencies: NO_DEPS,
    },
    PluginSpec {
        category: PluginCategory::Collector,
        id: "MemoryCollector",
        hard_dependencies: NO_DEPS,
        order_dependencies: NO_DEPS,
    },
    PluginSpec {
        category: PluginCategory::Collector,
        id: "DylibCollector",
        hard_dependencies: NO_DEPS,
        order_dependencies: NO_DEPS,
    },
    PluginSpec {
        category: PluginCategory::Collector,
        id: "ScreenshotCollector",
        hard_dependencies: NO_DEPS,
        order_dependencies: NO_DEPS,
    },
    PluginSpec {
        category: PluginCategory::Collector,
        id: "AttachmentCollector",
        hard_dependencies: NO_DEPS,
        order_dependencies: NO_DEPS,
    },
    PluginSpec {
        category: PluginCategory::Collector,
        id: "EnvironmentCollector",
        hard_dependencies: NO_DEPS,
        order_dependencies: NO_DEPS,
    },
    PluginSpec {
        category: PluginCategory::Collector,
        id: "ProcessOutputCollector",
        hard_dependencies: NO_DEPS,
        order_dependencies: NO_DEPS,
    },
    PluginSpec {
        category: PluginCategory::PreProcessor,
        id: "SessionEnricher",
        hard_dependencies: NO_DEPS,
        order_dependencies: NO_DEPS,
    },
    PluginSpec {
        category: PluginCategory::PreProcessor,
        id: "SymbolResolver",
        hard_dependencies: NO_DEPS,
        order_dependencies: NO_DEPS,
    },
    PluginSpec {
        category: PluginCategory::PreProcessor,
        id: "Fingerprinter",
        hard_dependencies: NO_DEPS,
        order_dependencies: &["SymbolResolver"],
    },
    PluginSpec {
        category: PluginCategory::PreProcessor,
        id: "BuildInfoEnricher",
        hard_dependencies: NO_DEPS,
        order_dependencies: NO_DEPS,
    },
    PluginSpec {
        category: PluginCategory::PreProcessor,
        id: "DuplicateDetector",
        hard_dependencies: &["Fingerprinter"],
        order_dependencies: NO_DEPS,
    },
    PluginSpec {
        category: PluginCategory::PreProcessor,
        id: "Sanitizer",
        hard_dependencies: NO_DEPS,
        order_dependencies: &["Fingerprinter"],
    },
    PluginSpec {
        category: PluginCategory::PostProcessor,
        id: "RawCleanup",
        hard_dependencies: NO_DEPS,
        order_dependencies: NO_DEPS,
    },
    PluginSpec {
        category: PluginCategory::PostProcessor,
        id: "SessionRecorder",
        hard_dependencies: NO_DEPS,
        order_dependencies: NO_DEPS,
    },
    PluginSpec {
        category: PluginCategory::PostProcessor,
        id: "PNGConverter",
        hard_dependencies: NO_DEPS,
        order_dependencies: NO_DEPS,
    },
    PluginSpec {
        category: PluginCategory::PostProcessor,
        id: "FeedbackDialog",
        hard_dependencies: NO_DEPS,
        order_dependencies: &["PNGConverter"],
    },
    PluginSpec {
        category: PluginCategory::PostProcessor,
        id: "ZIPArchiver",
        hard_dependencies: NO_DEPS,
        order_dependencies: &["RawCleanup", "PNGConverter", "FeedbackDialog"],
    },
    PluginSpec {
        category: PluginCategory::PostProcessor,
        id: "MoveToSent",
        hard_dependencies: NO_DEPS,
        order_dependencies: &["ZIPArchiver"],
    },
    PluginSpec {
        category: PluginCategory::PostProcessor,
        id: "LogRotator",
        hard_dependencies: NO_DEPS,
        order_dependencies: &["SessionRecorder"],
    },
    PluginSpec {
        category: PluginCategory::PostProcessor,
        id: "RetentionManager",
        hard_dependencies: NO_DEPS,
        order_dependencies: &["ZIPArchiver", "MoveToSent"],
    },
    PluginSpec {
        category: PluginCategory::Notifier,
        id: "ConsoleNotifier",
        hard_dependencies: NO_DEPS,
        order_dependencies: NO_DEPS,
    },
    PluginSpec {
        category: PluginCategory::Notifier,
        id: "SystemNotification",
        hard_dependencies: NO_DEPS,
        order_dependencies: NO_DEPS,
    },
];

#[cfg(test)]
pub(crate) fn registered_plugin_spec(plugin_id: &str) -> Option<(PluginCategory, PluginGraphNode)> {
    PLUGIN_SPECS
        .iter()
        .find(|spec| spec.id == plugin_id)
        .map(|spec| {
            (
                spec.category,
                PluginGraphNode {
                    id: spec.id.to_string(),
                    hard_dependencies: spec
                        .hard_dependencies
                        .iter()
                        .map(|dependency| (*dependency).to_string())
                        .collect(),
                    order_dependencies: spec
                        .order_dependencies
                        .iter()
                        .map(|dependency| (*dependency).to_string())
                        .collect(),
                },
            )
        })
}

/// Owned graph node shared by configuration and runtime factory validation.
#[derive(Debug, Clone)]
pub(crate) struct PluginGraphNode {
    pub id: String,
    pub hard_dependencies: Vec<String>,
    pub order_dependencies: Vec<String>,
}

pub(crate) fn validate_plugin_graph(
    category: PluginCategory,
    nodes: &[PluginGraphNode],
) -> Result<(), ConfigValidationError> {
    validate_plugin_graph_with_policy(category, nodes, true)
}

/// Validate an enabled runtime subset. Order-only providers may be absent
/// because disabling them does not disable their dependents.
pub(crate) fn validate_runtime_plugin_graph(
    category: PluginCategory,
    nodes: &[PluginGraphNode],
) -> Result<(), ConfigValidationError> {
    validate_plugin_graph_with_policy(category, nodes, false)
}

fn validate_plugin_graph_with_policy(
    category: PluginCategory,
    nodes: &[PluginGraphNode],
    require_order_dependencies: bool,
) -> Result<(), ConfigValidationError> {
    let mut indexes = HashMap::with_capacity(nodes.len());
    for (index, node) in nodes.iter().enumerate() {
        if indexes.insert(node.id.as_str(), index).is_some() {
            return Err(ConfigValidationError::DuplicatePluginId {
                plugin_id: node.id.clone(),
                first_category: category,
                second_category: category,
            });
        }
    }

    for node in nodes {
        for dependency in &node.hard_dependencies {
            if !indexes.contains_key(dependency.as_str()) {
                return Err(ConfigValidationError::MissingDependency {
                    category,
                    plugin_id: node.id.clone(),
                    dependency: dependency.clone(),
                    kind: DependencyKind::Hard,
                });
            }
        }
        if require_order_dependencies {
            for dependency in &node.order_dependencies {
                if !indexes.contains_key(dependency.as_str()) {
                    return Err(ConfigValidationError::MissingDependency {
                        category,
                        plugin_id: node.id.clone(),
                        dependency: dependency.clone(),
                        kind: DependencyKind::OrderOnly,
                    });
                }
            }
        }
    }

    if let Some(cycle) = find_dependency_cycle(nodes, &indexes) {
        return Err(ConfigValidationError::DependencyCycle { category, cycle });
    }

    for (plugin_index, node) in nodes.iter().enumerate() {
        for (dependency, kind) in node
            .hard_dependencies
            .iter()
            .map(|dependency| (dependency, DependencyKind::Hard))
            .chain(
                node.order_dependencies
                    .iter()
                    .map(|dependency| (dependency, DependencyKind::OrderOnly)),
            )
        {
            let Some(&dependency_index) = indexes.get(dependency.as_str()) else {
                // Only enabled runtime subsets may omit an order-only provider.
                continue;
            };
            if dependency_index >= plugin_index {
                return Err(ConfigValidationError::InvalidDependencyOrder {
                    category,
                    plugin_id: node.id.clone(),
                    dependency: dependency.clone(),
                    kind,
                });
            }
        }
    }

    Ok(())
}

/// Validate every category as one registry. Plugin IDs are process-wide so
/// diagnostics and runtime hard-dependency checks cannot become ambiguous.
pub(crate) fn validate_plugin_registry(
    categories: &[(PluginCategory, Vec<PluginGraphNode>)],
) -> Result<(), ConfigValidationError> {
    validate_plugin_registry_with_policy(categories, true)
}

/// Validate the assembled enabled runtime subset. Its complete static
/// registry was validated before enablement was resolved, so an absent
/// order-only provider here means it was disabled rather than misspelled.
pub(crate) fn validate_runtime_plugin_registry(
    categories: &[(PluginCategory, Vec<PluginGraphNode>)],
) -> Result<(), ConfigValidationError> {
    validate_plugin_registry_with_policy(categories, false)
}

fn validate_plugin_registry_with_policy(
    categories: &[(PluginCategory, Vec<PluginGraphNode>)],
    require_order_dependencies: bool,
) -> Result<(), ConfigValidationError> {
    let mut ids: HashMap<&str, PluginCategory> = HashMap::new();
    for (category, nodes) in categories {
        for node in nodes {
            if let Some(first_category) = ids.insert(node.id.as_str(), *category) {
                return Err(ConfigValidationError::DuplicatePluginId {
                    plugin_id: node.id.clone(),
                    first_category,
                    second_category: *category,
                });
            }
        }
    }

    for (category, nodes) in categories {
        for node in nodes {
            for (dependency, kind) in node
                .hard_dependencies
                .iter()
                .map(|dependency| (dependency, DependencyKind::Hard))
                .chain(
                    node.order_dependencies
                        .iter()
                        .map(|dependency| (dependency, DependencyKind::OrderOnly)),
                )
            {
                if ids
                    .get(dependency.as_str())
                    .is_some_and(|dependency_category| dependency_category != category)
                {
                    return Err(ConfigValidationError::MissingDependency {
                        category: *category,
                        plugin_id: node.id.clone(),
                        dependency: dependency.clone(),
                        kind,
                    });
                }
            }
        }
        if require_order_dependencies {
            validate_plugin_graph(*category, nodes)?;
        } else {
            validate_runtime_plugin_graph(*category, nodes)?;
        }
    }

    Ok(())
}

fn find_dependency_cycle(
    nodes: &[PluginGraphNode],
    indexes: &HashMap<&str, usize>,
) -> Option<Vec<String>> {
    fn visit(
        index: usize,
        nodes: &[PluginGraphNode],
        indexes: &HashMap<&str, usize>,
        states: &mut [u8],
        stack: &mut Vec<usize>,
    ) -> Option<Vec<String>> {
        states[index] = 1;
        stack.push(index);

        for dependency in nodes[index]
            .hard_dependencies
            .iter()
            .chain(&nodes[index].order_dependencies)
        {
            let Some(&dependency_index) = indexes.get(dependency.as_str()) else {
                continue;
            };
            if states[dependency_index] == 0 {
                if let Some(cycle) = visit(dependency_index, nodes, indexes, states, stack) {
                    return Some(cycle);
                }
            } else if states[dependency_index] == 1 {
                let start = stack
                    .iter()
                    .position(|candidate| *candidate == dependency_index)
                    .unwrap_or(0);
                let mut cycle: Vec<String> = stack[start..]
                    .iter()
                    .map(|node_index| nodes[*node_index].id.clone())
                    .collect();
                cycle.push(nodes[dependency_index].id.clone());
                return Some(cycle);
            }
        }

        stack.pop();
        states[index] = 2;
        None
    }

    let mut states = vec![0; nodes.len()];
    let mut stack = Vec::new();
    for index in 0..nodes.len() {
        if states[index] == 0
            && let Some(cycle) = visit(index, nodes, indexes, &mut states, &mut stack)
        {
            return Some(cycle);
        }
    }
    None
}

// ═══════════════════════════════════════════════════
fn resolve_plugin_enablement(
    config: &CrashReporterConfig,
) -> Result<(BTreeSet<PluginId>, Vec<ConfigValidationDiagnostic>), ConfigValidationError> {
    let categories: Vec<(PluginCategory, Vec<PluginGraphNode>)> = [
        PluginCategory::Filter,
        PluginCategory::Collector,
        PluginCategory::PreProcessor,
        PluginCategory::PostProcessor,
        PluginCategory::Notifier,
    ]
    .into_iter()
    .map(|category| {
        let nodes = PLUGIN_SPECS
            .iter()
            .filter(|spec| spec.category == category)
            .map(|spec| PluginGraphNode {
                id: spec.id.to_string(),
                hard_dependencies: spec
                    .hard_dependencies
                    .iter()
                    .map(|dependency| (*dependency).to_string())
                    .collect(),
                order_dependencies: spec
                    .order_dependencies
                    .iter()
                    .map(|dependency| (*dependency).to_string())
                    .collect(),
            })
            .collect();
        (category, nodes)
    })
    .collect();
    validate_plugin_registry(&categories)?;

    if !config.enabled {
        return Ok((BTreeSet::new(), Vec::new()));
    }

    let mut diagnostics = sensitive_collector_diagnostics(config);
    let requested = configured_plugin_toggles(config);
    let enabled: BTreeSet<PluginId> = requested
        .into_iter()
        .filter_map(|(id, requested)| requested.then_some(id))
        .collect();
    let (enabled, mut dependency_diagnostics) = close_plugin_enablement(PLUGIN_SPECS, enabled);
    diagnostics.append(&mut dependency_diagnostics);
    Ok((enabled, diagnostics))
}

const SENSITIVE_COLLECTORS: [&str; 5] = [
    "MemoryCollector",
    "ScreenshotCollector",
    "AttachmentCollector",
    "EnvironmentCollector",
    "ProcessOutputCollector",
];

fn sensitive_collector_toggle(collectors: &CollectorConfig, plugin_id: &str) -> bool {
    match plugin_id {
        "MemoryCollector" => collectors.memory.enabled,
        "ScreenshotCollector" => collectors.screenshot.enabled,
        "AttachmentCollector" => collectors.attachment.enabled,
        "EnvironmentCollector" => collectors.environment.enabled,
        "ProcessOutputCollector" => collectors.process_output.enabled,
        _ => false,
    }
}

fn privacy_allows_collector(privacy: &PrivacyConfig, plugin_id: &str) -> bool {
    if privacy.consent != ConsentState::Granted {
        return false;
    }
    match privacy.level {
        PrivacyLevel::Minimal => false,
        PrivacyLevel::Diagnostic => plugin_id == "MemoryCollector",
        PrivacyLevel::Full => SENSITIVE_COLLECTORS.contains(&plugin_id),
    }
}

fn privacy_allows_diagnostic_evidence(privacy: &PrivacyConfig) -> bool {
    privacy.consent == ConsentState::Granted
        && matches!(privacy.level, PrivacyLevel::Diagnostic | PrivacyLevel::Full)
}

fn privacy_allows_full_evidence(privacy: &PrivacyConfig) -> bool {
    privacy.consent == ConsentState::Granted && privacy.level == PrivacyLevel::Full
}

fn sensitive_collector_enabled(
    config: &CrashReporterConfig,
    plugin_id: &str,
    toggle: bool,
) -> bool {
    is_enabled(config.enabled, config.collectors.enabled, toggle)
        && privacy_allows_collector(&config.privacy, plugin_id)
}

fn sensitive_collector_diagnostics(
    config: &CrashReporterConfig,
) -> Vec<ConfigValidationDiagnostic> {
    if !config.collectors.enabled {
        return Vec::new();
    }
    SENSITIVE_COLLECTORS
        .iter()
        .filter(|plugin_id| {
            sensitive_collector_toggle(&config.collectors, plugin_id)
                && !privacy_allows_collector(&config.privacy, plugin_id)
        })
        .map(
            |plugin_id| ConfigValidationDiagnostic::SensitiveCollectorDisabled {
                plugin_id: (*plugin_id).to_string(),
                level: config.privacy.level,
                consent: config.privacy.consent,
            },
        )
        .collect()
}

fn sensitive_evidence_diagnostics(config: &CrashReporterConfig) -> Vec<ConfigValidationDiagnostic> {
    if !config.enabled {
        return Vec::new();
    }

    let mut diagnostics = Vec::new();
    if config.collectors.enabled
        && config.collectors.thread.enabled
        && config.collectors.thread.stack_memory
        && !privacy_allows_diagnostic_evidence(&config.privacy)
    {
        diagnostics.push(ConfigValidationDiagnostic::SensitiveEvidenceDenied {
            evidence: "thread stack memory",
            level: config.privacy.level,
            consent: config.privacy.consent,
        });
    }
    if config.privacy.raw_shm && !privacy_allows_full_evidence(&config.privacy) {
        diagnostics.push(ConfigValidationDiagnostic::SensitiveEvidenceDenied {
            evidence: "raw shared-memory breadcrumbs/context",
            level: config.privacy.level,
            consent: config.privacy.consent,
        });
    }
    diagnostics
}

fn resolve_collection_policy(
    config: &CrashReporterConfig,
    enabled_plugins: &BTreeSet<PluginId>,
) -> CollectionPolicy {
    if !config.enabled {
        return CollectionPolicy::MINIMAL;
    }

    CollectionPolicy {
        capture_stack_memory: config.collectors.enabled
            && config.collectors.thread.enabled
            && config.collectors.thread.stack_memory
            && privacy_allows_diagnostic_evidence(&config.privacy),
        capture_shm_screenshots: enabled_plugins.contains(&PluginId::new("ScreenshotCollector")),
        capture_shm_attachments: enabled_plugins.contains(&PluginId::new("AttachmentCollector")),
        persist_raw_shm: config.privacy.raw_shm && privacy_allows_full_evidence(&config.privacy),
    }
}

fn close_plugin_enablement<T>(
    specs: &[PluginSpec],
    mut enabled: BTreeSet<T>,
) -> (BTreeSet<T>, Vec<ConfigValidationDiagnostic>)
where
    T: Copy + Ord + AsRef<str>,
{
    let mut diagnostics = Vec::new();

    // Repeated removal computes the greatest hard-dependency-closed subset of
    // the requested plugins. An explicit opt-out is never reversed.
    loop {
        let mut removed = None;
        for spec in specs {
            let Some(enabled_id) = enabled
                .iter()
                .find(|enabled| enabled.as_ref() == spec.id)
                .copied()
            else {
                continue;
            };
            if let Some(dependency) = spec.hard_dependencies.iter().find(|dependency| {
                !enabled
                    .iter()
                    .any(|enabled| enabled.as_ref() == **dependency)
            }) {
                removed = Some((*spec, enabled_id, *dependency));
                break;
            }
        }
        let Some((spec, enabled_id, dependency)) = removed else {
            break;
        };
        enabled.remove(&enabled_id);
        diagnostics.push(ConfigValidationDiagnostic::DependentDisabled {
            category: spec.category,
            plugin_id: spec.id.to_string(),
            dependency: dependency.to_string(),
        });
    }

    (enabled, diagnostics)
}

#[allow(clippy::too_many_lines)] // explicit mapping is the config/plugin SSOT boundary
fn configured_plugin_toggles(config: &CrashReporterConfig) -> Vec<(PluginId, bool)> {
    vec![
        (
            PluginId::new("DiskSpaceFilter"),
            config.filters.enabled && config.filters.disk_space.enabled,
        ),
        (
            PluginId::new("RateLimiter"),
            config.filters.enabled && config.filters.rate_limiter.enabled,
        ),
        (
            PluginId::new("ThreadCollector"),
            config.collectors.enabled && config.collectors.thread.enabled,
        ),
        (
            PluginId::new("BreadcrumbCollector"),
            config.collectors.enabled && config.collectors.breadcrumb.enabled,
        ),
        (
            PluginId::new("ContextCollector"),
            config.collectors.enabled && config.collectors.context.enabled,
        ),
        (
            PluginId::new("MemoryCollector"),
            sensitive_collector_enabled(
                config,
                "MemoryCollector",
                config.collectors.memory.enabled,
            ),
        ),
        (
            PluginId::new("DylibCollector"),
            config.collectors.enabled && config.collectors.dylib.enabled,
        ),
        (
            PluginId::new("ScreenshotCollector"),
            sensitive_collector_enabled(
                config,
                "ScreenshotCollector",
                config.collectors.screenshot.enabled,
            ),
        ),
        (
            PluginId::new("AttachmentCollector"),
            sensitive_collector_enabled(
                config,
                "AttachmentCollector",
                config.collectors.attachment.enabled,
            ),
        ),
        (
            PluginId::new("EnvironmentCollector"),
            sensitive_collector_enabled(
                config,
                "EnvironmentCollector",
                config.collectors.environment.enabled,
            ),
        ),
        (
            PluginId::new("ProcessOutputCollector"),
            sensitive_collector_enabled(
                config,
                "ProcessOutputCollector",
                config.collectors.process_output.enabled,
            ),
        ),
        (
            PluginId::new("SessionEnricher"),
            config.pre_processors.enabled && config.pre_processors.session.enabled,
        ),
        (
            PluginId::new("SymbolResolver"),
            config.pre_processors.enabled && config.pre_processors.symbolizer.enabled,
        ),
        (
            PluginId::new("Fingerprinter"),
            config.pre_processors.enabled && config.pre_processors.fingerprint.enabled,
        ),
        (
            PluginId::new("BuildInfoEnricher"),
            config.pre_processors.enabled && config.pre_processors.build_info.enabled,
        ),
        (
            PluginId::new("DuplicateDetector"),
            config.pre_processors.enabled && config.pre_processors.duplicate.enabled,
        ),
        (
            PluginId::new("Sanitizer"),
            config.pre_processors.enabled && config.pre_processors.sanitizer.enabled,
        ),
        (
            PluginId::new("RawCleanup"),
            config.post_processors.enabled && config.post_processors.raw_cleanup.enabled,
        ),
        (
            PluginId::new("SessionRecorder"),
            config.post_processors.enabled && config.post_processors.session_recorder.enabled,
        ),
        (
            PluginId::new("PNGConverter"),
            config.post_processors.enabled && config.post_processors.png_converter.enabled,
        ),
        (
            PluginId::new("FeedbackDialog"),
            config.post_processors.enabled && config.post_processors.feedback_dialog.enabled,
        ),
        (
            PluginId::new("ZIPArchiver"),
            config.post_processors.enabled && config.post_processors.zip_archiver.enabled,
        ),
        (
            PluginId::new("MoveToSent"),
            config.post_processors.enabled && config.post_processors.move_to_sent.enabled,
        ),
        (
            PluginId::new("LogRotator"),
            config.post_processors.enabled && config.post_processors.log_rotator.enabled,
        ),
        (
            PluginId::new("RetentionManager"),
            config.post_processors.enabled && config.post_processors.retention.enabled,
        ),
        (
            PluginId::new("ConsoleNotifier"),
            config.notifiers.enabled && config.notifiers.console.enabled,
        ),
        (
            PluginId::new("SystemNotification"),
            config.notifiers.enabled && config.notifiers.system_notification.enabled,
        ),
    ]
}

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
/// A missing file selects [`CrashReporterConfig::default`]. Existing files
/// must be readable and valid JSON.
///
/// # Errors
/// Returns [`ConfigLoadError`] for data-directory, read, or parse failures.
pub fn load_config() -> Result<CrashReporterConfig, ConfigLoadError> {
    load_config_from_data_dir()
}

/// Load and normalize the runtime configuration exactly once.
///
/// # Errors
/// Returns [`ConfigLoadError`] for an existing unreadable/malformed file,
/// unavailable required encryption, or an invalid plugin registry.
pub fn load_validated_config() -> Result<ValidatedConfig, ConfigLoadError> {
    load_config()?.validate().map_err(ConfigLoadError::from)
}

/// Load and validate a specific file through the exact startup path.
///
/// # Errors
/// Returns explicit safety, read, parse, or validation errors.
pub fn load_validated_config_from_path(path: &Path) -> Result<ValidatedConfig, ConfigLoadError> {
    load_config_from_path(path)?.validate().map_err(Into::into)
}

fn load_config_from_data_dir() -> Result<CrashReporterConfig, ConfigLoadError> {
    let data_dir = paths::data_dir().map_err(ConfigLoadError::DataDirectory)?;
    let path = data_dir.join(CONFIG_FILENAME);
    load_config_from_path(&path)
}

fn load_config_from_path(path: &Path) -> Result<CrashReporterConfig, ConfigLoadError> {
    let mut file = match paths::open_private_file_optional(path) {
        Ok(Some(file)) => file,
        Ok(None) => return Ok(CrashReporterConfig::default()),
        Err(reason) => {
            return Err(ConfigLoadError::UnsafeFile {
                path: path.to_path_buf(),
                reason,
            });
        }
    };
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| ConfigLoadError::Read {
            path: path.to_path_buf(),
            error: error.to_string(),
        })?;
    serde_json::from_slice(&bytes).map_err(|error| ConfigLoadError::Parse {
        path: path.to_path_buf(),
        error: error.to_string(),
    })
}

#[cfg(test)]
#[path = "../tests/unit/config_tests.rs"]
mod tests;
