//! Pre-processor plugins — enrich collected data after resume.

pub mod build_info;
pub mod duplicate;
pub mod fingerprint;
pub mod report_formatter;
pub mod sanitizer;
pub mod session;
pub mod symbolicate;

pub use build_info::BuildInfoEnricher;
pub use duplicate::DuplicateDetector;
pub use fingerprint::Fingerprinter;
pub use sanitizer::Sanitizer;
pub use session::SessionEnricher;
pub use symbolicate::SymbolResolver;
