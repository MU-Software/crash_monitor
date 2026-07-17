# crash_monitor

Out-of-process crash monitor for native macOS applications. It launches a target
program as a child, watches it over a Mach exception port and an explicitly
enabled shared-memory heartbeat, and on a crash, hang, or manual snapshot
captures full per-thread diagnostic state into a JSON report — then provides
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
(registers, stack, backtrace), reads loaded images and the memory map, merges the
shared-memory breadcrumbs and context, and writes a report.

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

Each event receives a UUID `ReportId` and is atomically committed as one
directory, normally
`~/.crash_monitor/crashes/sent/<report-id>/{manifest.json,report.json|report.zip}`.
Until commit, its files remain hidden under `pending/.report-<report-id>.pending`.
Override the base directory with `CRASH_MONITOR_DATA_DIR`. Report types:
`crash`, `snapshot`, `anr`, `oom`, `exit_failure`, `signal_failure`.

## Inspect reports

```bash
crash_monitor analyze <report.json>                    # human-readable summary
crash_monitor stack <report.json> --thread <N>         # stack memory hex dump
crash_monitor symbolicate <report.json> --dsym <path>  # DWARF source resolution
```

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
