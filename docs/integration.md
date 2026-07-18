# Integrating a host application

A host application is monitored simply by being launched as crash_monitor's
child:

```bash
crash_monitor run <path-to-app> [args…]
```

No source changes are required to get thread registers, backtraces, and
loaded-image metadata. Raw stack bytes, memory maps, and screenshots remain
explicit privacy opt-ins. To also get breadcrumbs, context, screenshots, and
ANR detection, the app links a small in-process reporter that writes to the
shared-memory region. This page covers both the runtime contract and the
configuration knobs.

## Supported platform

The release contract is native **arm64 macOS**. The current Mach thread-state,
register, and unwind implementations use the ARM64 ABI and the build fails at
compile time on x86_64, including Rosetta builds. The minimum supported system
is macOS 13 on Apple Silicon. CI and release packaging must build and test the
`aarch64-apple-darwin` target; adding Intel support requires separate thread
flavors, register layouts, unwind handling, and Mach-O slice selection first.

## Codesigning

The monitor calls `task_for_pid()` on the child, which requires the
`com.apple.security.cs.debugger` entitlement. `make build` codesigns the binary
with [`crash_monitor.entitlements`](../crash_monitor.entitlements). Override the
signer with `make build SIGN_IDENTITY="Developer ID Application: …"`. At startup
the monitor first runs the platform's strict code-signature verification, then
parses the signed entitlement plist and accepts only a boolean `true`. Unsigned
code, an invalid/malformed signature, a malformed plist, a missing key, boolean
`false`, and a non-boolean value produce distinct diagnostics before any child
is launched.

## Producer contract (the child)

At startup the child should:

1. Read `CRASH_MONITOR_SHM` from its environment — the monitor sets it to the
   region name before `exec`.
2. `shm_open` + `mmap` the complete schema-sized region, then verify both the
   header magic and the exact `SUT_SHM_VERSION`. The current schema is version
   4; versions 1, 2, and 3 are rejected with no fallback, even though their
   total size and many offsets are unchanged. A mismatch means the producer
   must not derive payload addresses or write to the mapping.
3. Write into the shared sections using the acquire/release and nonblocking
   seqlock helpers from `schema/crash_shm_atomic.h` (see
   [shared-memory.md](shared-memory.md)):
   - **breadcrumbs** — a short trail of recent activity;
   - **annotations** — domain state as `key`/`value` string pairs;
   - **heartbeat / ANR opt-in** — release-store the first heartbeat, then
     release-store `SUT_SHM_PRODUCER_READY` to `header.producer_ready`, and keep
     advancing the heartbeat from the observed application loop.
4. (Optional) request a manual snapshot at any time by raising `SIGUSR1` — the
   monitor captures a `snapshot` report and the app keeps running.

Everything here is best-effort: if the mapping fails the app runs normally and
the monitor still produces crash/thread/memory reports.

ANR monitoring is dormant until the exact producer-ready handshake is observed.
A child that does not integrate SHM therefore cannot be reported as hung merely
because the monitor-created region remains zero-filled. The first ready sample
sets a baseline; warmup and hang accounting begin only after readiness.

All producer text fields are fixed-width C arrays. Each value must include a
NUL within its own array bound; the bytes before it must be valid UTF-8 and must
not contain Unicode control characters. The consumer omits a malformed typed
unit. `git_dirty` is the fixed-width `uint8_t` wire boolean: zero means false and
every nonzero value means true. Breadcrumb producers use the schema's category
range `0..=SUT_CRUMB_CATEGORY_MAX` and the `SUT_CRUMB_SEV_*` constants; the
consumer drops an entry with an out-of-range category or severity. See the
[wire-value validity contract](shared-memory.md#wire-value-validity) for the
complete consumer policy.

Keep `ring_count`, `annotation_count`, and attachment `count` within their
schema maxima. The consumer rejects the owning registry, context, or attachment
section when one of these array bounds is invalid; it does not clamp the value.

An `N`-byte text field therefore holds at most `N - 1` bytes plus its NUL. Do
not publish an exact-width unterminated hash, filename, or path; attachment paths
must also be representable as UTF-8.

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
| `CRASH_MONITOR_ANR_WARMUP_MS` | grace period after producer readiness before checking |
| `CRASH_MONITOR_ANR_THRESHOLD_MS` | how long the heartbeat may stall before it's an ANR |
| `CRASH_MONITOR_ANR_CHECK_INTERVAL_MS` | polling interval |
| `CRASH_MONITOR_ANR_COOLDOWN_MS` | quiet period after firing, to avoid repeats |

Synchronous monitor capture time is excluded. After a Snapshot or ANR capture
has resumed the child, the event loop acquire-samples readiness and heartbeat.
If the heartbeat advanced, that sample becomes a fresh baseline and clears the
hang accumulator. If it is unchanged, stale application-running time observed
before capture remains valid while only the measured monitor-owned interval is
removed from elapsed time. Existing warmup and cooldown budgets are preserved;
their remaining time advances only for application-running intervals on either
side of capture. Background finalization does not stop the child and is not
excluded.

## Configuration file

An optional `crash_reporter.json` in the data directory disables report
triggers or specific plugins and tunes parameters. Report triggers and
non-sensitive plugins are on by default. Raw stack bytes, memory diagnostics,
environment data, screenshots, attachments, and raw SHM persistence require an
explicit privacy profile, consent assertion, and evidence-specific opt-in; see
[privacy.md](privacy.md).
Top-level `"enabled": false` is a report-generation kill switch: child
supervision and Mach replies continue, but capture, plugins, and artifact
writes do not run. Per-trigger controls live under `triggers`; see
[pipeline.md](pipeline.md#configuration) for their precedence and exact
semantics. A missing file selects the minimal privacy profile. An existing
unreadable, malformed, non-regular, or symlinked file fails startup before the
child is spawned.
