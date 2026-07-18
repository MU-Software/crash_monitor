# Reports

## Types

| Type | Trigger |
|------|---------|
| `crash` | fatal Mach exception (SIGSEGV / SIGBUS / SIGABRT / SIGFPE) |
| `snapshot` | manual, on `SIGUSR1` — the app keeps running |
| `anr` | the ANR watchdog saw an opted-in producer's heartbeat stall past its threshold |
| `oom` | an opt-in `possible_oom` classification based only on primary SIGKILL |
| `exit_failure` | the child exited with a non-zero status |
| `signal_failure` | the child terminated from a signal not already reported as a Mach crash |

## Location and lifecycle

Every trigger event receives one globally unique, 32-character `ReportId`.
That identity is preserved across every pipeline stage and names the final
report directory:

```
<data-dir>/crashes/pending/.report-<ReportId>.pending/  # hidden staging only
<data-dir>/crashes/pending/<ReportId>/manifest.json     # committed in pending
<data-dir>/crashes/sent/<ReportId>/manifest.json        # committed after MoveToSent
```

Artifacts are written and synced inside the hidden staging directory. The
monitor writes and syncs `manifest.json` last, then publishes the entire report
with one atomic directory rename. Readers must ignore hidden staging directories
and treat only a final `<ReportId>/` directory with a valid manifest and its
exact artifact set as committed. This prevents a partially written JSON, ZIP,
attachment, or screenshot from becoming visible as a report.

If the monitor stops after syncing the manifest but before the rename, startup
recovery validates the prepared manifest and publishes the directory to its
recorded destination. A staging directory without a valid manifest remains
hidden and is never presented as a finished report.

The default data directory is `~/.crash_monitor`; see
[integration.md](integration.md) for overriding it. Post-processors may archive
reports, move them to `sent/`, and prune `sent/` by count / size / age.

### Manifest

`manifest.json` is the commit record and the authoritative artifact registry.
A representative archived report looks like:

```json
{
  "schema_version": 1,
  "report_id": "55d79fbc138d48b39a640b2aef61cbbb",
  "report_type": "crash",
  "pid": 4102,
  "process": "example-app",
  "committed_at": "2026-07-18T12:34:56.123456789+09:00",
  "destination": { "kind": "sibling", "directory": "sent" },
  "final_diagnostics": {
    "pipeline_duration_ms": 37,
    "plugins": { "DesktopNotification": { "status": "ok", "duration_ms": 2 } }
  },
  "artifacts": [
    { "path": "report.zip", "kind": "archive", "size": 48123 }
  ]
}
```

`report_id` must match the containing directory, and `report_type` uses
snake_case. Artifact paths are safe, report-local names; duplicate paths,
path traversal, non-regular files, size mismatches, and files not listed in the
manifest invalidate the directory for readers and recovery. The canonical
report artifact is `{"path":"report.json","kind":"report"}` or, after ZIP
archiving, `{"path":"report.zip","kind":"archive"}`. Other exact entries may
describe attachments, screenshots, or fail-safe raw data.

`final_diagnostics` is optional for backward compatibility. The monitor writes
it by atomically replacing `manifest.json` only after after-commit processors,
notifiers, after-notify processors, and final cleanup have completed. It does
not add an artifact or change any artifact size recorded by the manifest.
Report loaders prefer this terminal snapshot over the earlier `_diagnostics`
embedded in `report.json` or `report.zip`.

## JSON shape

The `report.json` artifact (also stored inside `report.zip`) is a single JSON
object. `header.version` is currently `1`. Every in-process consumer and CLI
checks this field before decoding; version `0` and future versions are rejected
with a structured compatibility error rather than guessed. Additive optional
fields remain compatible within version 1, while a breaking field/layout change
requires a new version and an explicit migration fixture. The exact fields
depend on which collectors ran, but the top-level shape is:

```jsonc
{
  "header":        { "version": 1, "timestamp", "pid", "process", "type", "collector" },
  "termination":   { "kind": "exited", "exit_code", "runtime_ms" },
  "exception":     { "type", "code", "subcode", "raw_codes": ["0x…", …], "signal", "fault_address" },
  "crash_context": { "annotations": { "<key>": "<value>", … } },   // app state, generic KV
  "threads": [
    {
      "index", "id", "name", "crashed",
      "registers":    { "<reg>": "0x…", … },
      "backtrace":    [ { "address", "image", "offset", "symbol", "file"?, "line"?, "column"? } ],
      "stack_memory": { "sp", "size", "hex_dump" } | null // explicit privacy opt-in
    }
  ],
  "loaded_images": [ { "path", "base", "slide", "uuid", "architecture", "text_start", "text_end" } ],
  "memory_map":    [ { "address", "size", "prot", "info" } ],
  "heap_summary":  {
    "task_vm": { "virtual_size_bytes", "resident_size_bytes", "physical_footprint_bytes", "internal_bytes", "compressed_bytes" },
    "zones": [ { "name", "resident_bytes_estimate", "region_count", "virtual_size_bytes" } ]
  },
  "session":       { "id", "start", "duration_s" },
  "crash_context": {
    "source": "producer_shared_memory",
    "annotations": { "key": "value" },
    "session_id": "…", "session_start_ns": 123, "heartbeat_counter": 55
  },
  "settings_snapshot": {
    "source": "producer_shared_memory", "world_bounds": [0, 0, 0, 1, 1, 1],
    "palette_count": 8, "history_max": 64, "extra": "…"
  },
  "environment": {
    "kernel_release": "24.3.0", "kernel_version": "Darwin Kernel Version …",
    "arch": "arm64", "hostname": "…",
    "variables_source": "spawn_environment_snapshot", "env_vars": { "LANG": "…" }
  },
  "breadcrumbs":   [ { "timestamp", "category", "message", … } ],
  "fingerprint":   "…",          // hash of the top app frames, for grouping
  "attachments":   [ … ],
  "_diagnostics":  { "<plugin>": { "status", "duration_ms" }, … }
}
```

`termination` is present on reports that describe a terminal child state,
including fatal Mach crashes, and omitted from non-terminal snapshots/ANR
reports. It is an internally tagged object with one of these shapes:

```jsonc
{ "kind": "exited",   "exit_code": 17, "runtime_ms": 250 }
{ "kind": "signaled", "signal": 11, "core_dumped": true, "runtime_ms": 999 }
```

`runtime_ms` is wall-clock time from the start of the spawn operation until the
terminal wait status is observed; it can therefore include supervisor
observation delay. `core_dumped` preserves the corresponding wait-status flag
rather than inferring it from the signal number. For a Mach crash, the monitor
first captures an immutable snapshot, then resumes and replies. After reaping
the child, the supervisor hands the actual wait status to the fatal finalizer;
the original JSON and ZIP are therefore written with `termination` already
present rather than patched afterward.

For subscribed Mach crashes, `exception.raw_codes` preserves the complete MIG
`mach_exception_data_t` array as hexadecimal bit patterns and therefore also
preserves its original element count. `type_code`, `code_value`, and `subcode`
retain numeric values independently from the optional `code_name` display
label. Only `EXC_BAD_ACCESS` treats the second code as `fault_address`, and its
kernel return distinguishes `SIGSEGV` from `SIGBUS`. Other exception classes do
not pretend that every code is a `kern_return_t` or every subcode is an address.
The current `EXC_CRASH` signal mapping is explicitly marked approximate because
the monitor does not yet authoritatively decode every kernel encoding.

Thread `id` is the system-wide `THREAD_IDENTIFIER_INFO.thread_id`, not the
monitor-local Mach port name. `unwind_method`, `unwind_truncated`, and
`unwind_note` expose the current frame-pointer-only quality. arm64e PAC bits are
removed from return-address lookup values; raw stack bytes remain unchanged.

Malloc-tag zone metrics are VM approximations: `resident_bytes_estimate` is
resident pages times host page size, and `region_count` counts VM regions, not
allocator objects. Legacy `in_use_bytes`/`in_use_count` inputs remain accepted
as aliases. The task VM summary exposes footprint, internal, and compressed
bytes without relabeling them as allocator usage.

Loaded-image identity includes Mach-O `LC_UUID`, CPU architecture, ASLR slide,
and the actual runtime `__TEXT` half-open range. Address attribution and
symbol-selection require that range and never use a fixed 256MB window or the
nearest lower image base.

Offline dSYM symbolication selects bounded thin/FAT/FAT64 slices by report UUID
and architecture, applies a loader only to frames inside that image's recorded
`__TEXT` range, and replaces file/line/column as one location unit so stale
components cannot survive a later resolution.
`exception.severity` is `fatal`. `EXC_BREAKPOINT` is subscribed as a fatal
`crash` mapped to `SIGTRAP`, and `EXC_GUARD` as a fatal `crash` mapped to
`SIGKILL`; both retain every raw code. `EXC_RESOURCE` is deliberately not
subscribed because it can be advisory/non-fatal, so it has no report severity
and no raw-code artifact.

## Monitor process exit status

The monitor keeps outcome categories separate internally and exposes these
stable CLI statuses:

| Status | Meaning |
|--------|---------|
| `0` | child completed normally |
| `70` | monitor-internal failure (including synchronous spawn/exec setup failure) |
| `80` | child exited non-zero; the original code is in `termination.exit_code` |
| `81` | a fatal Mach exception was detected but no signal termination was available |
| `128 + signal` | child terminated by a signal, including after a captured Mach exception |

A captured Mach exception keeps its typed outcome and report while the CLI exit
status follows the actual reaped signal (`128 + signal`). A subsequent non-zero
child exit uses status `80`; a recovered/zero exit keeps status `81` so a
captured fatal incident is never presented as success. The JSON report remains
authoritative for the exact exit code, signal, core-dump flag, and runtime.

Application/domain state lives under `crash_context.annotations` as string
key-value pairs (the monitor does not interpret them — see
[shared-memory.md](shared-memory.md#crash-context-app-agnostic)).
The entire `crash_context` and `settings_snapshot` objects are typed values
copied from the producer's stable shared-memory publications, identified by
`source = "producer_shared_memory"`. An empty producer `session_id`, a zero
`session_start_ns`, and an empty `settings.extra` are represented by omitted
optional fields. `heartbeat_counter` is retained even when zero because zero is
a valid observed counter value; producer readiness is a separate header flag.
The top-level `session` object is independently generated by the monitor's
session recorder and must not be confused with producer session metadata.

`environment.env_vars` is the filtered, immutable byte snapshot passed to
`posix_spawn`, never the monitor's current environment. It includes variables
added by the supervisor before spawn, but cannot include child-only additions
or changes made by the child after it starts. If a library caller does not
provide a spawn snapshot, `variables_source` is `"unavailable"` and `env_vars`
is empty. `kernel_release` and `kernel_version` are `uname` values; neither is
presented as the macOS product version.

Thread capture retains at most 512 threads, 64 KiB per stack, and 8 MiB of
stack bytes per event. The crashed thread consumes that budget first. Thread
and screenshot collectors also share the capture pipeline's absolute deadline.
Screenshot selection retains at most 8 frames/4 MiB, prioritizing lower numeric
producer `tier` values, then newer timestamps. Budget truncation is reported in the
corresponding collector diagnostic rather than silently expanding the report.

## CLI analysis tools

The same binary reads report artifacts offline. It accepts either the plain
`report.json` artifact or `report.zip`. Pass the artifact path recorded by
`manifest.json`; do not pass the report directory or `manifest.json` itself.

### `analyze` — human-readable summary

```bash
crash_monitor analyze <report-directory>/report.zip
```

Prints the header, the annotation map, session, fingerprint, exception details,
the crashed thread's backtrace, recent breadcrumbs, and a pipeline
success/error/skipped tally. Exit- and signal-failure summaries also print the
exit code or signal, core-dump status when set, and runtime.

### `stack` — stack memory hex dump

```bash
crash_monitor stack <report-directory>/report.zip --thread <N>
```

Classic hexdump (16 bytes/line + ASCII sidebar) of a thread's captured stack.
Decoded output is size-capped; larger captures are truncated with a notice.

### `symbolicate` — DWARF source resolution

```bash
crash_monitor symbolicate <report-directory>/report.zip --dsym <path> [--output <out.json>]
```

Resolves backtrace addresses to `file:line:column` using a dSYM bundle's DWARF
info. Notes:

- Release (stripped) builds need a dSYM; debug builds with symbol tables may not.
- The `--dsym` path may be the `.dSYM` bundle or the inner DWARF binary.
- The ASLR slide is taken from the report's `loaded_images` entries.
- Frames outside the dSYM's range (system dylibs) are skipped; partial success
  is preserved.
- Without `--output` it prints to stdout; with it, the enriched report is written
  to that path — **the input report is never modified in place**.
