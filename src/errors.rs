//! Typed failures at the monitor's domain boundaries.

macro_rules! domain_error {
    ($name:ident, $domain:literal) => {
        #[derive(Clone, Debug, PartialEq, Eq)]
        pub struct $name(String);

        impl $name {
            #[must_use]
            pub fn new(message: impl Into<String>) -> Self {
                Self(message.into())
            }

            #[must_use]
            pub fn message(&self) -> &str {
                &self.0
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(formatter, "{}: {}", $domain, self.0)
            }
        }

        impl std::error::Error for $name {}

        impl std::ops::Deref for $name {
            type Target = str;

            fn deref(&self) -> &Self::Target {
                &self.0
            }
        }

        impl PartialEq<&str> for $name {
            fn eq(&self, other: &&str) -> bool {
                self.0 == *other
            }
        }

        impl From<String> for $name {
            fn from(message: String) -> Self {
                Self(message)
            }
        }

        impl From<&str> for $name {
            fn from(message: &str) -> Self {
                Self(message.to_string())
            }
        }
    };
}

domain_error!(PlatformError, "platform");
domain_error!(PluginError, "plugin");
domain_error!(PathError, "path");
domain_error!(ShmError, "shared-memory");
domain_error!(ArtifactError, "artifact");

#[cfg(test)]
mod tests {
    use super::{ArtifactError, PathError, PlatformError, PluginError, ShmError};

    #[test]
    fn domain_errors_preserve_identity_and_original_message() {
        let failures: Vec<Box<dyn std::error::Error>> = vec![
            Box::new(PlatformError::new("denied")),
            Box::new(PluginError::new("denied")),
            Box::new(PathError::new("denied")),
            Box::new(ShmError::new("denied")),
            Box::new(ArtifactError::new("denied")),
        ];
        let rendered: Vec<String> = failures.iter().map(ToString::to_string).collect();

        assert_eq!(
            rendered,
            [
                "platform: denied",
                "plugin: denied",
                "path: denied",
                "shared-memory: denied",
                "artifact: denied",
            ]
        );
    }
}
