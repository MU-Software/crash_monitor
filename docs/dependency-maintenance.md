# Dependency maintenance

Dependabot proposes Cargo updates monthly and the RustSec audit runs weekly and
for every manifest or lockfile pull request. A maintainer triages advisories by
affected code path, exploitability, patched versions, and available mitigation.
An old major version is not by itself a defect and is not upgraded solely to
make version numbers current.

Every dependency update must record:

- advisory identifiers and whether this project is affected;
- upstream changelog and release-note review;
- compatibility with the Rust 1.93 MSRV and supported macOS/arm64 target;
- API and behavior changes, including default-feature changes;
- migration cost and rollback plan;
- `make lint`, workspace test, privileged E2E, and package verification results.

Major upgrades are isolated from unrelated updates. Patch updates may be
grouped only when their changelogs show no interacting behavior change. Lockfile
changes are reviewed for newly introduced packages, build scripts, licenses,
and platform-specific transitive dependencies.
