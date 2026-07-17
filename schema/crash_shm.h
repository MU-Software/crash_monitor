/*
 * crash_shm.h -- Crash-report shared-memory LAYOUT schema (single source of truth).
 *
 * This header defines ONLY the memory layout of the POSIX shared-memory region
 * shared between the in-process C crash reporter (producer) and the
 * out-of-process Rust monitor (consumer). It contains no logic and no API.
 *
 * The Rust monitor generates its type mirror from this header (bindgen), so this
 * file is the authoritative definition of the C<->Rust shm contract.
 *
 * Keep it dependency-free (standard headers only) and layout-stable.
 */
#ifndef CRASH_SHM_H_
#define CRASH_SHM_H_

#include <stddef.h>
#include <stdint.h>

/* Fields accessed atomically remain plain fixed-width integers in this layout
 * header because bindgen does not model C _Atomic fields. Both processes MUST
 * access those words through their language's aligned atomic operations. C
 * producers use the helpers in crash_shm_atomic.h. */

/* ── Region header — 64 bytes, initialized by the monitor ── */
#define SUT_SHM_MAGIC   0x434D4F4Eu /* "CMON" (Crash MONitor) */
#define SUT_SHM_VERSION 4u
#define SUT_SHM_CANARY  0xDEADBEEFu
#define SUT_SHM_TOTAL_SIZE 50033364u
#define SUT_SHM_PRODUCER_NOT_READY 0u
#define SUT_SHM_PRODUCER_READY     1u

typedef struct sut_shm_header {
    uint32_t magic;
    uint32_t version;
    uint32_t ring_capacity_per_thread;
    uint32_t max_threads;

    /* Seqlock for ring registration and the breadcrumb-state ring_count. */
    uint32_t breadcrumb_registry_generation;

    uint32_t screenshot_slots;
    uint32_t screenshot_width;
    uint32_t screenshot_height;

    /* Independent seqlocks for the corresponding multi-field sections. */
    uint32_t context_generation;
    uint32_t settings_generation;
    uint32_t attachments_generation;

    /* ANR opt-in: release-store 1 only after publishing the first heartbeat. */
    uint32_t producer_ready;
    uint8_t reserved[16];
} sut_shm_header_t;

_Static_assert(sizeof(sut_shm_header_t) == 64, "sut_shm_header_t must be 64 bytes");
_Static_assert(offsetof(sut_shm_header_t, magic) == 0, "header magic offset changed");
_Static_assert(offsetof(sut_shm_header_t, version) == 4, "header version offset changed");
_Static_assert(offsetof(sut_shm_header_t, breadcrumb_registry_generation) == 16,
               "breadcrumb registry generation offset changed");
_Static_assert(offsetof(sut_shm_header_t, screenshot_slots) == 20,
               "screenshot slot-count offset changed");
_Static_assert(offsetof(sut_shm_header_t, context_generation) == 32,
               "context generation offset changed");
_Static_assert(offsetof(sut_shm_header_t, settings_generation) == 36,
               "settings generation offset changed");
_Static_assert(offsetof(sut_shm_header_t, attachments_generation) == 40,
               "attachments generation offset changed");
_Static_assert(offsetof(sut_shm_header_t, producer_ready) == 44,
               "producer readiness offset changed");
_Static_assert(SUT_SHM_PRODUCER_NOT_READY == 0, "producer not-ready value changed");
_Static_assert(SUT_SHM_PRODUCER_READY == 1, "producer ready value changed");

/* ── Breadcrumb category ── */
#define SUT_CRUMB_CATEGORY_MAX 10u

typedef enum sut_crumb_category {
    SUT_CRUMB_CAT_TOOL = 0,
    SUT_CRUMB_CAT_WORLD,
    SUT_CRUMB_CAT_UNDO,
    SUT_CRUMB_CAT_MESH,
    SUT_CRUMB_CAT_IO,
    SUT_CRUMB_CAT_RENDER,
    SUT_CRUMB_CAT_INPUT,
    SUT_CRUMB_CAT_MEMORY,
    SUT_CRUMB_CAT_THREAD,
    SUT_CRUMB_CAT_LIFECYCLE,
    SUT_CRUMB_CAT_USER = SUT_CRUMB_CATEGORY_MAX,
} sut_crumb_category_t;

_Static_assert(SUT_CRUMB_CAT_TOOL == 0, "breadcrumb category minimum changed");
_Static_assert(SUT_CRUMB_CAT_USER == SUT_CRUMB_CATEGORY_MAX,
               "breadcrumb category maximum changed");

/* ── Breadcrumb severity (fixed-width wire values) ── */
#define SUT_CRUMB_SEV_INFO      0u
#define SUT_CRUMB_SEV_WARN      1u
#define SUT_CRUMB_SEV_ERROR     2u
#define SUT_CRUMB_SEVERITY_MAX SUT_CRUMB_SEV_ERROR

_Static_assert(SUT_CRUMB_SEV_INFO == 0, "breadcrumb INFO severity changed");
_Static_assert(SUT_CRUMB_SEV_WARN == 1, "breadcrumb WARN severity changed");
_Static_assert(SUT_CRUMB_SEV_ERROR == SUT_CRUMB_SEVERITY_MAX,
               "breadcrumb severity maximum changed");

/* ── Breadcrumb entry — 64-byte fixed stride ── */
typedef struct sut_breadcrumb {
    uint64_t timestamp_ns; /* monotonic nanoseconds */
    uint32_t thread_id;    /* Mach thread ID (macOS) or pthread ID */
    uint16_t category;     /* sut_crumb_category_t */
    uint16_t severity;     /* SUT_CRUMB_SEV_* */
    char file[16];         /* basename tail: at most 15 UTF-8 bytes plus NUL */
    uint16_t line;         /* __LINE__ */
    uint16_t _pad;
    char message[28]; /* snprintf-formatted short description */
} sut_breadcrumb_t;

_Static_assert(sizeof(sut_breadcrumb_t) == 64, "sut_breadcrumb_t must be 64 bytes");

/* ── Per-thread ring buffer ── */
#define SUT_CRUMB_RING_CAPACITY 512 /* per thread: 512 × 64B = 32KB */
#define SUT_CRUMB_MAX_THREADS   8

typedef struct sut_crumb_ring {
    sut_breadcrumb_t buf[SUT_CRUMB_RING_CAPACITY];
    uint32_t write_idx;  /* this thread only; protected by generation */
    uint32_t count;      /* total recorded (for wrap detection) */
    uint32_t tid;        /* Mach/pthread thread ID */
    uint32_t generation; /* per-ring seqlock; odd means write in progress */
} sut_crumb_ring_t;

/* Global breadcrumb state (placed in shared memory) */
typedef struct sut_crumb_state {
    sut_crumb_ring_t rings[SUT_CRUMB_MAX_THREADS];
    uint32_t ring_count; /* published with a release store */
} sut_crumb_state_t;

_Static_assert(sizeof(sut_crumb_ring_t) == 32784, "sut_crumb_ring_t size changed");
_Static_assert(offsetof(sut_crumb_ring_t, generation) == 32780,
               "breadcrumb ring generation offset changed");
_Static_assert(sizeof(sut_crumb_state_t) == 262280, "sut_crumb_state_t size changed");
_Static_assert(offsetof(sut_crumb_state_t, ring_count) == 262272,
               "breadcrumb ring_count offset changed");

/* ── App-supplied annotation (generic key-value) ──
 * Domain state (active tool, counts, undo depth, user tags, ...) is recorded as
 * string key-value pairs rather than typed fields, so the monitor stays entirely
 * app-agnostic. A host app records these via sut_crash_set_annotation(); the
 * typed convenience setters (sut_crash_set_counts, ...) are thin wrappers. */
#define SUT_CRASH_MAX_ANNOTATIONS 16

typedef struct sut_crash_annotation {
    char key[32];
    char value[64];
} sut_crash_annotation_t;

_Static_assert(sizeof(sut_crash_annotation_t) == 96, "sut_crash_annotation_t must be 96 bytes");

/* ── Crash context — app-agnostic engine snapshot ── */
typedef struct sut_crash_context {
    /* ANR detection heartbeat (accessed atomically; see note above) */
    uint64_t heartbeat_counter;

    /* Session */
    uint64_t session_start_ns;
    char session_id[37]; /* UUID v4 (36 chars + NUL) */

    /* Build identification (populated from CMake defines at init) */
    char app_version[16];
    uint32_t build_number;
    char git_hash[16];
    uint8_t git_dirty; /* wire boolean: 0 is false; every nonzero value is true */
    char build_type[16];
    char build_preset[16];
    char build_timestamp[24];
    char compiler[32];
    char os_version[32];

    /* App-supplied domain state as generic key-value annotations. */
    int32_t annotation_count;
    sut_crash_annotation_t annotations[SUT_CRASH_MAX_ANNOTATIONS];
} sut_crash_context_t;

_Static_assert(sizeof(sut_crash_context_t) == 1760, "sut_crash_context_t size changed");
_Static_assert(offsetof(sut_crash_context_t, heartbeat_counter) == 0,
               "heartbeat counter offset changed");
_Static_assert(offsetof(sut_crash_context_t, git_dirty) == 92,
               "git_dirty offset changed");
_Static_assert(offsetof(sut_crash_context_t, annotation_count) == 216,
               "annotation count offset changed");

/* ── Settings snapshot ── */
typedef struct sut_crash_settings_snapshot {
    int32_t world_bound_min[3];
    int32_t world_bound_max[3];
    int32_t palette_count;
    int32_t history_max;
    char extra[128];
} sut_crash_settings_snapshot_t;

_Static_assert(sizeof(sut_crash_settings_snapshot_t) == 160,
               "sut_crash_settings_snapshot_t size changed");

/* ── Registered attachment paths ── */
#define SUT_SHM_MAX_ATTACHMENTS 4

typedef struct sut_shm_attachment_slot {
    char label[32];
    char path[256];
} sut_shm_attachment_slot_t;

typedef struct sut_shm_attachment_section {
    uint32_t count;
    uint32_t _pad; /* keep slots 8-byte aligned and preserve the v1 layout */
    sut_shm_attachment_slot_t slots[SUT_SHM_MAX_ATTACHMENTS];
} sut_shm_attachment_section_t;

_Static_assert(sizeof(sut_shm_attachment_slot_t) == 288,
               "sut_shm_attachment_slot_t size changed");
_Static_assert(sizeof(sut_shm_attachment_section_t) == 1160,
               "sut_shm_attachment_section_t size changed");
_Static_assert(offsetof(sut_shm_attachment_section_t, slots) == 8,
               "attachment slots offset changed");

/* ── Screenshot ring (RGBA slots, newest wins) ──
 * valid[i] is a per-slot generation: 0 means unpublished, odd means a write is
 * in progress, and nonzero even means published. Producer publication is
 * begin(odd), data/timestamp/tier writes, end(even release). */
#define SUT_SCREENSHOT_SLOTS  96
#define SUT_SCREENSHOT_WIDTH  480
#define SUT_SCREENSHOT_HEIGHT 270
#define SUT_SCREENSHOT_BYTES  (SUT_SCREENSHOT_WIDTH * SUT_SCREENSHOT_HEIGHT * 4)

typedef struct sut_screenshot_section {
    uint32_t valid[SUT_SCREENSHOT_SLOTS];
    uint64_t timestamp[SUT_SCREENSHOT_SLOTS];
    uint32_t tier[SUT_SCREENSHOT_SLOTS];
    uint8_t data[SUT_SCREENSHOT_SLOTS][SUT_SCREENSHOT_BYTES];
} sut_screenshot_section_t;

_Static_assert(sizeof(sut_screenshot_section_t) == 49767936,
               "sut_screenshot_section_t size changed");
_Static_assert(offsetof(sut_screenshot_section_t, valid) == 0,
               "screenshot generation offset changed");
_Static_assert(offsetof(sut_screenshot_section_t, timestamp) == 384,
               "screenshot timestamp offset changed");
_Static_assert(offsetof(sut_screenshot_section_t, tier) == 1152,
               "screenshot tier offset changed");
_Static_assert(offsetof(sut_screenshot_section_t, data) == 1536,
               "screenshot data offset changed");
_Static_assert(SUT_SHM_TOTAL_SIZE ==
                   sizeof(sut_shm_header_t) + sizeof(sut_crumb_state_t) +
                       sizeof(sut_crash_context_t) + sizeof(sut_crash_settings_snapshot_t) +
                       sizeof(sut_shm_attachment_section_t) +
                       sizeof(sut_screenshot_section_t) + sizeof(uint32_t),
               "shared-memory total size changed");

#endif /* CRASH_SHM_H_ */
