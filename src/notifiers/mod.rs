//! Notifier plugins — fire-and-forget notifications after report generation.

pub mod console;
pub mod system;

pub use console::ConsoleNotifier;
pub use system::SystemNotification;
