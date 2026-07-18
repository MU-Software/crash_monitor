//! Collector: capture environment variables and system information.

use crate::pipeline::traits::Collector;
use crate::pipeline::{
    CollectedData, CrashEvent, Plugin, PluginContext, PluginExecution, Priority,
};
use mach2::port::mach_port_t;
use nix::sys::utsname::uname;
use std::ffi::CString;
use std::sync::Arc;

/// Minimal diagnostic allowlist. Everything else, including URL/DSN/cookie,
/// credential and key-material variables, is excluded by default.
const ALLOWED_ENV_KEYS: &[&str] = &["LANG", "TERM", "TZ"];

/// Raw environment data captured by the collector.
pub struct RawEnvironment {
    pub kernel_release: String,
    pub kernel_version: String,
    pub arch: String,
    pub hostname: String,
    pub variables_source: &'static str,
    pub env_vars: Vec<(String, String)>,
}

/// Immutable byte-for-byte snapshot of the environment passed to the child.
///
/// Keeping the POSIX `key=value` entries as bytes preserves the exact spawn
/// input. The JSON-oriented collector safely skips entries that cannot be
/// represented as UTF-8 instead of accidentally reporting the monitor's own
/// environment or corrupting the child's values.
#[derive(Debug, Clone, Default)]
pub struct ChildEnvironmentSnapshot {
    entries: Vec<Vec<u8>>,
}

impl ChildEnvironmentSnapshot {
    #[must_use]
    pub fn from_c_strings(environment: &[CString]) -> Self {
        Self {
            entries: environment
                .iter()
                .map(|entry| entry.as_bytes().to_vec())
                .collect(),
        }
    }

    fn reportable_entries(&self) -> impl Iterator<Item = (&str, &str)> {
        self.entries.iter().filter_map(|entry| {
            let separator = entry.iter().position(|byte| *byte == b'=')?;
            let key = std::str::from_utf8(&entry[..separator]).ok()?;
            let value = std::str::from_utf8(&entry[separator + 1..]).ok()?;
            Some((key, value))
        })
    }
}

pub struct EnvironmentCollector {
    child_environment: Option<Arc<ChildEnvironmentSnapshot>>,
}

impl EnvironmentCollector {
    #[must_use]
    pub fn new() -> Self {
        Self {
            child_environment: None,
        }
    }

    #[must_use]
    pub fn with_child_environment(environment: Arc<ChildEnvironmentSnapshot>) -> Self {
        Self {
            child_environment: Some(environment),
        }
    }
}

impl Plugin for EnvironmentCollector {
    fn name(&self) -> &'static str {
        "EnvironmentCollector"
    }
    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }
    fn priority(&self) -> Priority {
        Priority::Low
    }
}

impl Collector for EnvironmentCollector {
    fn collect(
        &self,
        _event: &CrashEvent,
        _task: mach_port_t,
        data: &mut CollectedData,
        context: &PluginContext,
    ) -> Result<(), String> {
        context.checkpoint()?;
        let (kernel_release, kernel_version, arch) = match uname() {
            Ok(info) => (
                info.release().to_string_lossy().into_owned(),
                info.version().to_string_lossy().into_owned(),
                info.machine().to_string_lossy().into_owned(),
            ),
            Err(_) => (String::new(), String::new(), String::new()),
        };

        // Hostnames commonly contain a person's name or managed-device ID.
        // Keep the schema stable while making exclusion explicit.
        let hostname = "[REDACTED]".to_string();

        context.checkpoint()?;
        let mut env_vars = Vec::new();
        let variables_source;
        if let Some(environment) = &self.child_environment {
            variables_source = "spawn_environment_snapshot";
            for (key, value) in environment.reportable_entries() {
                context.checkpoint()?;
                if is_allowed(key) {
                    env_vars.push((key.to_string(), value.to_string()));
                }
            }
        } else {
            // Never present the monitor's environment as target data. Library
            // callers that do not provide the spawn snapshot get an explicitly
            // unavailable source and an empty variable map.
            variables_source = "unavailable";
        }

        data.raw.environment = Some(RawEnvironment {
            kernel_release,
            kernel_version,
            arch,
            hostname,
            variables_source,
            env_vars,
        });

        context.checkpoint()?;
        Ok(())
    }
}

fn is_allowed(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    ALLOWED_ENV_KEYS.contains(&upper.as_str()) || upper.starts_with("LC_")
}

#[cfg(test)]
#[path = "../../tests/unit/collectors/environment_tests.rs"]
mod tests;
