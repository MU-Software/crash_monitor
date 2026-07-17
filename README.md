# crash_monitor

Out-of-process crash monitor for native macOS applications. It launches a target
program as a child, watches it over a Mach exception port and an explicitly
enabled shared-memory heartbeat, and on a crash, hang, or manual snapshot
captures bounded per-thread diagnostic state into a JSON report — then provides
offline CLI tools to inspect it. Running out-of-process means the reporter still
works when the target's own address space is corrupt.

## How it works

```
crash_monitor (parent)  ──fork+exec──▶  target app (child)
  Mach exception port                     maps POSIX shared memory
  SIGUSR1 snapshot listener               writes breadcrumbs / context / heartbeat
  ANR watchdog (ready + heartbeat poll)   (via the shared schema — see docs)
  plugin pipeline → JSON report
```

On a crash, hang, or snapshot the monitor suspends the child, walks every thread
(registers, stack, backtrace), reads loaded images, merges shared-memory
breadcrumbs and context, and writes a report. Memory maps, environment data,
screenshots, and attachments are sensitive opt-ins controlled by the privacy
policy.

## Build

```bash
make build      # release build + codesign with the debugger entitlement
```

Codesigning with `crash_monitor.entitlements` grants
`com.apple.security.cs.debugger`, required for `task_for_pid()` on the child
without sudo. Override the identity with `make build SIGN_IDENTITY="…"`.

## Run

```bash
./target/release/crash_monitor run <path-to-app> [args…]
```

Each event is assigned a 32-character `ReportId`. While its artifacts are being
built, they stay hidden in
`~/.crash_monitor/crashes/pending/.report-<ReportId>.pending/`. A report becomes
visible only after `manifest.json` is written last and the whole directory is
atomically published as either `pending/<ReportId>/` or `sent/<ReportId>/`.
Prepared reports interrupted just before publication are recovered on restart;
incomplete staging directories remain hidden. Override the base directory with
`CRASH_MONITOR_DATA_DIR`. Report types include `crash`, `snapshot`, `anr`,
`oom`, `exit_failure`, and `signal_failure`.

## Inspect reports

```bash
crash_monitor analyze <report-dir>/report.zip                    # human-readable summary
crash_monitor stack <report-dir>/report.zip --thread <N>         # stack memory hex dump
crash_monitor symbolicate <report-dir>/report.zip --dsym <path>  # DWARF source resolution
```

Pass the artifact named by the directory's `manifest.json`, not the report
directory or manifest itself. The canonical entry has kind `archive` and path
`report.zip`, or kind `report` and path `report.json` when no archive was made.

## Test

```bash
make test        # unit + integration + e2e   (e2e requires codesigning)
make coverage    # HTML coverage report
```

## Documentation

- [docs/architecture.md](docs/architecture.md) — process model, event sources, capture lifecycle
- [docs/shared-memory.md](docs/shared-memory.md) — the C↔Rust shared-memory layout contract
- [docs/pipeline.md](docs/pipeline.md) — plugin pipeline and stability invariants
- [docs/reports.md](docs/reports.md) — report JSON format and CLI analysis tools
- [docs/integration.md](docs/integration.md) — integrating a host application as the child
- [docs/privacy.md](docs/privacy.md) — consent gates, retention, and encryption limitations
