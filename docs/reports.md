# Reports

## Types

| Type | Trigger |
|------|---------|
| `crash` | fatal Mach exception (SIGSEGV / SIGBUS / SIGABRT / SIGFPE) |
| `snapshot` | manual, on `SIGUSR1` — the app keeps running |
| `anr` | the ANR watchdog saw the heartbeat stall past its threshold |
| `oom` | the child was killed under memory pressure (opt-in trigger) |

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
success/error/skipped tally.

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
