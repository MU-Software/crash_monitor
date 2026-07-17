//! Tests for the data dir override.
//!
//! These tests rely on `.cargo/config.toml` setting `MBB_CRASH_DATA_DIR` to
//! `target/test-crash-data` for all `cargo test` invocations. They never mutate
//! the env var (which would race with parallel tests) — they only assert that
//! the override is in effect, so accidental fallback to the user's home
//! directory is impossible during testing.

use super::*;

#[test]
fn test_data_dir_override_env_is_set_during_cargo_test() {
    // .cargo/config.toml MUST set MBB_CRASH_DATA_DIR for all cargo invocations.
    // If this assertion fails, the cargo config is missing or broken — and any
    // test that triggers `pending_dir()` could leak files to the user's home.
    let val = std::env::var("MBB_CRASH_DATA_DIR").expect(
        "MBB_CRASH_DATA_DIR must be set by .cargo/config.toml during cargo test \
         to prevent test fixtures from polluting ~/.modelblockbuilder/",
    );
    assert!(!val.is_empty(), "MBB_CRASH_DATA_DIR must not be empty");
}

#[test]
fn test_data_dir_uses_override_not_home() {
    let dir = data_dir().expect("data_dir() must succeed under override");
    let s = dir.to_string_lossy();
    assert!(
        !s.contains(".modelblockbuilder"),
        "data_dir() must NOT use ~/.modelblockbuilder during tests, got: {s}"
    );
    assert!(
        s.contains("test-crash-data"),
        "data_dir() should resolve under target/test-crash-data, got: {s}"
    );
}

#[test]
fn test_pending_dir_uses_override_not_home() {
    let dir = pending_dir().expect("pending_dir() must succeed under override");
    let s = dir.to_string_lossy();
    assert!(
        !s.contains(".modelblockbuilder"),
        "pending_dir() must NOT use ~/.modelblockbuilder during tests, got: {s}"
    );
    assert!(
        s.contains("test-crash-data"),
        "pending_dir() should resolve under target/test-crash-data, got: {s}"
    );
    assert!(
        s.ends_with("crashes/pending"),
        "pending_dir() should end with crashes/pending, got: {s}"
    );
}
