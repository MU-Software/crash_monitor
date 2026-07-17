# Reports

## Types

| Type | Trigger |
|------|---------|
| `crash` | fatal Mach exception (SIGSEGV / SIGBUS / SIGABRT / SIGFPE) |
| `snapshot` | manual, on `SIGUSR1` — the app keeps running |
| `anr` | the ANR watchdog saw the heartbeat stall past its threshold |
| `oom` | a primary SIGKILL child termination classified as probable OOM |
| `exit_failure` | the child exited with a non-zero status |
| `signal_failure` | the child terminated from a signal not already reported as a Mach crash |

## Location and lifecycle

Finished reports are written under the data directory:

```
<data-dir>/crashes/pending/<type>_<timestamp>_<pid>.json   # freshly written
<data-dir>/crashes/sent/                                    # after post-processing
```

The default data directory is `~/.crash_monitor`; see
[integration.md](integration.md) for overriding it. Post-processors may archive
finished reports to `sent/` and prune it by count / size / age.

## JSON shape

A report is a single JSON object. The exact fields depend on which collectors
ran, but the top-level shape is:

```jsonc
{
  "header":        { "version", "timestamp", "pid", "process", "type", "collector" },
  "termination":   { "kind": "exited", "exit_code", "runtime_ms" },
  "exception":     { "type", "code", "subcode", "signal", "fault_address" },
  "crash_context": { "annotations": { "<key>": "<value>", … } },   // app state, generic KV
  "threads": [
    {
      "index", "id", "name", "crashed",
      "registers":    { "<reg>": "0x…", … },
      "backtrace":    [ { "address", "image", "offset", "symbol", "file"?, "line"?, "column"? } ],
      "stack_memory": { "sp", "size", "hex_dump" }
    }
  ],
  "loaded_images": [ { "path", "base", "slide" } ],
  "memory_map":    [ { "address", "size", "prot", "info" } ],
  "heap_summary":  { "zones": [ { "name", "in_use_bytes", "in_use_count" } ] },
  "session":       { "id", "start", "duration_s" },
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

## Monitor process exit status

The monitor keeps outcome categories separate internally and exposes these
stable CLI statuses:

| Status | Meaning |
|--------|---------|
| `0` | child completed normally |
| `70` | monitor-internal failure (including synchronous spawn/exec setup failure) |
| `80` | child exited non-zero; the original code is in `termination.exit_code` |
| `81` | a fatal Mach exception was detected and reported |
| `128 + signal` | child terminated from an otherwise-unreported signal |

A Unix process status cannot carry every child exit code and separate monitor
namespaces at once. The JSON report is therefore authoritative for the original
exit code, signal, core-dump flag, and runtime; the monitor status communicates
the outcome category.

Application/domain state lives under `crash_context.annotations` as string
key-value pairs (the monitor does not interpret them — see
[shared-memory.md](shared-memory.md#crash-context-app-agnostic)).

## CLI analysis tools

The same binary reads reports offline. Reports may be plain `.json` or inside a
`.zip` archive; both are accepted.

### `analyze` — human-readable summary

```bash
crash_monitor analyze <report.json>
```

Prints the header, the annotation map, session, fingerprint, exception details,
the crashed thread's backtrace, recent breadcrumbs, and a pipeline
success/error/skipped tally. Exit- and signal-failure summaries also print the
exit code or signal, core-dump status when set, and runtime.

### `stack` — stack memory hex dump

```bash
crash_monitor stack <report.json> --thread <N>
```

Classic hexdump (16 bytes/line + ASCII sidebar) of a thread's captured stack.
Decoded output is size-capped; larger captures are truncated with a notice.

### `symbolicate` — DWARF source resolution

```bash
crash_monitor symbolicate <report.json> --dsym <path> [--output <out.json>]
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
