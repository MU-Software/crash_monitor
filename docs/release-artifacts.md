# Release artifact contract

The production archive installs the monitor at `bin/crash_monitor` and its
least-privileged UI helper at
`libexec/crash_monitor/crash_dialog_macos`. The monitor resolves the helper
relative to its own executable; production does not search `PATH` or accept an
environment override. The monitor is signed with
`com.apple.security.cs.debugger`; the dialog is separately signed with empty
entitlements. Both signatures must share the approved release identity.

Releases support native arm64 on the minimum macOS version encoded by the
release build. In-place replacement is not supported while a monitor is
running: stop monitors, verify the archive checksum, atomically replace the
installation directory, then restart. `SHA256SUMS` covers every shipped
binary and dSYM file. Matching dSYMs are retained with the release indefinitely
and selected by image UUID and architecture during symbolication.

`make package` creates `target/package/crash-monitor.tar.gz`;
`make verify-package` checks the exact binary allowlist, checksums, dSYM
presence, signatures, debugger entitlement, and arm64 architecture.
