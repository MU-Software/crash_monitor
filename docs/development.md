# Development setup

The repository is pinned to Rust 1.97.1 by `rust-toolchain.toml`. Rustup
installs the compiler, rustfmt, Clippy, and `llvm-tools-preview` automatically.
Install `cargo-llvm-cov` separately before running the coverage targets.
`cargo-llvm-cov` discovers the rustup LLVM tools; callers may override
`LLVM_COV` and `LLVM_PROFDATA` when using another LLVM installation.

The build script generates Rust SHM bindings with bindgen. On macOS it uses
Xcode's Clang by default. If libclang is installed elsewhere, set
`LIBCLANG_PATH` to the directory containing `libclang.dylib`. CI and local
builds both require an arm64 macOS host and the Xcode command-line tools.
