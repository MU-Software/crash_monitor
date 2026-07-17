//! Configuration system for crash reporter plugins (opt-out design).
//!
//! Report triggers and most plugins are enabled by default. The config file
//! (`crash_reporter.json`) is only needed to disable specific behavior, enable
//! an opt-in plugin, or adjust parameters. Missing files or parse errors
//! silently fall back to defaults.

use serde::Deserialize;
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::Path;

use crate::pipeline::types::{DependencyKind, PluginCategory};
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
    enabled_plugins: BTreeSet<&'static str>,
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
    pub(crate) fn plugin_enabled(&self, plugin_id: &str) -> bool {
        self.enabled && self.enabled_plugins.contains(plugin_id)
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
        }
    }
}

/// Fatal plugin graph validation failures. These are returned to startup;
/// invalid plugin combinations never use `panic!` as control flow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigValidationError {
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
    /// Returns a structured error when the built-in plugin registry contains
    /// duplicate IDs, a missing hard dependency, or a dependency cycle/order
    /// violation.
    pub fn validate(self) -> Result<ValidatedConfig, ConfigValidationError> {
        let trigger_category_enabled = self.triggers.enabled;
        let triggers = ValidatedTriggersConfig {
            crash: trigger_category_enabled && self.triggers.crash.enabled,
            exit_failure: trigger_category_enabled && self.triggers.exit_failure.enabled,
            signal_failure: trigger_category_enabled && self.triggers.signal_failure.enabled,
            probable_oom: trigger_category_enabled && self.triggers.oom_detection.enabled,
            anr: trigger_category_enabled && self.triggers.anr.enabled,
            snapshot: trigger_category_enabled && self.triggers.snapshot.enabled,
        };
        let (enabled_plugins, diagnostics) = resolve_plugin_enablement(&self)?;
        Ok(ValidatedConfig {
            enabled: self.enabled,
            triggers,
            diagnostics,
            enabled_plugins,
            config: self,
        })
    }
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
#[serde(default)]
pub struct TriggersConfig {
    pub enabled: bool,
    pub crash: PluginToggle,
    pub exit_failure: PluginToggle,
    pub signal_failure: PluginToggle,
    pub oom_detection: PluginToggle,
    pub anr: PluginToggle,
    pub snapshot: PluginToggle,
}

impl Default for TriggersConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            crash: PluginToggle::default(),
            exit_failure: PluginToggle::default(),
            signal_failure: PluginToggle::default(),
            oom_detection: PluginToggle::default(),
            anr: PluginToggle::default(),
            snapshot: PluginToggle::default(),
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
        validate_plugin_graph_with_policy(*category, nodes, require_order_dependencies)?;
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
) -> Result<(BTreeSet<&'static str>, Vec<ConfigValidationDiagnostic>), ConfigValidationError> {
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

    let requested = configured_plugin_toggles(config);
    let enabled: BTreeSet<&'static str> = requested
        .into_iter()
        .filter_map(|(id, requested)| requested.then_some(id))
        .collect();
    Ok(close_plugin_enablement(PLUGIN_SPECS, enabled))
}

fn close_plugin_enablement(
    specs: &[PluginSpec],
    mut enabled: BTreeSet<&'static str>,
) -> (BTreeSet<&'static str>, Vec<ConfigValidationDiagnostic>) {
    let mut diagnostics = Vec::new();

    // Repeated removal computes the greatest hard-dependency-closed subset of
    // the requested plugins. An explicit opt-out is never reversed.
    loop {
        let mut removed = None;
        for spec in specs {
            if !enabled.contains(spec.id) {
                continue;
            }
            if let Some(dependency) = spec
                .hard_dependencies
                .iter()
                .find(|dependency| !enabled.contains(**dependency))
            {
                removed = Some((*spec, *dependency));
                break;
            }
        }
        let Some((spec, dependency)) = removed else {
            break;
        };
        enabled.remove(spec.id);
        diagnostics.push(ConfigValidationDiagnostic::DependentDisabled {
            category: spec.category,
            plugin_id: spec.id.to_string(),
            dependency: dependency.to_string(),
        });
    }

    (enabled, diagnostics)
}

#[allow(clippy::too_many_lines)] // explicit mapping is the config/plugin SSOT boundary
fn configured_plugin_toggles(config: &CrashReporterConfig) -> Vec<(&'static str, bool)> {
    vec![
        (
            "DiskSpaceFilter",
            config.filters.enabled && config.filters.disk_space.enabled,
        ),
        (
            "RateLimiter",
            config.filters.enabled && config.filters.rate_limiter.enabled,
        ),
        (
            "ThreadCollector",
            config.collectors.enabled && config.collectors.thread.enabled,
        ),
        (
            "BreadcrumbCollector",
            config.collectors.enabled && config.collectors.breadcrumb.enabled,
        ),
        (
            "ContextCollector",
            config.collectors.enabled && config.collectors.context.enabled,
        ),
        (
            "MemoryCollector",
            config.collectors.enabled && config.collectors.memory.enabled,
        ),
        (
            "DylibCollector",
            config.collectors.enabled && config.collectors.dylib.enabled,
        ),
        (
            "ScreenshotCollector",
            config.collectors.enabled && config.collectors.screenshot.enabled,
        ),
        (
            "AttachmentCollector",
            config.collectors.enabled && config.collectors.attachment.enabled,
        ),
        (
            "EnvironmentCollector",
            config.collectors.enabled && config.collectors.environment.enabled,
        ),
        (
            "SessionEnricher",
            config.pre_processors.enabled && config.pre_processors.session.enabled,
        ),
        (
            "SymbolResolver",
            config.pre_processors.enabled && config.pre_processors.symbolizer.enabled,
        ),
        (
            "Fingerprinter",
            config.pre_processors.enabled && config.pre_processors.fingerprint.enabled,
        ),
        (
            "BuildInfoEnricher",
            config.pre_processors.enabled && config.pre_processors.build_info.enabled,
        ),
        (
            "DuplicateDetector",
            config.pre_processors.enabled && config.pre_processors.duplicate.enabled,
        ),
        (
            "Sanitizer",
            config.pre_processors.enabled && config.pre_processors.sanitizer.enabled,
        ),
        (
            "RawCleanup",
            config.post_processors.enabled && config.post_processors.raw_cleanup.enabled,
        ),
        (
            "SessionRecorder",
            config.post_processors.enabled && config.post_processors.session_recorder.enabled,
        ),
        (
            "PNGConverter",
            config.post_processors.enabled && config.post_processors.png_converter.enabled,
        ),
        (
            "FeedbackDialog",
            config.post_processors.enabled && config.post_processors.feedback_dialog.enabled,
        ),
        (
            "ZIPArchiver",
            config.post_processors.enabled && config.post_processors.zip_archiver.enabled,
        ),
        (
            "MoveToSent",
            config.post_processors.enabled && config.post_processors.move_to_sent.enabled,
        ),
        (
            "LogRotator",
            config.post_processors.enabled && config.post_processors.log_rotator.enabled,
        ),
        (
            "RetentionManager",
            config.post_processors.enabled && config.post_processors.retention.enabled,
        ),
        (
            "ConsoleNotifier",
            config.notifiers.enabled && config.notifiers.console.enabled,
        ),
        (
            "SystemNotification",
            config.notifiers.enabled && config.notifiers.system_notification.enabled,
        ),
    ]
}

//  Helpers
// ═══════════════════════════════════════════════════

/// Three-level AND check: global → category → plugin.
#[must_use]
#[allow(dead_code)] // retained as the public hierarchical-toggle helper
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

/// Load and normalize the runtime configuration exactly once.
///
/// # Errors
/// Returns [`ConfigValidationError`] if the plugin registry is not globally
/// unique or contains an invalid dependency graph.
pub fn load_validated_config() -> Result<ValidatedConfig, ConfigValidationError> {
    load_config().validate()
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
