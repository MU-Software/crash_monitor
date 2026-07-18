# Contributing

Development requires an arm64 Mac, Xcode command-line tools, and the pinned Rust
toolchain described in `docs/development.md`.

Create a focused branch, keep each logical change in its own commit, and run:

```sh
make lint
cargo test --workspace --all-targets
```

Privileged E2E tests additionally require an Apple Development signing identity.
Use `make e2e SIGN_IDENTITY="Apple Development: ..."`; an identity SHA-1 is
also accepted when names are ambiguous. Update user-facing docs, schema
fixtures, and the release checklist whenever their contracts change.
