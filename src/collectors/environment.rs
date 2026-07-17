//! Collector: capture environment variables and system information.

use crate::pipeline::traits::Collector;
use crate::pipeline::{
    CollectedData, CrashEvent, Plugin, PluginContext, PluginExecution, Priority,
};
use mach2::port::mach_port_t;
use nix::sys::utsname::uname;

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

pub struct EnvironmentCollector;

impl EnvironmentCollector {
    #[must_use]
    pub fn new() -> Self {
        Self
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
        for (key, value) in std::env::vars() {
            context.checkpoint()?;
            if !is_sensitive(&key) {
                env_vars.push((key, value));
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
