# Plugin pipeline

Report generation is a pipeline of small plugins grouped into five categories.
A default pipeline is assembled per run; individual plugins can be toggled or
tuned by config (see [Configuration](#configuration)).

## Stages

```
MonitorEvent
   │
   ▼
Filter        should this event produce a report at all?
   │
   ▼
Collector     gather data while the child is SUSPENDED
   │  (child resumed here)
   ▼
PreProcessor  enrich the collected data
   │
   ▼
Report        Stage-1 raw fail-safe dump → Stage-2 JSON
   │
   ▼
PostProcessor file operations on the finished report
   │
   ▼
Notifier      announce the report
```

| Category | Runs while child… | Typical plugins |
|----------|-------------------|-----------------|
| **Filter** | before capture | disk-space guard, rate limiter, duplicate/dedup |
| **Collector** | suspended | threads, memory + heap, dylibs, breadcrumbs, context, screenshots, attachments, environment |
| **PreProcessor** | resumed | session, symbolication, fingerprint, build info, sanitizer |
| **PostProcessor** | resumed | raw cleanup, session recorder, PNG conversion, feedback dialog, ZIP archive, move-to-sent, log rotation, retention |
| **Notifier** | resumed | console, system notification |

(The exact roster is what the default pipeline registers; treat the source as
the authority, not this list.)

## Safety wrapper

Every plugin runs inside `run_plugin_safe()`, which:

- wraps the call in `catch_unwind` — a panic in one plugin is contained and the
  pipeline continues, and
- arms an `alarm()` timeout so a hung plugin is interrupted rather than wedging
  the whole report.

Per-category default timeouts (a plugin may override its own):

| Category | Default |
|----------|---------|
| Filter | 1 s |
| Collector | 5 s |
| PreProcessor | 2 s |
| Report stage | 5 s |
| PostProcessor | 30 s |
| Notifier | 5 s |

## Stability invariants

The monitor is deliberately over-defensive — **losing crash data is the worst
outcome**, worse than a degraded report. The design rules:

1. **No `panic = "abort"`** in any profile — `catch_unwind` depends on unwinding.
2. **No `unwrap`/`expect` on the capture path** — use `?`, `.get()`, `unwrap_or`.
3. **Per-plugin timeouts** — `alarm()` interrupts a hung plugin.
4. **Partial success is preserved** — one collector failing never discards what
   the others already gathered.
5. **Stage-1 fail-safe** — a raw binary dump is written before JSON
   serialization, so a report exists even if formatting later fails.
6. **Input size caps** — bounded report / dSYM / decoded-stack sizes guard
   against pathological input.
7. **No cross-category plugin dependencies** — a plugin's `depends_on` may name
   only same-category plugins, keeping ordering acyclic and local.

## Configuration

All plugins are enabled by default (opt-out design). An optional
`crash_reporter.json` in the data directory disables specific plugins or adjusts
parameters (rate-limit window, retention limits, fingerprint frame count, …). A
missing or malformed file falls back to defaults silently — configuration can
never prevent a report from being written.
