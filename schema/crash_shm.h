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

#include <stdbool.h>
#include <stdint.h>

/* Fields that are accessed atomically by the producer are declared with plain
 * integer types here (not _Atomic): atomicity is an access property, not a
 * layout property, and _Atomic is not representable by bindgen. The C producer
 * uses __atomic_* builtins on these fields; the monitor reads them volatile. */

/* ── Breadcrumb category ── */
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
    SUT_CRUMB_CAT_USER,
} sut_crumb_category_t;

/* ── Breadcrumb entry — 64 bytes, cache-line aligned ── */
typedef struct sut_breadcrumb {
    uint64_t timestamp_ns; /* monotonic nanoseconds */
    uint32_t thread_id;    /* Mach thread ID (macOS) or pthread ID */
    uint16_t category;     /* sut_crumb_category_t */
    uint16_t severity;     /* SUT_CRUMB_SEV_INFO / WARN / ERROR */
    char file[16];         /* __FILE__ last 16 chars (path stripped) */
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
    uint32_t write_idx; /* this thread only — no atomic needed */
    uint32_t count;     /* total recorded (for wrap detection) */
    uint32_t tid;       /* Mach/pthread thread ID */
    uint32_t _pad;
} sut_crumb_ring_t;

/* Global breadcrumb state (placed in shared memory) */
typedef struct sut_crumb_state {
    sut_crumb_ring_t rings[SUT_CRUMB_MAX_THREADS];
    uint32_t ring_count; /* atomic: number of registered threads */
} sut_crumb_state_t;

/* ── Crash context — engine state snapshot ── */
typedef struct sut_crash_context {
    /* Engine state */
    char active_tool[32];
    int32_t region_count;
    int32_t voxel_count;
    int32_t undo_depth;
    int32_t redo_depth;
    uint32_t last_action_id;
    uint32_t frame_number;

    /* Memory */
    uint64_t alloc_count;
    uint64_t free_count;
    uint64_t alloc_bytes_total;

    /* Thread pool */
    int32_t thread_pool_size;
    int32_t active_batch;

    /* ANR detection heartbeat (accessed atomically; see note above) */
    uint64_t heartbeat_counter;

    /* Session */
    uint64_t session_start_ns;
    char session_id[37]; /* UUID v4 (36 chars + NUL) */

    /* User-defined key-value tags */
    char tags[4][2][64]; /* [slot][key=0/value=1][chars] */
    int32_t tag_count;

    /* Build identification (populated from CMake defines at init) */
    char app_version[16];
    uint32_t build_number;
    char git_hash[16];
    bool git_dirty;
    char build_type[16];
    char build_preset[16];
    char build_timestamp[24];
    char compiler[32];
    char os_version[32];
} sut_crash_context_t;

/* ── Settings snapshot ── */
typedef struct sut_crash_settings_snapshot {
    int32_t world_bound_min[3];
    int32_t world_bound_max[3];
    int32_t palette_count;
    int32_t history_max;
    char extra[128];
} sut_crash_settings_snapshot_t;

/* ── Screenshot ring (RGBA slots, newest wins) ──
 * Producer writes data[], then timestamp[]/tier[], then publishes valid[]
 * with release semantics; the monitor reads valid[] with acquire. */
#define SUT_SCREENSHOT_SLOTS  96
#define SUT_SCREENSHOT_WIDTH  480
#define SUT_SCREENSHOT_HEIGHT 270
#define SUT_SCREENSHOT_BYTES  (SUT_SCREENSHOT_WIDTH * SUT_SCREENSHOT_HEIGHT * 4)

typedef struct sut_screenshot_section {
    uint32_t valid[SUT_SCREENSHOT_SLOTS]; /* published via __atomic_store_n (release) */
    uint64_t timestamp[SUT_SCREENSHOT_SLOTS];
    uint32_t tier[SUT_SCREENSHOT_SLOTS];
    uint8_t data[SUT_SCREENSHOT_SLOTS][SUT_SCREENSHOT_BYTES];
} sut_screenshot_section_t;

#endif /* CRASH_SHM_H_ */
