# Integrating a host application

A host application is monitored simply by being launched as crash_monitor's
child:

```bash
crash_monitor run <path-to-app> [args…]
```

No source changes are required to get thread/memory/image capture. To also get
breadcrumbs, context, screenshots, and ANR detection, the app links a small
in-process reporter that writes to the shared-memory region. This page covers
both the runtime contract and the configuration knobs.

## Codesigning

The monitor calls `task_for_pid()` on the child, which requires the
`com.apple.security.cs.debugger` entitlement. `make build` codesigns the binary
with [`crash_monitor.entitlements`](../crash_monitor.entitlements). Override the
signer with `make build SIGN_IDENTITY="Developer ID Application: …"`. Without a
valid signature the monitor detects the missing entitlement at startup and exits
with a clear message rather than hanging.

## Producer contract (the child)

At startup the child should:

1. Read `CRASH_MONITOR_SHM` from its environment — the monitor sets it to the
   region name before `exec`.
2. `shm_open` + `mmap` that name and verify the header magic.
3. Write into the shared sections (see
   [shared-memory.md](shared-memory.md)):
   - **breadcrumbs** — a short trail of recent activity;
   - **annotations** — domain state as `key`/`value` string pairs;
   - **heartbeat** — bump the counter regularly from the main loop; this is what
     the ANR watchdog observes.
4. (Optional) request a manual snapshot at any time by raising `SIGUSR1` — the
   monitor captures a `snapshot` report and the app keeps running.

Everything here is best-effort: if the mapping fails the app runs normally and
the monitor still produces crash/thread/memory reports.

## Data directory

Where reports, sessions, and the optional config file live, in precedence order:

1. **`CRASH_MONITOR_DATA_DIR`** (environment, absolute path) — highest priority.
   Used for tests and for pointing a run at a specific location. The monitor
   passes its own environment to the child, so setting this once keeps both
   sides in agreement.
2. **A compile-time default** — a host may bake its own directory *name* by
   setting `CRASH_MONITOR_DATA_DIR_NAME` when compiling the monitor (consumed via
   `option_env!`; the C reporter has a matching `-DSUT_CRASH_DATA_DIR_NAME`
   define). Only the name is baked; it is resolved under `$HOME` at runtime, so
   the binary carries no build-machine path. This lets a packaged app use its own
   namespace with no runtime environment.
3. **`~/.crash_monitor`** — the generic tool default when neither is set.

The child (producer) and the monitor (consumer) must resolve to the *same*
directory; because both read the same env with the same fallback, they agree
automatically.

## ANR tuning

The watchdog timings default to production-safe values and can be overridden via
the environment (used mainly by tests to shorten them):

| Variable | Meaning |
|----------|---------|
| `CRASH_MONITOR_ANR_WARMUP_MS` | grace period after start before checking |
| `CRASH_MONITOR_ANR_THRESHOLD_MS` | how long the heartbeat may stall before it's an ANR |
| `CRASH_MONITOR_ANR_CHECK_INTERVAL_MS` | polling interval |
| `CRASH_MONITOR_ANR_COOLDOWN_MS` | quiet period after firing, to avoid repeats |

## Configuration file

An optional `crash_reporter.json` in the data directory disables report
triggers or specific plugins and tunes parameters. Report triggers and most
plugins are on by default; a few plugins remain explicitly opt-in.
Top-level `"enabled": false` is a report-generation kill switch: child
supervision and Mach replies continue, but capture, plugins, and artifact
writes do not run. Per-trigger controls live under `triggers`; see
[pipeline.md](pipeline.md#configuration) for their precedence and exact
semantics. A missing or invalid file currently falls back to defaults.
