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
shutdown cannot wait forever.

| Boundary | Deadline / bound | Expiry policy |
|----------|------------------|---------------|
| Mach live-task capture | 5 s absolute from request receipt | publish cancellation, resume/reply, retire worker, finalize minimum event metadata |
| Snapshot/ANR queue submission | non-blocking, capacity 2 | log and drop when full/disconnected |
| Background shutdown drain | 2 s | detach any worker still running |
| Fatal termination handoff | no independent timer | wait for the supervisor's explicit reason/`None`, or channel disconnect; never write JSON/ZIP first |
| Fatal or task-independent terminal finalization | 310 s supervisor wait | detach and report no final artifact if the worker has not completed |
| Feedback dialog | 300 s, enforced by the dialog post-processor | kill its process group and continue finalization |

Other worker plugins currently have no hard per-plugin deadline. They are
panic-isolated and kept off the event-loop thread; killable per-plugin deadlines
belong to P0-03.

## Failure policy

- If capture reaches its absolute deadline or its capture thread/collector
  fails, the monitor discards any unfinished mutable worker state, creates an
  immutable minimum crash payload, and immediately proceeds to resume and
  reply. The timed-out worker is retired and cannot accept another capture.
- A manual snapshot or ANR that requires a consistent suspended snapshot is
  skipped when task suspension fails. It is not finalized from inconsistent
  live data.
- When a collector returns an ordinary error, earlier completed collector data
  is retained. A deadline timeout is different: the in-flight mutable payload
  is quarantined as a unit and only event metadata enters the minimum snapshot.
- A queue-full snapshot/ANR is logged and dropped. It does not block capture,
  resume, reply, or child-state observation.
- Fatal finalization waits for the supervisor's termination handoff before
  producing JSON/ZIP. If that handoff fails, the failure is recorded and the
  available minimum crash evidence is retained.

## Panic and timeout isolation

Plugin panics are isolated with `catch_unwind`, so one plugin cannot unwind
through the worker or discard already completed stages. Release profiles must
therefore continue to use `panic = "unwind"`.

The finalization worker deliberately does **not** arm process-global `SIGALRM`.
An alarm set by one worker can be delivered to the event loop or Mach listener,
and it cannot reliably stop CPU-bound or otherwise non-cooperative code. Worker
execution currently provides panic isolation and critical-path isolation, not
forced termination of arbitrary plugin code.

Hard cancellation for non-cooperative plugins is deferred to P0-03, where such
plugins run in a killable process. Until then, a hung worker is contained by the
bounded queue and two-second shutdown-drain policy; the supervisor stops
waiting at the capture deadline and does not delay a Mach reply for the worker.

Cancellation is cooperative at collector boundaries in this phase. An
individual collector or Mach kernel call already in progress cannot be killed
inside the Rust process; after the supervisor deadline its late result is
quarantined and discarded, but the call may return after task resume. Likewise,
`task_suspend`/`task_resume` themselves are synchronous kernel calls. P0-03 is
the work item that turns non-cooperative execution into a hard kill guarantee.

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
8. **Input size caps** — bounded report / dSYM / decoded-stack sizes guard
   against pathological input.
9. **No cross-category plugin dependencies** — a plugin's `depends_on` may name
   only same-category plugins, keeping ordering acyclic and local.

## Configuration

All plugins are enabled by default (opt-out design). An optional
`crash_reporter.json` in the data directory disables specific plugins or adjusts
parameters (rate-limit window, retention limits, fingerprint frame count, …). A
missing or malformed file falls back to defaults silently — configuration can
never prevent a report from being written.
