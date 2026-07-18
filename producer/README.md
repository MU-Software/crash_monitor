# Crash Monitor producer SDK

This directory is the standalone C11 producer SDK package. Consumers include
`crash_monitor_producer.h`; its only implementation dependencies are the two
versioned wire-contract headers installed from `schema/`.

The SDK is intentionally not linked to `crash-capture-macos` or any Rust
crate. CMake consumers can use the `crash_monitor_producer` interface target,
and non-CMake builds may install the same three headers directly. The contract
test in `tests/e2e/fixtures/producer_sdk_contract.c` compiles against this
package without the monitor binary.
