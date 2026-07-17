# Plugin pipeline

Report generation is a pipeline of small plugins grouped into five categories.
A default pipeline is assembled per run; individual plugins can be toggled or
tuned by config (see [Configuration](#configuration)).

## Stages

The pipeline has a short live-task capture phase and a separate finalization
phase. Only the capture phase may hold a task suspension or inspect target
memory.

```
Mach request received (record monotonic timestamp)
   │
   ├─ live-task critical phase ──────────────────────────────────────┐
   │  suspend → capture collectors + owned SHM copy → release ports │
   │                                             → resume → reply    │
   └─────────────────────────────────────────────────────────────────┘
                              │
                              ▼ owned CapturedSnapshot (no task port)
                    bounded finalization worker
                              │
                              ▼
 Filter → PreProcessor / symbolication → Stage 1 → Stage 2 JSON
        → feedback / PNG / ZIP / move / retention → Notifier
```

For a Mach exception, the supervisor uses one absolute five-second wait budget
measured from the listener's Mach receive timestamp. The budget is not reset
for each collector. On the successful path, task suspension, capture collectors,
copying required shared-memory payload into owned data, and releasing captured
port rights all finish before task resume. At budget expiry, unfinished worker
state is quarantined as described below. The Mach reply is attempted immediately
after resume; no queue operation, filter, report write, or user interaction is
allowed between resume and reply.

The finalization entry point receives no task port and reads the owned SHM byte
copy stored in `CapturedEvent`; it never invokes the collector set. (The shared
`Pipeline` owner still contains the capture-only collectors and SHM handle, so
this is an enforced call-path boundary rather than a Rust capability type
boundary.) Filters, pre-processors (including symbolication), Stage-1 and
Stage-2 writes, feedback, PNG conversion, ZIP creation, relocation, retention,
and notifications therefore cannot delay task resume or the Mach reply.
Task-independent exit/signal reports use a dedicated bounded-wait finalizer as
well, so report serialization and notification code never runs on the event
loop thread.

| Phase | Child state/capability | Typical work |
|-------|------------------------|--------------|
| **Critical capture** | suspended only for the bounded capture window | threads, memory + heap, dylibs, owned breadcrumbs/context/screenshots/attachment registration |
| **Finalization worker** | no task/SHM argument; capture collectors are not invoked | filters, session, symbolication, fingerprint, raw/JSON/PNG writes, feedback, ZIP, move-to-sent, log rotation, retention, notifications |

(The exact roster is what the default pipeline registers; treat the source as
the authority, not this list.)

## Queue and shutdown policy

Snapshot and ANR finalization use a background queue with capacity two.
Submission is non-blocking. If the queue is full, the event is dropped and the
monitor logs the drop; the live task is never kept suspended while waiting for
queue capacity.

A fatal crash is not finalized as an ordinary background snapshot. After the
task is resumed and the Mach reply is attempted, the supervisor reaps the child
and hands the observed termination reason to the fatal finalizer. The worker
then creates the JSON and ZIP with that termination metadata. Finalization is
never awaited before reply or child reap begins.

During normal shutdown, the monitor drains queued background work for at most
two seconds. A worker that is still blocked after that deadline is detached so
shutdown cannot wait forever. Detaching bounds the caller's wait; it does not
terminate arbitrary in-process Rust or a synchronous OS call already in
progress. Work that is expected to wait indefinitely must use the subprocess
boundary below.

| Boundary | Deadline / bound | Expiry policy |
|----------|------------------|---------------|
| Mach live-task capture | 5 s absolute from request receipt | publish cancellation, resume/reply, retire worker, finalize minimum event metadata |
| Snapshot/ANR queue submission | non-blocking, capacity 2 | log and drop when full/disconnected |
| Background shutdown drain | 2 s | detach any worker still running |
| Fatal termination handoff | no independent timer | wait for the supervisor's explicit reason/`None`, or channel disconnect; never write JSON/ZIP first |
| Fatal or task-independent terminal finalization | 310 s caller wait | detach the in-process worker and return no artifact path to the caller; the worker may still finish later |
| Cooperative filter / collector / pre-processor | 1 s / 5 s / 2 s defaults | publish an invocation-local absolute deadline; plugin checkpoints stop further work and record `TimedOut` |
| Cooperative post-processor / notifier | 30 s / 5 s defaults | stop at the next checkpoint, retain completed earlier stages, and record `TimedOut` |
| Feedback dialog | 300 s hard deadline | kill its process group, reap the helper, record `TimedOut`, and continue finalization |
| System notification helper | 5 s hard deadline | kill its process group, reap it, record `TimedOut`, and continue finalization |

Every registered plugin declares `Cooperative` or `Subprocess`, and every
pipeline category consumes that declaration. `Cooperative` is reserved for
audited in-process implementations that receive a mandatory `PluginContext`
and check it at bounded work boundaries. `Subprocess` denotes a small trusted
in-process adapter whose blocking or untrusted payload must use the subprocess
supervisor described below. The pipeline rejects an adapter result if it did
not cross that boundary (except an explicitly recorded no-op invocation).
Plugin metadata (`name`, execution kind, priority, dependencies, availability,
and timeout) is cached, constant-time state; metadata access performs no I/O.

## Failure policy

- If capture reaches its outer absolute deadline, or the capture thread panics
  or disconnects, the monitor discards its unfinished mutable worker state,
  creates an immutable minimum crash payload, and immediately proceeds to
  resume and reply. The timed-out worker is retired and cannot accept another
  capture.
- A manual snapshot or ANR that requires a consistent suspended snapshot is
  skipped when task suspension fails. It is not finalized from inconsistent
  live data.
- When an individual collector returns an ordinary error or observes its
  cooperative plugin deadline, safely owned data already written by it and
  earlier collectors is retained. Only the outer capture deadline quarantines
  the worker's mutable payload as a unit and falls back to event metadata.
- A plugin deadline is recorded as `TimedOut`, separately from an ordinary
  returned error or a caught panic. Collector and pre-processor hard
  dependencies do not treat a timed-out prerequisite as successful.
  Post-processor dependencies are currently ordering-only, so later artifact
  cleanup can still run after a timeout; P0-05 makes that distinction explicit
  in configuration validation.
- A queue-full snapshot/ANR is logged and dropped. It does not block capture,
  resume, reply, or child-state observation.
- Fatal finalization waits for the supervisor's termination handoff before
  producing JSON/ZIP. If that handoff fails, the failure is recorded and the
  available minimum crash evidence is retained.

## Panic and timeout isolation

Plugin panics are isolated with `catch_unwind`, so one cooperative plugin cannot
unwind through the worker or discard already completed stages. Release profiles
must therefore continue to use `panic = "unwind"`.

Plugin timeout code never installs a signal handler and never arms `alarm()`.
Each invocation instead owns an absolute monotonic deadline and a cloneable
cancellation token. Concurrent plugins therefore cannot overwrite one another's
timer or alter the event loop's signal disposition. Cooperative plugins check
the context before work and within potentially long loops. Capture collectors
also share the supervisor's absolute cancellation flag, so expiry stops the
next checkpoint rather than starting another task-facing operation.

This cooperative mechanism diagnoses expiry at checkpoints and after the
plugin returns; it cannot preempt a syscall, Mach call, codec call, lock wait,
or CPU loop while execution is between checkpoints. A plugin whose
uncheckpointed work can remain unbounded must not be classified
`Cooperative`. Built-in cooperative plugins checkpoint their loops and apply
stage-specific caps where available (VM enumeration also has independent query
and failure budgets). ZIP archival accepts at most 256 matching entries, only
regular files, at most 256 MiB per file and 512 MiB total; file bodies are
streamed through 64 KiB checkpointed buffers. Move-to-sent accepts regular
files up to 576 MiB; an `EXDEV` fallback streams through the same buffer size to
a unique temporary destination before publishing it and deleting the source.
PNG conversion reads only regular report/RGBA files through one descriptor,
caps report JSON at 256 MiB and decoded RGBA at 128 MiB, and publishes through
unique temporary files. Session-log rotation reads no more than the configured
trigger plus 8 MiB and caps individual lines and line count.
A cooperative deadline is not a universal resource limit:
an individual synchronous filesystem, kernel, or codec call remains
non-preemptible, and artifact formats without a streaming byte budget can run
past the deadline before returning. Finalization isolation keeps that work away
from task resume and the Mach reply, but a detached in-process worker may keep
running. The outer capture supervisor still resumes/replies at its absolute
deadline and quarantines any late mutable result. `task_suspend` and
`task_resume` themselves are synchronous kernel calls too. The Stage-1/Stage-2
five-second setting has the same cooperative semantics: it diagnoses an
overrun after serialization or a synchronous write returns; it is not a hard
I/O preemption boundary.

Payloads deliberately designed to wait (the feedback UI and system notifier
helper) run through an `exec`-based subprocess supervisor; the trusted adapter
does not run Rust in the post-`fork` child. The child gets a dedicated process
group; stdout and stderr are drained concurrently, capped at 1 MiB per stream,
and an incomplete capture is an error rather than a partial success. At
deadline expiry the supervisor sends `SIGKILL` to the group and reaps the
direct child before returning `TimedOut`. It also removes same-group descendants
left by a normally exiting direct child. A child that does not become waitable
within the two-second cleanup grace produces an error instead of `TimedOut`;
a background waiter retains direct-child reaping ownership. Feedback and macOS
system notification use this boundary. This is lifecycle isolation, not an OS
sandbox: a malicious helper could deliberately create a new session/process
group and escape group cleanup, so only trusted executables may be configured.

## Stability invariants

The monitor is deliberately over-defensive — **losing crash data is the worst
outcome**, worse than a degraded report. The design rules:

1. **No `panic = "abort"`** in any profile — `catch_unwind` depends on unwinding.
2. **No `unwrap`/`expect` on the capture path** — use `?`, `.get()`, `unwrap_or`.
3. **One absolute capture deadline** — collector boundaries never reset the
   five-second deadline measured from Mach receipt.
4. **Resume and reply before finalization** — the finalization entry point has
   no task/SHM argument, and no finalization work runs between resume and reply.
5. **Partial success is preserved** — one collector failing never discards what
   the others already gathered.
6. **Stage-1 fail-safe** — a raw binary dump is written before JSON
   serialization, so a report exists even if formatting later fails.
7. **Bounded background work** — snapshot/ANR submission never waits for queue
   space, and shutdown never waits more than two seconds for a hung worker.
8. **No process-global plugin timers** — plugin deadlines never change signal
   dispositions or arm a process-wide timer; helper payloads designed to wait
   are owned, killable subprocesses.
9. **Input size caps** — bounded report / dSYM / decoded-stack sizes guard
   against pathological input.
10. **No cross-category plugin dependencies** — a plugin's `depends_on` may name
   only same-category plugins, keeping ordering acyclic and local.

## Configuration

Report triggers and most plugins are enabled by default (primarily opt-out
design). An optional `crash_reporter.json` in the data directory can disable
behavior, enable an opt-in plugin, or adjust parameters (rate-limit window,
retention limits, fingerprint frame count, …).

The top-level `enabled: false` is an absolute report-generation kill switch.
The monitor still supervises the child, replies to a Mach exception, and reaps
the child, but it does not suspend for capture, run collectors or finalization
plugins, read SHM evidence, or create raw/JSON/post-processed artifacts. There
is no implicit emergency-evidence exception to this switch.

`triggers.enabled` disables the whole trigger category. Each primary event can
also be controlled independently with `triggers.crash`, `exit_failure`,
`signal_failure`, `oom_detection`, `anr`, and `snapshot` (each is an object with
an `enabled` boolean). A probable OOM is a primary SIGKILL termination when
`oom_detection` is enabled; otherwise the same event may use `signal_failure`.
A received Mach exception is always the primary crash incident: its later wait
status enriches that incident and does not fire a second exit/signal/OOM report,
including when the crash trigger itself is disabled.

The configuration is loaded and normalized once per monitor run so startup and
the pipeline share one immutable policy snapshot. A missing or malformed file
currently falls back to the built-in defaults silently.
