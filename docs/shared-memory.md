# Shared-memory contract

The child (producer) and the monitor (consumer) communicate through one POSIX
shared-memory region. Its byte layout and publication protocol are a strict
cross-process, cross-language ABI: both sides must agree on field offsets,
atomic alignment, schema version, and memory ordering.

## Single source of truth

[`schema/crash_shm.h`](../schema/crash_shm.h) is the authoritative layout. It
defines the 64-byte header, breadcrumb rings, crash context, settings,
attachments, screenshots, and their fixed-size constants. The current ABI is
**schema version 4**.

- C producers include `crash_shm.h` and use the publication helpers in
  [`schema/crash_shm_atomic.h`](../schema/crash_shm_atomic.h).
- The Rust monitor generates the C structure mirrors from the schema with
  `bindgen` (see [`build.rs`](../build.rs)).
- Both headers and the Rust mirror use compile-time size, alignment, and key
  offset assertions. The C helper additionally requires 32- and 64-bit atomic
  operations to be always lock-free.

A producer must verify both `SUT_SHM_MAGIC` and the exact `SUT_SHM_VERSION`
before deriving payload addresses or writing anything. The monitor likewise
accepts only its exact schema version. Version 4 rejects versions 1, 2, and 3,
and a version 4 producer must not write an older version. There is no fallback
based on common sizes or preserved offsets: on any version mismatch, the
producer leaves the region alone and the consumer omits its SHM payload.
Producer and monitor releases therefore have to upgrade in lockstep. In
particular, a legacy producer that ignores the version cannot be made safe by a
new monitor merely placing a newer version in the monitor-owned header; mixing
such binaries is unsupported.

## Region layout

The monitor creates and sizes the region. Sections remain back to back:

```text
[ Header ][ Breadcrumb state ][ Crash context ][ Settings ][ Attachments ][ Screenshots ][ Canary ]
```

That order and every section offset are generated from `sut_shm_region_t`; C
producers must address sections through that type rather than repeating offset
arithmetic. Any change to a field, array bound, section order, alignment,
publication meaning, or accepted wire value requires incrementing
`SUT_SHM_VERSION` in the same change. Adding assertions, aliases, or helper
APIs that leave the bytes and semantics unchanged does not require a bump.

The screenshot ring dominates the roughly 50 MB region. Schema v2 reused
reserved or padding words and also changed the meaning of the former header
offset-16 `ring_count` and screenshot `valid` words. Schema v3 replaces the wire
C `bool` with a fixed-width byte and adds semantic constants. Schema v4 uses a
reserved header word for producer readiness. The 64-byte header, every payload
offset, and the total region size remain unchanged across versions 1 through 4,
but the publication semantics are intentionally incompatible.

### Header (64 bytes)

The monitor initializes the header, but its layout is part of the shared C
schema. Besides immutable dimensions, it contains these publication words:

| Offset | Field | Protected unit |
|---:|---|---|
| 16 | `breadcrumb_registry_generation` | ring registration and `ring_count` |
| 32 | `context_generation` | crash context except heartbeat |
| 36 | `settings_generation` | settings snapshot |
| 40 | `attachments_generation` | attachment section |
| 44 | `producer_ready` | ANR opt-in after the first heartbeat publication |

The first four rows are generations: zero is stable, odd values mean a write is
in progress, and nonzero even values are stable. Generation wraparound is
allowed. Only the screenshot-slot protocol gives zero the additional meaning
“unpublished.” `producer_ready` is not a generation. It is a monotonic atomic
handshake whose only valid values are `SUT_SHM_PRODUCER_NOT_READY = 0` and
`SUT_SHM_PRODUCER_READY = 1`; every other untrusted value is treated as not
ready.

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
for example, `file[16]` and `git_hash[16]` carry at most 15 bytes. This strict
rule was introduced in version 3 and remains part of version 4; it rejects
producers that fill every byte without a terminator, including older exact-width
hashes or filenames. Attachment paths are also
UTF-8 wire text, so a native Unix path containing non-UTF-8 bytes cannot be
published through this typed field.

If any check fails, the consumer omits the owning typed unit rather than
constructing a partial or replacement-character value. Thus an invalid
breadcrumb file/message drops that breadcrumb, an invalid context or settings
string omits that typed section, and an invalid attachment label/path omits that
attachment slot. Independent stable units remain available. Raw owned-snapshot
sanitization for torn generations is a separate publication-integrity rule.

## Capture ownership boundary

## Report field provenance

Stable crash-context and producer-extension publications are serialized as the typed
`crash_context` and `producer_extension` report objects. Both carry
`source = "producer_shared_memory"`. `session_id`, `session_start_ns`, and
`heartbeat_counter` therefore describe the producer's last published state,
not values synthesized by the monitor. Empty `session_id`, zero
`session_start_ns` and an empty extension map mean unavailable;
the heartbeat counter is always emitted, including zero. Producer readiness is
validated separately and is not inferred from that counter.

The report's top-level `session` object has a different source: it is the
monitor-maintained session lock. Consumers should not merge the two identities.

The live mapping is not exposed as ordinary Rust references or borrowed byte
slices. After the monitor successfully suspends the child, it allocates one
zero-filled schema-sized destination and copies only privacy-authorized sections
through bounded raw-pointer chunks. Header/footer validation metadata is always
copied; breadcrumb/context, attachment, and screenshot payloads are selected by
the immutable collection policy. Their parsers and explicitly enabled Stage-1
raw persistence consume only that owned snapshot. The task may therefore resume
even if a capture worker is still parsing payload bytes. Denied screenshot
pixels and attachment paths are never copied from the live mapping.

Suspension prevents bytes from changing during the copy, while generations
detect a producer that was suspended partway through a multi-field update. An
odd generation makes only its protected unit unavailable; the consumer does not
spin or resume the producer merely to finish that update. Before typed parsing
or Stage 1 persistence, the monitor sanitizes that unit in the owned snapshot so
torn payload bytes are not retained in either output path.

If suspension fails under the fatal-crash best-effort policy, the monitor skips
all non-atomic SHM payload reads. The watchdog readiness/heartbeat pair is the
sole live-state exception and uses a dedicated ordered atomic API.

## Publication protocol

All multi-field publication-unit writers use a single-attempt, nonblocking
seqlock operation:

1. `sut_shm_seqlock_try_begin` performs at most one compare-exchange from an
   even generation to the following odd value.
2. If it fails, the producer skips or defers that update. It never spins in a
   crash-reporting path.
3. After writing every field in the protected unit, the producer calls
   `sut_shm_seqlock_end`, which release-stores the following even generation.

The publication protocol permits a consumer to acquire-load the generation,
reject an odd value, copy the unit, then acquire-load it again. A copy is valid
only when the two values are identical and even. The production monitor API
requires task suspension before copying payload bytes, so it normally cannot
observe a generation change, but it still rejects an odd value left by an
interrupted producer. The concurrent stress fixture exercises the same
before/after validation without suspension. Neither path waits for an odd
value; it drops only that ring, section, or screenshot slot.

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
while the child is running. ANR monitoring is explicitly producer-owned:

1. The producer release-stores its first aligned `uint64_t` heartbeat. The
   initial value may be zero.
2. Once the heartbeat source can continue running, the producer release-stores
   `SUT_SHM_PRODUCER_READY` to `header.producer_ready` and never clears it.
3. The producer continues release-storing heartbeat updates from the observed
   application loop.

The monitor acquire-loads readiness first and arms the watchdog only for the
exact ready value. It then acquire-loads the matching Rust `AtomicU64`. The
first ready observation establishes only the heartbeat baseline; elapsed time
from before readiness is never considered a hang. A heartbeat operation
publishes no other context field and does not participate in
`context_generation`.

### Producer extension

The producer begins `header.settings_generation`, writes extension schema
version 1 plus up to four bounded generic key/value entries, and ends it. An
odd/changed generation, unsupported version, invalid count, or malformed string
drops the extension without affecting other sections. Product concepts belong
in these values or generic annotations, never in the core wire layout.

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

The numeric `tier` is retained in raw and report attachment metadata. Lower
tier values have higher selection priority; frames within a tier are ordered
newest first, followed by ascending slot index as a stable tie-breaker. Report
capture retains at most 8 frames and 4 MiB of
RGBA data, checking the collector deadline before every slot copy. A budget
omission is deterministic and appears in `ScreenshotCollector` diagnostics.
The later PNG conversion runs under its own post-processor deadline. Screenshot
attachment metadata retains the selected slot's `tier` and `timestamp_ns`.

## Atomic ABI rules

Atomic words are declared as plain fixed-width integers in the schema because
C `_Atomic` fields are not represented portably by bindgen. This does not make
ordinary or volatile access valid. Every concurrent access uses aligned atomic
operations:

- C uses the acquire/release and seqlock helpers in `crash_shm_atomic.h`.
- Rust uses aligned `AtomicU32`/`AtomicU64` acquire and release operations.
- The Rust policy-selected copy is split around every atomic word; ordinary raw
  copy instructions never read a generation, readiness, `ring_count`, or
  heartbeat word. Atomics belonging only to a denied payload section are not
  loaded either.
- The ABI is accepted only when atomic size and alignment match the underlying
  integer and the target reports the operations as always lock-free.

There is no volatile fallback. Acquire/release orders publication; `volatile`
neither makes a cross-process data race safe nor detects torn multi-field data.

## Producer responsibilities

A producer reads `CRASH_MONITOR_SHM`, opens and maps that name, checks the
mapping size plus header magic/version, and follows the publication sequence for
every unit it writes. Nothing is mandatory: the monitor can still capture its
configured thread/image diagnostics for a child that writes no SHM payload.
ANR remains dormant for such a child. A producer opts into ANR only through the ordered first
heartbeat plus `producer_ready` handshake above. Breadcrumbs and context remain
independently optional. The standalone example is
[`tests/e2e/fixtures/crash_app.c`](../tests/e2e/fixtures/crash_app.c).
