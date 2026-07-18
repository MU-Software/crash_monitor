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
`com.apple.security.cs.debugger` entitlement. `make sign` codesigns the binary
with [`crash_monitor.entitlements`](../crash_monitor.entitlements). Override the
signer with `make sign SIGN_IDENTITY="Developer ID Application: …"`. At startup
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
   5; older versions are rejected with no fallback, even though some older
   total size and many offsets are unchanged. A mismatch means the producer
   must not derive payload addresses or write to the mapping.
3. Include `producer/crash_monitor_producer.h` and use its heartbeat,
   breadcrumb, annotation, extension, attachment, and screenshot APIs. It uses
   the acquire/release and nonblocking seqlock helpers from the SSOT schema (see
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

### Managed data-directory security

`CRASH_MONITOR_DATA_DIR` and configured report roots must be absolute. Relative
paths, `..` traversal, symlink components, non-directory components, ancestors
writable by an untrusted account, and extended ACLs that grant additional
access fail closed. Darwin's root-owned `/tmp` and `/var` aliases are resolved
to `/private/tmp` and `/private/var`; no other symlink exception exists.

Crash Monitor creates and enforces `0700` on the managed data, `crashes`,
`pending`, `sent`, staging, and report directories. Managed JSON, raw evidence,
screenshots, attachments, ZIPs, manifests, logs, locks, and temporary files are
`0600`, independent of umask. Existing managed nodes must be regular files or
directories owned by the effective user. Owned POSIX mode drift is corrected;
wrong ownership, unsafe type, or an allowing extended ACL is rejected rather
than silently replaced. Trusted ancestors are validated but never chmod'ed.

Files are opened relative to validated directory descriptors with
`O_NOFOLLOW`; new files/directories use exclusive creation. Artifact files are
written to unique temporary names, synced, and published without clobbering an
existing destination. A report directory becomes visible only after its exact
artifact set and manifest are synced and the directory is atomically renamed.
Startup recovery uses the same validation before publishing a prepared report.

Multiple monitor processes can share one data root: unique `ReportId`
directories prevent normal name collisions, per-report owner locks keep
recovery/retention away from a live transaction, and rate-limit, session-log,
and retention state use bounded process/file locking. A lock conflict skips or
defers maintenance; it never authorizes overwriting another instance's report.

If startup rejects a configured path, stop all monitors using it and inspect
each component before changing anything:

```bash
ls -ldeO /absolute/path /absolute/path/crashes
chmod 700 /absolute/path /absolute/path/crashes
chmod -N /absolute/path /absolute/path/crashes  # remove an unintended macOS ACL
```

Do not recursively `chown` or delete a path until its intended owner and scope
are independently confirmed. A root-owned or foreign-owned final data
directory should be replaced by a newly created directory owned by the account
that runs Crash Monitor, then configured with its absolute path. Preserve
rejected report directories for investigation; after permissions are repaired,
restart so manifest recovery can decide whether they are complete.

## ANR tuning

The JSON config is authoritative. Its production defaults are:

```json
{
  "watchdog": {
    "warmup_ms": 10000,
    "threshold_ms": 5000,
    "check_interval_ms": 2000,
    "cooldown_ms": 60000
  }
}
```

All four values must be greater than zero. Operations and test harnesses may
explicitly set `CRASH_MONITOR_ALLOW_ENV_OVERRIDES=1` to enable these temporary
overrides; without that gate the environment cannot silently replace JSON:

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
Attachment source paths must resolve below the monitor's startup working
directory and name regular, non-symlink files. Start the monitor from the
application-owned attachment root (or a narrower directory) and do not register
paths outside it.
Top-level `"enabled": false` is a report-generation kill switch: child
supervision and Mach replies continue, but capture, plugins, and artifact
writes do not run. Per-trigger controls live under `triggers`; see
[configuration.md](configuration.md) for the complete JSON document, defaults,
ranges, override precedence, and exact trigger semantics. A missing file selects
the minimal privacy profile. An existing
unreadable, malformed, non-regular, or symlinked file fails startup before the
child is spawned.

Unknown fields are rejected. Enabled duration fields and
`pre_processors.fingerprint.top_frames` must be greater than zero.
`filters.rate_limiter.max_events=0` intentionally rejects every event; it does
not disable the filter. Enabled retention requires `max_reports>0`, while zero
`max_size_mb` or `max_age_days` are immediate-delete thresholds. Validate the
same loader and range rules without starting a child with
`crash_monitor check-config [--config PATH]`.
