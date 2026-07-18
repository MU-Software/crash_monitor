//! Collector: capture environment variables and system information.

use crate::pipeline::traits::Collector;
use crate::pipeline::{
    CollectedData, CrashEvent, Plugin, PluginContext, PluginExecution, Priority,
};
use mach2::port::mach_port_t;
use nix::sys::utsname::uname;
use std::ffi::CString;
use std::sync::Arc;

/// Sensitive environment variable name patterns (case-insensitive).
const SENSITIVE_PATTERNS: &[&str] = &["TOKEN", "SECRET", "KEY", "PASSWORD", "CREDENTIAL", "AUTH"];

/// Raw environment data captured by the collector.
pub struct RawEnvironment {
    pub os_version: String,
    pub os_build: String,
    pub arch: String,
    pub hostname: String,
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
        let (os_version, os_build, arch) = match uname() {
            Ok(info) => (
                info.release().to_string_lossy().into_owned(),
                info.version().to_string_lossy().into_owned(),
                info.machine().to_string_lossy().into_owned(),
            ),
            Err(_) => (String::new(), String::new(), String::new()),
        };

        let hostname = nix::unistd::gethostname()
            .map(|h| h.to_string_lossy().into_owned())
            .unwrap_or_default();

        context.checkpoint()?;
        let mut env_vars = Vec::new();
        if let Some(environment) = &self.child_environment {
            for (key, value) in environment.reportable_entries() {
                context.checkpoint()?;
                if !is_sensitive(key) {
                    env_vars.push((key.to_string(), value.to_string()));
                }
            }
        } else {
            // Retained for library callers and unit tests that do not spawn a
            // child. Production injects the final posix_spawn environment.
            for (key, value) in std::env::vars() {
                context.checkpoint()?;
                if !is_sensitive(&key) {
                    env_vars.push((key, value));
                }
            }
        }

        data.raw.environment = Some(RawEnvironment {
            os_version,
            os_build,
            arch,
            hostname,
            env_vars,
        });

        context.checkpoint()?;
        Ok(())
    }
}

fn is_sensitive(key: &str) -> bool {
    let upper = key.to_uppercase();
    SENSITIVE_PATTERNS.iter().any(|pat| upper.contains(pat))
}

#[cfg(test)]
#[path = "../../tests/unit/collectors/environment_tests.rs"]
mod tests;
