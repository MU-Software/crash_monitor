# Shared-memory contract

The child (producer) and the monitor (consumer) communicate through a single
POSIX shared-memory region. Its byte layout is a strict cross-process,
cross-language contract: the producer writes a field at some offset and the
monitor reads it at the same offset, so both sides must agree exactly.

## Single source of truth

[`schema/crash_shm.h`](../schema/crash_shm.h) is the authoritative definition of
every shared structure. It contains layout only — no logic, no API — and depends
only on `<stdint.h>` / `<stdbool.h>`.

- The **C producer** includes this header directly.
- The **Rust monitor** generates its mirror from the same header with `bindgen`
  (see [`build.rs`](../build.rs)); compile-time size/offset assertions in
  `src/shm/types.rs` re-validate the generated layout as a second check.

Because the Rust types are generated from the C header, the two cannot drift.

## Region layout

The monitor creates and sizes the region; sections are laid out back to back and
their offsets are derived from the struct sizes (never hand-coded):

```
[ Header ][ Breadcrumb state ][ Crash context ][ Settings ][ Attachments ][ Screenshots ][ Canary ]
```

The region is a few tens of MB, dominated by the screenshot ring.

### Header (monitor-owned, 64 bytes)

Written by the monitor when it creates the region; the producer only skips past
it. It is deliberately **not** part of the schema. It carries a magic word
(`"CMON"`), a version, and the ring/screenshot dimensions. A `0xDEADBEEF` canary
at the very end of the region is checked to detect truncation/corruption.

### Breadcrumb rings

A fixed set of per-thread ring buffers (each entry is 64 bytes: timestamp,
thread id, category, severity, a truncated file name, line, and a short
message). Each thread owns its own ring and advances a private `write_idx`, so
recording a breadcrumb is lock-free. On capture the monitor merges all rings and
sorts by timestamp.

### Crash context (app-agnostic)

Typed fields that are meaningful to any application:

- `heartbeat_counter` — bumped by the app; the ANR watchdog watches it.
- session id + start time.
- build identity (version, git hash/dirty, build type, compiler, OS, …),
  populated by the host at init.

Application/domain state is **not** stored as typed fields. Instead it is carried
as a generic **annotation** array — a small set of `key`/`value` string pairs.
This keeps the schema and the monitor entirely app-agnostic: the monitor reads
annotations into a map and emits them verbatim, without knowing any app's domain.
A host records them via its own key/value API (e.g. `active_tool → "brush"`).

### Screenshots

A ring of RGBA slots (newest wins). The producer fills a slot's pixels, then
publishes it by storing into a per-slot `valid` flag with release ordering; the
monitor reads `valid` with acquire ordering.

## Atomics

Fields accessed atomically (the heartbeat counter, the screenshot `valid` flags)
are declared with **plain** integer types in the schema — `_Atomic` is not
representable by bindgen, and atomicity is an access property, not a layout one.
The C producer accesses them with `__atomic_*` builtins; the monitor reads them
volatile.

## Producer responsibilities

A child that wants rich reports should, at startup, read `CRASH_MONITOR_SHM` from
its environment, `shm_open` + `mmap` that name, verify the header magic, and then
write into the sections above. Nothing is mandatory — the monitor still captures
threads/memory/images for a child that writes nothing — but breadcrumbs, context,
and a regularly-advanced heartbeat are what make reports (and ANR detection)
useful. See [integration.md](integration.md).
