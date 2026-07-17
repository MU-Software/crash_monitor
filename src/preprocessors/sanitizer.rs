//! Pre-processor: mask sensitive information (usernames) in collected data.
//!
//! Must run AFTER `Fingerprinter` so the fingerprint is computed on
//! unsanitized data, producing stable hashes for identical crashes.

use crate::pipeline::{CollectedData, CrashEvent, Plugin, PreProcessor, Priority};

pub struct Sanitizer {
    /// The username to mask, detected from `$USER`.
    pub(crate) username: Option<String>,
}

impl Sanitizer {
    #[must_use]
    pub fn new() -> Self {
        Self {
            username: std::env::var("USER").ok().filter(|u| !u.is_empty()),
        }
    }

    fn sanitize_str(&self, s: &mut String) {
        if let Some(ref user) = self.username {
            let pattern = format!("/Users/{user}/");
            if s.contains(&pattern) {
                *s = s.replace(&pattern, "/Users/[USERNAME]/");
            }
        }
    }
}

impl Plugin for Sanitizer {
    fn name(&self) -> &'static str {
        "Sanitizer"
    }
    fn priority(&self) -> Priority {
        Priority::Low
    }
    fn depends_on(&self) -> &'static [&'static str] {
        &["Fingerprinter"]
    }
}

impl PreProcessor for Sanitizer {
    fn process(&self, _event: &CrashEvent, data: &mut CollectedData) -> Result<(), String> {
        if self.username.is_none() {
            return Ok(());
        }

        // Sanitize image paths
        for img in &mut data.raw.images {
            self.sanitize_str(&mut img.path);
        }

        // Sanitize symbol names in-place
        for sym in data.raw.symbols.values_mut() {
            self.sanitize_str(sym);
        }

        // Sanitize environment variable values
        if let Some(ref mut env) = data.raw.environment {
            for (_, val) in &mut env.env_vars {
                self.sanitize_str(val);
            }
        }

        Ok(())
    }
}

#[cfg(test)]
#[path = "../../tests/unit/preprocessors/sanitizer_tests.rs"]
mod tests;
