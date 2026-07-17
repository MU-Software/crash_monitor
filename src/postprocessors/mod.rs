//! Post-processor plugins — operate on the written report file.

pub mod feedback;
pub mod log_rotator;
pub mod move_to_sent;
pub mod png_converter;
mod raw_cleanup;
pub mod retention;
mod session_recorder;
pub mod zip_archiver;

pub use feedback::FeedbackPostProcessor;
pub use log_rotator::LogRotator;
pub use move_to_sent::MoveToSent;
pub use png_converter::PNGConverter;
pub use raw_cleanup::RawCleanup;
pub use retention::RetentionManager;
pub use session_recorder::SessionRecorder;
pub use zip_archiver::ZIPArchiver;
