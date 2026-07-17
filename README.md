# crash_monitor — Crash Monitor for Model Block Builder

`crash_monitor` is an out-of-process crash reporter for Model Block Builder (macOS).
It spawns the desktop app as a child process, monitors for crashes via Mach
exception ports, captures full diagnostic state on crash/snapshot, and provides
offline CLI tools for analyzing the resulting reports.

## Architecture

Two-process model:

```
┌────────────────────────────────────────────┐
│  crash_monitor (Rust, parent)                │
│  - Mach exception port + SIGUSR1 handler   │
│  - POSIX shared memory (breadcrumbs/ctx)   │
│  - ANR watchdog (heartbeat polling)        │
│  - Plugin pipeline (collect/process/write) │
└────────────────────────────────────────────┘
                  │
                  ▼ fork+exec
┌────────────────────────────────────────────┐
│  voxelcore_desktop (C, child)              │
│  - sut_crash_reporter (breadcrumbs, ctx)   │
│  - SUT_CRUMB() / SUT_ASSERT() macros       │
│  - F8 → SIGUSR1 → manual snapshot          │
└────────────────────────────────────────────┘
```

When the child crashes (SIGSEGV/SIGBUS/SIGABRT/SIGFPE) or hangs (>5s no
heartbeat), the monitor suspends it, walks every thread (registers, stack,
backtrace), reads loaded dylibs and memory map, and writes a JSON report to
`~/.modelblockbuilder/crashes/pending/`.

For the full design, see [`docs/plans/crash_reporter.md`](../../docs/plans/crash_reporter.md).

## Building

From the project root:

```bash
make crash-monitor          # builds and codesigns the release binary
```

The signed binary is placed at:

- `tools/crash_monitor/target/release/crash_monitor`
- `apps/desktop/build-debug/crash_monitor` (also copied here)

The codesign step uses [`crash_monitor.entitlements`](crash_monitor.entitlements)
to grant `com.apple.security.cs.debugger` so the monitor can attach to its
child via `task_for_pid()` without sudo.

## Running the Monitor

```bash
make desktop-monitor-run    # build + run app under monitor
```

This launches the desktop app as a child of `crash_monitor`. While running:

- **Crashes** are caught automatically via Mach exception port
- **F8** triggers a manual snapshot (app keeps running)
- **Hangs >5s** trigger an ANR snapshot (after 10s warmup)

Reports land in `~/.modelblockbuilder/crashes/pending/<type>_<timestamp>_<pid>.json`.

## CLI Analysis Tools

Phase 5 added three offline subcommands for inspecting reports.

### `make crash-list`

List all crash reports in the pending directory, newest first.

```bash
make crash-list
```

### `make crash-analyze` — Human-readable summary

Display a high-level summary of a crash/snapshot/ANR report.

```bash
make crash-analyze                                     # latest report
make crash-analyze REPORT=/path/to/report.json         # specific report
```

Output includes:

- **Header**: report type, signal, fault address, PID, process name, timestamp
- **Context**: active tool, frame number, region count (from `crash_context`)
- **Session**: ID and duration
- **Fingerprint**: 16-char hash for crash grouping (Phase 5)
- **Exception details**: type, code, subcode, signal, fault address
- **Crashed thread backtrace**: top 20 frames with symbols
- **Recent breadcrumbs**: last 10 SUT_CRUMB events
- **Diagnostics**: pipeline plugin success/error/skipped counts

Example:

```
Crash Report: SIGSEGV at 0x600000000  (PID 58260, voxelcore_desktop)
Time: 2026-04-06T00:20:33.305237000+09:00
Context: tool=face_pull_drag | frame=2847 | regions=42
Session: 2h 34m (id: abc-123)
Fingerprint: a1b2c3d4e5f67890

Exception:
  Type:          EXC_BAD_ACCESS
  Code:          KERN_INVALID_ADDRESS
  Subcode:       0x600000000
  Signal:        SIGSEGV
  Fault address: 0x600000000

Thread 0 (main) [CRASHED]:
  #0   vxc_face_drag_step + 0x1a8  [voxelcore_desktop]
  #1   abi_dispatch_action + 0x94  [voxelcore_desktop]
  #2   main + 0x544                [voxelcore_desktop]

Recent breadcrumbs:
  [12:00:00.001] crumb    face_drag step dx=3
  [12:00:00.002] crumb    set_voxel (1,2,3)=5

Pipeline: 7 ok, 0 error, 0 skipped  (12ms total)
```

### `make crash-stack` — Stack memory hex dump

Dump the captured stack memory for a thread as classic hexdump (16 bytes
per line, ASCII sidebar).

```bash
make crash-stack                                       # latest report, thread 0
make crash-stack THREAD=2                              # specific thread
make crash-stack REPORT=/path/to/report.json THREAD=0  # specific report+thread
```

Example:

```
Stack memory for thread 0 (SP: 0x16d4fe000, 8192 bytes)

00000000  48 65 6c 6c 6f 20 57 6f  72 6c 64 21 00 00 00 00  |Hello World!....|
00000010  ff ff ff ff 00 00 00 00  01 00 00 00 00 00 00 00  |................|
...
```

Safety: Decoded data is capped at 16 MB. Larger captures are truncated with a
notice.

### `make crash-symbolicate` — DWARF source resolution

Resolve backtrace addresses to source `file:line:column` using a dSYM bundle's
DWARF debug info (via `addr2line` + `gimli`).

```bash
make crash-symbolicate                                                # latest report, default dSYM
make crash-symbolicate REPORT=/path/report.json DSYM=/path/foo.dSYM   # explicit
make crash-symbolicate OUTPUT=enriched.json                           # write to file instead of stdout
```

Default `DSYM` path is `apps/desktop/build-debug/voxelcore_desktop.dSYM`.

By default, prints a symbolicated backtrace to stdout. With `OUTPUT=`, writes
the enriched report (with `file`/`line`/`column` fields populated in each
`BacktraceFrame`) to the specified path. The original report is **never
modified in place**.

Example output:

```
Thread 0 (main) [CRASHED]:
  #0   0x0000000100a3b1a8  vxc_face_drag_step + 0x1a8  vxc_tool_face_pull_drag.c:234:5
  #1   0x0000000100a2f094  abi_dispatch_action + 0x94   abi_actions.c:156
```

Notes:

- Works with debug builds that retain symbol tables. Release `strip` builds
  require a dSYM bundle (build with `dsymutil` or Xcode Archive).
- The dSYM path can be either the `.dSYM` directory or the inner DWARF binary
  at `<dsym>/Contents/Resources/DWARF/<name>` directly.
- ASLR slide is computed automatically from `loaded_images[].base` and
  `loaded_images[].slide` in the report.
- Frames that fall outside the dSYM's range (e.g., system dylibs) are skipped
  silently — partial success is preserved.

### Direct CLI Invocation

Without using make targets:

```bash
./tools/crash_monitor/target/release/crash_monitor analyze <report.json>
./tools/crash_monitor/target/release/crash_monitor stack <report.json> --thread <N>
./tools/crash_monitor/target/release/crash_monitor symbolicate <report.json> --dsym <path> [--output <out.json>]
./tools/crash_monitor/target/release/crash_monitor --help
```

## Report Format

Reports are JSON with this top-level shape:

```jsonc
{
  "header":            { "version", "timestamp", "pid", "process", "type", ... },
  "exception":         { "type", "code", "subcode", "signal", "fault_address" },
  "crash_context":     { "active_tool", "frame_number", "region_count", ... },
  "threads": [
    {
      "index", "id", "name", "crashed",
      "registers":   { "x0": "0x...", ... },
      "backtrace":   [ { "address", "image", "offset", "symbol", "file"?, "line"?, "column"? } ],
      "stack_memory": { "sp", "size", "hex_dump" }   // base64
    }
  ],
  "loaded_images":     [ { "path", "base", "slide" } ],
  "memory_map":        [ { "address", "size", "prot", "info" } ],
  "heap_summary":      { "zones": [ { "name", "in_use_bytes", "in_use_count" } ] },
  "session":           { "id", "start", "duration_s" },
  "breadcrumbs":       [ { "timestamp", "category", "message", ... } ],
  "fingerprint":       "a1b2c3d4e5f67890",          // Phase 5 — top-8 app frame hash
  "attachments":       [ ... ],                      // file/screenshot metadata
  "_diagnostics":      { "<plugin_name>": { "status", "duration_ms" }, ... }
}
```

Report types:

- `crash` — Mach exception (SIGSEGV, SIGBUS, SIGABRT, SIGFPE)
- `snapshot` — F8 manual snapshot (app keeps running)
- `anr` — Watchdog detected hang (>5s without heartbeat after 10s warmup)

## Plugin Pipeline

The monitor uses a 6-category plugin architecture (`pipeline/mod.rs`):

```
Trigger → Filter → Collect → Pre-process → Generate Report → Post-process → Notify
```

| Category | Plugins | Purpose |
|----------|---------|---------|
| **Collector** | ThreadCollector, MemoryCollector, DylibCollector, BreadcrumbCollector, ContextCollector, ScreenshotCollector, AttachmentCollector | Gather data while child is suspended |
| **PreProcessor** | SessionEnricher, SymbolResolver, **Fingerprinter** | Enrich data after resume |
| **PostProcessor** | RawCleanup, SessionRecorder, ConsoleNotifier | File operations + notifications |

Every plugin runs inside `run_plugin_safe()` which wraps it in `catch_unwind`
+ a per-category alarm timeout (Filter 1s, Collector 5s, PreProcessor 2s,
PostProcessor 30s). A panic in one plugin never aborts the pipeline.

## Stability Principles

The monitor is **pathologically defensive** — losing crash data is the worst
possible failure. Key invariants:

1. **No `panic = "abort"`** — required for `catch_unwind` to work
2. **No `unwrap()`/`expect()` in hot paths** — use `?`, `.get()`, `unwrap_or`
3. **Per-plugin timeouts** — `alarm()` interrupts hung plugins
4. **Partial success preserved** — collector failures don't roll back
5. **Stage 1 fail-safe** — raw binary dump written before JSON serialization
6. **Input size caps** — reports 256 MB, dSYM 1 GB, decoded stack 16 MB
7. **No cross-category plugin dependencies** — `depends_on` is same-category only

See [`memory/crash_monitor_architecture.md`](../../memory/crash_monitor_architecture.md)
for the full architecture rules.

## Testing

```bash
make crash-monitor-test                # all tests (unit + integration + e2e)
make crash-monitor-unit-test           # 136 unit tests
make crash-monitor-integration-test    # 4 integration tests + cli_analyze + cli_stack
make crash-monitor-e2e-test            # 5 end-to-end tests with real child process
make crash-monitor-coverage            # combined HTML coverage report
```

E2E tests run in parallel — each test creates its own temporary directory via
`CRASH_MONITOR_DATA_DIR` so there is no shared state between tests. Both release
(codesigned) and debug (unsigned) builds are required. `make crash-monitor-test`
handles this automatically. The `test_e2e_unsigned_binary_fails_fast` test uses
the debug build to verify the entitlement self-check; it is skipped if
`target/debug/crash_monitor` does not exist.

Coverage report: `tools/crash_monitor/coverage-report/html/index.html`.

## Module Layout

```
src/
├── main.rs                  ← entry point + CLI routing
├── lib.rs                   ← public modules for integration tests
├── event_loop.rs            ← MonitorEvent dispatch (testable)
├── watchdog.rs              ← ANR state machine
│
├── cli/                     ← Phase 5: offline analysis tools
│   ├── analyze.rs           ← report summary printer
│   ├── stack.rs             ← hex dump
│   └── symbolicate.rs       ← DWARF source resolution
│
├── collectors/              ← gather data during suspension
│   ├── thread.rs            ← registers + backtrace + stack
│   ├── dylib.rs             ← loaded images + ASLR slide
│   ├── memory.rs            ← VM regions + heap summary
│   ├── breadcrumb.rs        ← shm ring buffer
│   ├── context.rs           ← shm crash context
│   ├── screenshot.rs        ← shm 96-slot screenshot ring
│   └── attachment.rs        ← user-attached files
│
├── preprocessors/           ← enrich after resume
│   ├── session.rs           ← read session.lock
│   ├── symbolicate.rs       ← LC_SYMTAB → function names
│   ├── fingerprint.rs       ← Phase 5: SHA-256 of top-8 app frames
│   └── report_formatter.rs  ← raw → formatted (fixed conversion stage)
│
├── postprocessors/          ← file ops + notifications
│   ├── raw_cleanup.rs       ← delete Stage 1 raw.bin
│   ├── session_recorder.rs  ← session_crash.lock
│   └── console.rs           ← stderr notification
│
├── pipeline/                ← plugin infrastructure
│   ├── mod.rs               ← Pipeline + default_macos_pipeline
│   ├── traits.rs            ← 6 plugin traits
│   ├── types.rs             ← CrashEvent, CollectedData, Diagnostics
│   ├── report.rs            ← CrashReport serde + load_report()
│   └── safety.rs            ← run_plugin_safe + AlarmGuard
│
├── platform/                ← OS abstraction
│   ├── macos/               ← MacOsPlatform (Mach + POSIX)
│   │   ├── exceptions.rs    ← Mach exception parsing
│   │   ├── thread.rs        ← thread name extraction
│   │   ├── memory.rs        ← VM region enumeration
│   │   └── ffi/             ← unsafe Mach + libc bindings
│   └── mock/                ← test mock for unit tests
│
├── shm/                     ← POSIX shared memory mirror of C structs
│   ├── reader.rs            ← ring buffer parsing
│   └── types.rs             ← repr(C) + const layout assertions
│
└── utils/
    ├── paths.rs             ← ~/.modelblockbuilder/ paths
    └── vm_tags.rs           ← VM memory tag → human label
```

## Related Documentation

- Design doc: [`docs/plans/crash_reporter.md`](../../docs/plans/crash_reporter.md) (7-phase plan)
- Architecture principles: [`memory/crash_monitor_architecture.md`](../../memory/crash_monitor_architecture.md)
- Test infrastructure: [`memory/crash_monitor_test_plan.md`](../../memory/crash_monitor_test_plan.md)
- C-side reporter: [`cores/shared/utils/include/sut_crash_reporter.h`](../../cores/shared/utils/include/sut_crash_reporter.h)
