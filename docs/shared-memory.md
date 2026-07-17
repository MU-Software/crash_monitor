# Shared-memory contract

The child (producer) and the monitor (consumer) communicate through one POSIX
shared-memory region. Its byte layout and publication protocol are a strict
cross-process, cross-language ABI: both sides must agree on field offsets,
atomic alignment, schema version, and memory ordering.

## Single source of truth

[`schema/crash_shm.h`](../schema/crash_shm.h) is the authoritative layout. It
defines the 64-byte header, breadcrumb rings, crash context, settings,
attachments, screenshots, and their fixed-size constants. The current ABI is
**schema version 2**.

- C producers include `crash_shm.h` and use the publication helpers in
  [`schema/crash_shm_atomic.h`](../schema/crash_shm_atomic.h).
- The Rust monitor generates the C structure mirrors from the schema with
  `bindgen` (see [`build.rs`](../build.rs)).
- Both headers and the Rust mirror use compile-time size, alignment, and key
  offset assertions. The C helper additionally requires 32- and 64-bit atomic
  operations to be always lock-free.

A producer must verify both `SUT_SHM_MAGIC` and the exact `SUT_SHM_VERSION`
before deriving payload addresses or writing anything. A version mismatch is
not a best-effort compatibility mode: the producer must leave the region alone.

## Region layout

The monitor creates and sizes the region. Sections remain back to back:

```text
[ Header ][ Breadcrumb state ][ Crash context ][ Settings ][ Attachments ][ Screenshots ][ Canary ]
```

The screenshot ring dominates the roughly 50 MB region. Schema v2 reuses
previously reserved words for generations, so the 64-byte header, payload
offsets, and total region size remain unchanged from version 1.

### Header (64 bytes)

The monitor initializes the header, but its layout is part of the shared C
schema. Besides immutable dimensions, it contains these publication words:

| Offset | Field | Protected unit |
|---:|---|---|
| 16 | `breadcrumb_registry_generation` | ring registration and `ring_count` |
| 32 | `context_generation` | crash context except heartbeat |
| 36 | `settings_generation` | settings snapshot |
| 40 | `attachments_generation` | attachment section |

Each generation starts at zero. Odd values mean a write is in progress and even
values, including zero, are stable. Generation wraparound is allowed. Only the
screenshot-slot protocol gives zero the additional meaning “unpublished.”

## Capture ownership boundary

The live mapping is not exposed as ordinary Rust references or borrowed byte
slices. After the monitor successfully suspends the child, it copies the entire
schema-sized mapping through bounded raw-pointer chunks into an immutable owned
snapshot. Breadcrumb, context, settings, attachment, screenshot, and Stage 1
raw-dump readers consume only that snapshot. The task may therefore resume even
if a capture worker is still parsing payload bytes.

Suspension prevents bytes from changing during the copy, while generations
detect a producer that was suspended partway through a multi-field update. An
odd generation makes only its protected unit unavailable; the consumer does not
spin or resume the producer merely to finish that update. Before typed parsing
or Stage 1 persistence, the monitor sanitizes that unit in the owned snapshot so
torn payload bytes are not retained in either output path.

If suspension fails under the fatal-crash best-effort policy, the monitor skips
all non-atomic SHM payload reads. The watchdog heartbeat is the sole live-state
exception and uses its dedicated atomic API.

## Publication protocol

All writers use a single-attempt, nonblocking seqlock operation:

1. `sut_shm_seqlock_try_begin` performs at most one compare-exchange from an
   even generation to the following odd value.
2. If it fails, the producer skips or defers that update. It never spins in a
   crash-reporting path.
3. After writing every field in the protected unit, the producer calls
   `sut_shm_seqlock_end`, which release-stores the following even generation.

A concurrent consumer acquire-loads the generation, rejects an odd value,
copies the unit, then acquire-loads it again. It accepts the copy only when the
two values are identical and even. The monitor's suspended owned-snapshot path
cannot observe a generation change during its copy, but it still rejects an odd
value left by an interrupted producer. Consumers never wait for an odd value;
they drop only that ring, section, or screenshot slot.

### Breadcrumb registry and rings

Ring registration is published in this order:

1. Begin `header.breadcrumb_registry_generation`.
2. Initialize the selected ring under that ring's own `generation` using
   begin, payload writes, and end.
3. Release-store the new `crumb_state.ring_count`.
4. End `header.breadcrumb_registry_generation` with a release store.

The consumer validates the registry generation and acquire-loads `ring_count`
before iterating. It then validates every ring independently. A ring writer
begins that ring's `generation`, writes the breadcrumb plus `tid`, `write_idx`,
and `count`, and ends the generation. An odd or changed ring generation drops
only that ring; other registered rings remain usable.

### Crash context

`header.context_generation` protects all crash-context fields except
`heartbeat_counter`. The producer begins the context generation, writes the
session, build identity, annotation count, and annotation payload, then ends the
generation. An odd or changed value drops the context as one unit.

The heartbeat is deliberately independent because the watchdog must observe it
while the child is running. The producer release-stores an aligned `uint64_t`;
the monitor acquire-loads the matching aligned Rust `AtomicU64`. A heartbeat
operation publishes no other context field and does not participate in
`context_generation`.

### Settings

The producer begins `header.settings_generation`, writes the complete settings
snapshot, and ends it. An odd or changed value drops the settings unit without
retrying or affecting other sections.

### Attachments

The producer begins `header.attachments_generation`, writes slots first, writes
the bounded `count`, and ends the generation. The padding word at attachment
section offset 4 remains reserved for alignment; it is not another generation.
An odd or changed header generation drops the entire attachment registration
section. Attachment structures and the slot-count constant come from the C
schema rather than a handwritten Rust mirror.

### Screenshots

Each `valid[i]` word is now the screenshot slot's generation, not a boolean:

- `0`: never published;
- odd: pixels or metadata are being written;
- nonzero even: published generation.

The producer begins `valid[i]`, writes `data[i]`, `timestamp[i]`, and `tier[i]`,
then ends `valid[i]` with a release store. The consumer acquire-loads the slot
generation before and after copying its pixels and metadata and accepts only the
same nonzero even value. An odd or changed value drops that slot without
spinning; other slots remain eligible.

## Atomic ABI rules

Atomic words are declared as plain fixed-width integers in the schema because
C `_Atomic` fields are not represented portably by bindgen. This does not make
ordinary or volatile access valid. Every concurrent access uses aligned atomic
operations:

- C uses the acquire/release and seqlock helpers in `crash_shm_atomic.h`.
- Rust uses aligned `AtomicU32`/`AtomicU64` acquire and release operations.
- The ABI is accepted only when atomic size and alignment match the underlying
  integer and the target reports the operations as always lock-free.

There is no volatile fallback. Acquire/release orders publication; `volatile`
neither makes a cross-process data race safe nor detects torn multi-field data.

## Producer responsibilities

A producer reads `CRASH_MONITOR_SHM`, opens and maps that name, checks the
mapping size plus header magic/version, and follows the publication sequence for
every unit it writes. Nothing is mandatory: the monitor can still capture
threads, memory, and images for a child that writes no SHM payload. Breadcrumbs,
context, and a regularly advanced heartbeat make reports and ANR detection more
useful. The standalone example is
[`tests/e2e/fixtures/crash_app.c`](../tests/e2e/fixtures/crash_app.c).
