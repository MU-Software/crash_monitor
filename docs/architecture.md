# Architecture

## Two-process model

crash_monitor runs as a **parent** process; the target application runs as its
**child**. The monitor never links the target's code and never shares its heap,
so a crash that corrupts the child's address space cannot stop the monitor from
producing a report. This is the core reason for the out-of-process design.

```
┌─ crash_monitor (parent) ─────────────┐        ┌─ target app (child) ─────────┐
│  Mach exception port (crashes)       │        │  maps POSIX shared memory    │
│  SIGUSR1 listener (manual snapshot)  │◀──shm──▶│  writes breadcrumbs/context  │
│  ANR watchdog (ready + heartbeat)    │        │  opts in, then bumps heartbeat│
│  plugin pipeline → JSON report       │        │  (optionally) raises SIGUSR1 │
└──────────────────────────────────────┘        └──────────────────────────────┘
             ▲  posix_spawn, task port acquired, exception port pre-installed
```

## Startup

1. Create a POSIX shared-memory region and a Mach exception port.
2. `posix_spawn` the child with the exception port pre-installed (survives
   `exec`) and `CRASH_MONITOR_SHM=<shm-name>` in its environment.
3. Acquire the child's task port (`task_for_pid`) — this needs the debugger
   entitlement and must succeed before monitoring starts, or early crashes
   can't be inspected.
4. Start the exception-listener thread and enter the event loop.

`posix_spawn` is the only child-creation path. The monitor prepares the process
group, signal mask/defaults, inherited descriptors, exception-port setup, and
shared-memory environment before spawning; it does not run application code in
a post-`fork` monitor process.

## Event sources

All sources feed a single event loop as a `MonitorEvent`:

| Source | Event | Report type |
|--------|-------|-------------|
| Mach exception port | `Crash` (SIGSEGV/SIGBUS/SIGABRT/SIGFPE …) | `crash` |
| `SIGUSR1` (self-pipe) | `Snapshot` | `snapshot` |
| ANR watchdog (inline) | fired when an opted-in producer's heartbeat stalls | `anr` |
| `waitpid` | non-zero child exit | `exit_failure` |
| `waitpid` | primary SIGKILL termination with opt-in OOM classification | `oom` (`possible_oom`, not confirmed) |
| `waitpid` | other primary signal termination | `signal_failure` |

A clean child exit ends the loop without a report. A Mach exception remains
the primary `crash` incident; the later wait status is attached as termination
metadata rather than firing a second waitpid-based report.

The **ANR watchdog** is a pure state machine polled from the event loop. It stays
disarmed until the producer release-publishes its first heartbeat and then the
explicit `producer_ready` handshake. Once armed, it reads the heartbeat every
*check interval*; if the value does not advance for the *threshold*, it emits an
ANR event, then enters a cooldown. A *warmup* delay starts only after readiness.
After synchronous Snapshot/ANR capture resumes the child, the event loop samples
the heartbeat again and removes exactly the monitor-owned interval from the ANR
clock. Unchanged-heartbeat stale time from before capture remains observable;
heartbeat progress establishes a fresh baseline. All four timings are
configurable (see [integration.md](integration.md)).

The event loop is driven through an `EventSource` trait, so it can be exercised
in-process with a synthetic source — no real child or Mach ports required.

## Capture lifecycle

On a crash/snapshot/ANR event:

1. **Suspend** the child task (frozen while inspected).
2. **Collect** the minimal per-thread state (registers and backtrace), loaded
   images + ASLR slides, and privacy-authorized optional evidence such as stack
   bytes, VM/heap summaries, or selected shared-memory sections.
3. **Resume** the child (for snapshots and ANRs it keeps running; for a fatal
   crash the exception reply lets it proceed to termination).
4. **Pre-process** the collected data (sessions, symbol names, fingerprint, …).
5. **Write** the report in stages (fail-safe metadata and authorized raw dumps
   → JSON).
6. **Post-process** (archive, prune, notify).

Steps 2 and 4–6 run through the [plugin pipeline](pipeline.md). The shared-memory
layout that couples the child (producer) and monitor (consumer) is described in
[shared-memory.md](shared-memory.md).

## Artifact lifecycle

Every event owns one immutable `ReportId` and one report-local transaction:

```text
event
  → pending/.report-<id>.pending/       hidden staging; partial by definition
  → report.json + optional raw/PNG      exact files registered in memory
  → report.zip                          default canonical artifact
  → manifest.json                       written and synced last
  → sent/<id>/                          one atomic directory publication
  → retention                           count/bytes/age cleanup after notification
```

The default pipeline archives the JSON and registered attachments into
`report.zip`, selects `sent/` as the destination, then publishes the complete
directory with `manifest.json` as its commit record. A deliberately disabled or
failed archiver can leave `report.json` as the canonical artifact; a disabled
move step publishes to `pending/<id>/`. Readers use the manifest rather than
guessing filenames. On restart, a staging directory whose synced manifest is
complete is recovered to its recorded destination; an incomplete staging
directory stays hidden and is eligible for bounded scavenging.

| Trigger | Capture-time evidence | Terminal metadata | Child after capture |
|---|---|---|---|
| `snapshot` | policy-authorized thread/image/SHM snapshot | none | resumed |
| `anr` | same snapshot plus watchdog context | none | resumed |
| `crash` | exception codes and bounded task snapshot | actual reaped status | terminates after Mach reply |
| `exit_failure` / `signal_failure` / `oom` | owned termination/process-output state; no live task capture | exit code or signal/core flag/runtime | already terminal |

See [reports.md](reports.md#location-and-lifecycle) for the manifest contract
and [privacy.md](privacy.md) for evidence gates and retention limitations.
