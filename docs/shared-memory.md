# Shared-memory contract

The child (producer) and the monitor (consumer) communicate through one POSIX
shared-memory region. Its byte layout and publication protocol are a strict
cross-process, cross-language ABI: both sides must agree on field offsets,
atomic alignment, schema version, and memory ordering.

## Single source of truth

[`schema/crash_shm.h`](../schema/crash_shm.h) is the authoritative layout. It
defines the 64-byte header, breadcrumb rings, crash context, settings,
attachments, screenshots, and their fixed-size constants. The current ABI is
**schema version 3**.

- C producers include `crash_shm.h` and use the publication helpers in
  [`schema/crash_shm_atomic.h`](../schema/crash_shm_atomic.h).
- The Rust monitor generates the C structure mirrors from the schema with
  `bindgen` (see [`build.rs`](../build.rs)).
- Both headers and the Rust mirror use compile-time size, alignment, and key
  offset assertions. The C helper additionally requires 32- and 64-bit atomic
  operations to be always lock-free.

A producer must verify both `SUT_SHM_MAGIC` and the exact `SUT_SHM_VERSION`
before deriving payload addresses or writing anything. The monitor likewise
accepts only its exact schema version. Version 3 rejects version 1 and version 2
regions, and a version 3 producer must not write either older version. There is
no fallback based on common sizes or preserved offsets: on any version mismatch,
the producer leaves the region alone and the consumer omits its SHM payload.

## Region layout

The monitor creates and sizes the region. Sections remain back to back:

```text
[ Header ][ Breadcrumb state ][ Crash context ][ Settings ][ Attachments ][ Screenshots ][ Canary ]
```

The screenshot ring dominates the roughly 50 MB region. Schema v2 reused
previously reserved words for generations. Schema v3 replaces the wire C
`bool` with a fixed-width byte and adds semantic constants. The 64-byte header,
every payload offset, and the total region size remain unchanged across versions
1, 2, and 3, but that layout overlap does not make the versions compatible.

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

## Wire-value validity

Shared memory is untrusted input. Publication generations establish that a
copied unit is stable; they do not make its integers or byte arrays valid Rust
values. The consumer first copies wire integers as integers and validates their
meaning before constructing a typed report value.

`sut_crash_context_t.git_dirty` is a `uint8_t`, not a C `bool`. Its wire
contract is `0` = false and every nonzero byte = true. Consumers therefore read
the byte before converting it; values such as `1`, `2`, and `255` all mean true.

Breadcrumb categories are the inclusive range `0..=SUT_CRUMB_CATEGORY_MAX`
(`0..=10`). Severities are `SUT_CRUMB_SEV_INFO = 0`,
`SUT_CRUMB_SEV_WARN = 1`, and `SUT_CRUMB_SEV_ERROR = 2`, with
`SUT_CRUMB_SEVERITY_MAX = 2`. The producer writes only these schema constants.
The consumer drops a breadcrumb entry whose category or severity is outside
those ranges; it keeps other valid entries from the same ring.

Section counts must also fit their declared arrays: `ring_count` is
`0..=SUT_CRUMB_MAX_THREADS`, `annotation_count` is
`0..=SUT_CRASH_MAX_ANNOTATIONS`, and attachment `count` is
`0..=SUT_SHM_MAX_ATTACHMENTS`. An out-of-range value rejects the owning
breadcrumb registry, context, or attachment section instead of being clamped
into an apparently valid value. A ring's `count` and `write_idx` are different:
they are monotonic publication counters, so wrap indexing and limiting the
number of returned records to the ring capacity are valid operations.

Every fixed-width C `char` array that is consumed as text follows one strict
contract:

1. A NUL byte must occur within that array's fixed bound. The consumer never
   searches into the next field, and bytes after the first NUL are ignored.
2. The bytes before that NUL must be valid UTF-8. There is no lossy decoding.
3. The decoded text must contain no Unicode control character.

Consequently, an `N`-byte array carries at most `N - 1` text bytes plus its NUL;
for example, `file[16]` and `git_hash[16]` carry at most 15 bytes. Version 3
deliberately rejects producers that filled every byte without a terminator,
including older exact-width hashes or filenames. Attachment paths are also
UTF-8 wire text, so a native Unix path containing non-UTF-8 bytes cannot be
published through this typed field.

If any check fails, the consumer omits the owning typed unit rather than
constructing a partial or replacement-character value. Thus an invalid
breadcrumb file/message drops that breadcrumb, an invalid context or settings
string omits that typed section, and an invalid attachment label/path omits that
attachment slot. Independent stable units remain available. Raw owned-snapshot
sanitization for torn generations is a separate publication-integrity rule.

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
