//! Pre-processor: mask sensitive information (usernames) in collected data.
//!
//! Must run AFTER `Fingerprinter` so the fingerprint is computed on
//! unsanitized data, producing stable hashes for identical crashes.

use crate::pipeline::{
    CollectedData, CrashEvent, Plugin, PluginContext, PluginExecution, PreProcessor, Priority,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

pub struct Sanitizer {
    /// Username derived from `$USER`, falling back to the final `$HOME` component.
    pub(crate) username: Option<String>,
    home: Option<String>,
}

impl Sanitizer {
    #[must_use]
    pub fn new() -> Self {
        Self::from_identity(std::env::var("USER").ok(), std::env::var("HOME").ok())
    }

    fn from_identity(user: Option<String>, home: Option<String>) -> Self {
        let home = home.filter(|value| !value.is_empty());
        let username = user.filter(|value| !value.is_empty()).or_else(|| {
            home.as_deref()
                .and_then(|path| path.trim_end_matches('/').rsplit('/').next())
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        });
        Self { username, home }
    }

    pub(crate) fn sanitize_str(&self, value: &mut String) {
        if let Some(home) = &self.home {
            *value = replace_path_prefix(value, home, "[HOME]");
        }
        if let Some(user) = &self.username {
            let user_home = format!("/Users/{user}");
            *value = replace_path_prefix(value, &user_home, "/Users/[USERNAME]");
            if value.eq_ignore_ascii_case(user) {
                *value = "[USERNAME]".to_string();
            }
        }
    }

    pub(crate) fn sanitize_json_value(&self, value: &mut serde_json::Value) {
        match value {
            serde_json::Value::String(text) => self.sanitize_str(text),
            serde_json::Value::Array(values) => {
                for value in values {
                    self.sanitize_json_value(value);
                }
            }
            serde_json::Value::Object(values) => {
                for value in values.values_mut() {
                    self.sanitize_json_value(value);
                }
            }
            serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            }
        }
    }

    pub(crate) fn sanitize_serializable<T>(&self, value: &mut T) -> Result<(), String>
    where
        T: Serialize + DeserializeOwned,
    {
        let mut json = serde_json::to_value(&*value)
            .map_err(|error| format!("privacy sanitizer encode failed: {error}"))?;
        self.sanitize_json_value(&mut json);
        *value = serde_json::from_value(json)
            .map_err(|error| format!("privacy sanitizer decode failed: {error}"))?;
        Ok(())
    }
}

fn replace_path_prefix(value: &str, prefix: &str, replacement: &str) -> String {
    if prefix.is_empty() {
        return value.to_string();
    }
    let folded_value = value.to_ascii_lowercase();
    let folded_prefix = prefix.to_ascii_lowercase();
    let mut output = String::with_capacity(value.len());
    let mut cursor = 0;
    while let Some(relative) = folded_value[cursor..].find(&folded_prefix) {
        let start = cursor + relative;
        let end = start + prefix.len();
        let component_boundary = end == value.len() || value.as_bytes().get(end) == Some(&b'/');
        if component_boundary {
            output.push_str(&value[cursor..start]);
            output.push_str(replacement);
            cursor = end;
        } else {
            output.push_str(&value[cursor..end]);
            cursor = end;
        }
    }
    output.push_str(&value[cursor..]);
    output
}

impl Plugin for Sanitizer {
    fn name(&self) -> &'static str {
        "Sanitizer"
    }
    fn execution(&self) -> PluginExecution {
        PluginExecution::Cooperative
    }
    fn priority(&self) -> Priority {
        Priority::Low
    }
    fn order_after(&self) -> &'static [&'static str] {
        &["Fingerprinter"]
    }
}

impl PreProcessor for Sanitizer {
    fn process(
        &self,
        _event: &CrashEvent,
        data: &mut CollectedData,
        context: &PluginContext,
    ) -> Result<(), String> {
        context.checkpoint()?;
        if self.username.is_none() && self.home.is_none() {
            return Ok(());
        }

        for thread in &mut data.raw.threads {
            if let Some(name) = &mut thread.name {
                self.sanitize_str(name);
            }
        }

        // Sanitize image paths
        for img in &mut data.raw.images {
            context.checkpoint()?;
            self.sanitize_str(&mut img.path);
        }

        // Sanitize symbol names in-place
        for sym in data.raw.symbols.values_mut() {
            context.checkpoint()?;
            self.sanitize_str(sym);
        }

        for crumb in &mut data.raw.breadcrumbs {
            context.checkpoint()?;
            self.sanitize_str(&mut crumb.file);
            self.sanitize_str(&mut crumb.message);
        }

        if let Some(crash_context) = &mut data.raw.crash_context {
            for (key, value) in &mut crash_context.annotations {
                context.checkpoint()?;
                self.sanitize_str(key);
                self.sanitize_str(value);
            }
            self.sanitize_str(&mut crash_context.session_id);
            self.sanitize_str(&mut crash_context.build_type);
            self.sanitize_str(&mut crash_context.build_preset);
            self.sanitize_str(&mut crash_context.compiler);
            self.sanitize_str(&mut crash_context.os_version);
        }

        if let Some(settings) = &mut data.raw.settings_snapshot {
            self.sanitize_str(&mut settings.extra);
        }

        for attachment in &mut data.raw.attachment_registrations {
            context.checkpoint()?;
            self.sanitize_str(&mut attachment.label);
            self.sanitize_str(&mut attachment.path);
        }
        for attachment in &mut data.raw.attachments {
            context.checkpoint()?;
            self.sanitize_str(&mut attachment.label);
            self.sanitize_str(&mut attachment.original_path);
        }

        // Sanitize environment variable values
        if let Some(ref mut env) = data.raw.environment {
            for (_, val) in &mut env.env_vars {
                context.checkpoint()?;
                self.sanitize_str(val);
            }
            self.sanitize_str(&mut env.hostname);
        }

        if let Some(output) = &mut data.raw.process_output {
            self.sanitize_str(&mut output.stdout.tail);
            self.sanitize_str(&mut output.stderr.tail);
            if let Some(error) = &mut output.stdout.read_error {
                self.sanitize_str(error);
            }
            if let Some(error) = &mut output.stderr.read_error {
                self.sanitize_str(error);
            }
        }

        context.checkpoint()?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "../../tests/unit/preprocessors/sanitizer_tests.rs"]
mod tests;
