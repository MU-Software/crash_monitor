//! Filter plugins — gate whether the pipeline should process an event.

pub mod disk_space;
pub mod rate_limiter;

pub use disk_space::DiskSpaceFilter;
pub use rate_limiter::RateLimiter;
