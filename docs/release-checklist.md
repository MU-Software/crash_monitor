# Release checklist

1. Review dependency advisories, changelogs, MSRV, and lockfile changes.
2. Run `make lint` and `cargo test --workspace --all-targets`.
3. Run privileged `make e2e` with the release signing identity.
4. Build and verify the production package contract and allowed binary list.
5. Archive matching dSYMs and record artifact SHA-256 checksums.
6. Verify signatures, entitlements, minimum macOS version, and arm64 slices.
7. Publish release notes, upgrade notes, and any coordinated security advisory.
