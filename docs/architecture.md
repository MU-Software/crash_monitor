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
│  ANR watchdog (heartbeat polling)    │        │  bumps the heartbeat counter │
│  plugin pipeline → JSON report       │        │  (optionally) raises SIGUSR1 │
└──────────────────────────────────────┘        └──────────────────────────────┘
             ▲  fork + exec, task port acquired, exception port pre-installed
```

## Startup

1. Create a POSIX shared-memory region and a Mach exception port.
2. `posix_spawn` the child with the exception port pre-installed (survives
   `exec`) and `CRASH_MONITOR_SHM=<shm-name>` in its environment.
3. Acquire the child's task port (`task_for_pid`) — this needs the debugger
   entitlement and must succeed before monitoring starts, or early crashes
   can't be inspected.
4. Start the exception-listener thread and enter the event loop.

## Event sources

All sources feed a single event loop as a `MonitorEvent`:

| Source | Event | Report type |
|--------|-------|-------------|
| Mach exception port | `Crash` (SIGSEGV/SIGBUS/SIGABRT/SIGFPE …) | `crash` |
| `SIGUSR1` (self-pipe) | `Snapshot` | `snapshot` |
| ANR watchdog (inline) | fired when the heartbeat counter stalls | `anr` |
| OOM trigger (opt-in) | child killed under memory pressure | `oom` |
| `waitpid` | `ChildExited` / `ChildSignaled` / `ChildGone` | — (loop exit) |

The **ANR watchdog** is a pure state machine polled from the event loop: it
reads the heartbeat counter from shared memory every *check interval*; if the
value does not advance for the *threshold*, it emits an ANR event, then enters a
cooldown. A *warmup* delay suppresses false positives during startup. All four
timings are configurable (see [integration.md](integration.md)).

The event loop is driven through an `EventSource` trait, so it can be exercised
in-process with a synthetic source — no real child or Mach ports required.

## Capture lifecycle

On a crash/snapshot/ANR event:

1. **Suspend** the child task (frozen while inspected).
2. **Collect** per-thread state (registers, stack memory, backtrace), loaded
   images + ASLR slides, VM regions + heap summary, and the shared-memory
   breadcrumbs / context / screenshots.
3. **Resume** the child (for snapshots and ANRs it keeps running; for a fatal
   crash the exception reply lets it proceed to termination).
4. **Pre-process** the collected data (sessions, symbol names, fingerprint, …).
5. **Write** the report in stages (raw fail-safe dump → JSON).
6. **Post-process** (archive, prune, notify).

Steps 2 and 4–6 run through the [plugin pipeline](pipeline.md). The shared-memory
layout that couples the child (producer) and monitor (consumer) is described in
[shared-memory.md](shared-memory.md).
