# Dependency maintenance

Dependabot proposes Cargo updates monthly and the RustSec audit runs weekly and
for every manifest or lockfile pull request. A maintainer triages advisories by
affected code path, exploitability, patched versions, and available mitigation.
An old major version is not by itself a defect and is not upgraded solely to
make version numbers current.

Every dependency update must record:

- advisory identifiers and whether this project is affected;
- upstream changelog and release-note review;
- compatibility with the Rust 1.97.1 MSRV and supported macOS/arm64 target;
- API and behavior changes, including default-feature changes;
- migration cost and rollback plan;
- `make lint`, workspace test, privileged E2E, and package verification results.

Major upgrades are isolated from unrelated updates. Patch updates may be
grouped only when their changelogs show no interacting behavior change. Lockfile
changes are reviewed for newly introduced packages, build scripts, licenses,
and platform-specific transitive dependencies.

## 2026-07-18 full dependency refresh

- Raised the project MSRV and CI toolchain from Rust 1.93 to Rust 1.97.1. The
  Rust 1.97.1 release fixes an LLVM miscompilation in Rust 1.97.0; local
  validation used the `aarch64-apple-darwin` toolchain.
- Updated all compatible Cargo dependencies and reviewed the release notes for
  the direct major upgrades: `addr2line` 0.27, `bindgen` 0.72, `png` 0.18,
  `sha2` 0.11, and `zip` 8.6. `zip` remains restricted to its `deflate`
  feature with default features disabled.
- Removed `bincode` after `cargo audit` reported RUSTSEC-2025-0141
  (unmaintained). Capture-helper results now use `rmp-serde` MessagePack and
  wire version 2. This wire format is exchanged only between matching helper
  and monitor binaries and is not persisted as a compatibility contract.
- Updated GitHub Actions to `actions/checkout@v7`, pinned Rust jobs to 1.97.1,
  and enabled monthly Dependabot updates for GitHub Actions.
- This intentionally groups the complete refresh at the maintainer's request.
  Roll back by reverting this dependency-refresh commit; no data migration or
  persistent wire artifact needs to be reversed.
- Validation passed with strict workspace Clippy and tests (`make ci-fast`),
  `cargo audit --deny warnings` (162 packages), all 16 signed privileged E2E
  tests (`make e2e`), and signed release archive verification
  (`make verify-package`).
