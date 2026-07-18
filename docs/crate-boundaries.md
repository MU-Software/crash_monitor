# Crate boundaries

The workspace separates execution responsibilities:

- `crash-report-core` loads bounded JSON/ZIP reports and provides
  platform-neutral inspection primitives.
- `crash-monitor-cli` depends only on `crash-report-core` plus portable CLI
  libraries. The `crash-report` binary can be compiled without Mach APIs,
  bindgen, entitlements, or the capture crate.
- `crash-capture-macos` owns process supervision, Mach capture, shared-memory
  consumption, pipeline finalization, and the `crash_monitor` binary.
- `producer/` is a separately installable C11 header package and depends only
  on the versioned wire schema.

The monitor binary imports the capture library API; it does not declare a
second module tree. Mach FFI modules and generated SHM bindings are private.
Only curated safe wrappers and owned SHM values cross the library boundary.
External integration fixtures use the feature-gated, doc-hidden
`crash_monitor::test_support` facade, which is absent from production builds.
