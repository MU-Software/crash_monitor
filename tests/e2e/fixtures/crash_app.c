/**
 * crash_app.c — self-contained E2E crash producer for crash_monitor.
 *
 * Standalone: depends ONLY on the shared-memory schema (schema/crash_shm.h),
 * NOT on any host application's crash reporter. It maps the monitor-created
 * region from $CRASH_MONITOR_SHM, publishes a breadcrumb + minimal context via
 * the schema's release/seqlock contract, then triggers the requested scenario.
 *
 * Usage: crash_app <sigsegv|sigabrt|sigill|sigkill|wait|exit1|exit42|anr|clean|uninstrumented>
 *   sigsegv       — NULL pointer dereference
 *   sigabrt       — abort()
 *   sigterm       — terminate via an uncaught SIGTERM
 *   sigill        — terminate via an uncaught SIGILL
 *   sigkill       — terminate via SIGKILL (possible-OOM policy fixture)
 *   wait          — remain alive for externally delivered monitor signals
 *   exit1         — immediate non-zero exit (1)
 *   exit42        — immediate non-zero exit (42)
 *   anr           — hang forever after publishing one heartbeat (ANR)
 *   clean         — normal exit (no report expected)
 *   uninstrumented — run longer than an E2E ANR threshold without using SHM
 *
 * The monitor initializes the 64-byte region header. Its layout, magic, and
 * schema version are defined by crash_shm.h and validated before any payload
 * address is derived.
 */

#include "crash_shm_atomic.h"

#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

static uint64_t now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ULL + (uint64_t)ts.tv_nsec;
}

static int sleep_ms(uint64_t duration_ms) {
    struct timespec requested = {
        .tv_sec = (time_t)(duration_ms / 1000u),
        .tv_nsec = (long)((duration_ms % 1000u) * 1000000u),
    };
    struct timespec remaining;

    while (nanosleep(&requested, &remaining) != 0) {
        if (errno != EINTR) return -1;
        requested = remaining;
    }
    return 0;
}

static void publish_fixture_state(void) {
    const char* path = getenv("CRASH_APP_STATE_FILE");
    if (!path || path[0] == '\0') return;
    int fd = open(path, O_WRONLY | O_CREAT | O_EXCL, 0600);
    if (fd < 0) _Exit(124);
    const char* shm_name = getenv("CRASH_MONITOR_SHM");
    if (!shm_name) shm_name = "";
    if (dprintf(fd, "%ld\n%s\n", (long)getpid(), shm_name) < 0 || close(fd) != 0) {
        _Exit(124);
    }
}

/* Map the monitor-created region and populate a breadcrumb + context so the
 * report carries real producer-written data (exercising the C->Rust shm path).
 * Best-effort: on any failure we still run the scenario — the monitor catches
 * crashes regardless of what the producer managed to write. */
static void populate_shm(const char* scenario) {
    const char* name = getenv("CRASH_MONITOR_SHM");
    if (!name || name[0] == '\0') return;

    int fd = shm_open(name, O_RDWR, 0);
    if (fd < 0) return;
    struct stat st;
    if (fstat(fd, &st) != 0) {
        close(fd);
        return;
    }
    const size_t required = SUT_SHM_TOTAL_SIZE;
    if (st.st_size < 0 || (uintmax_t)st.st_size < (uintmax_t)required) {
        close(fd);
        return;
    }
    void* base = mmap(NULL, (size_t)st.st_size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    close(fd);
    if (base == MAP_FAILED) return;

    sut_shm_region_t* region = (sut_shm_region_t*)base;
    sut_shm_header_t* header = &region->header;
    if (sut_shm_atomic_u32_load_acquire(&header->magic) != SUT_SHM_MAGIC ||
        sut_shm_atomic_u32_load_acquire(&header->version) != SUT_SHM_VERSION) {
        munmap(base, (size_t)st.st_size);
        return;
    }

    sut_crumb_state_t* crumbs = &region->breadcrumbs;
    sut_crash_context_t* ctx = &region->context;

    /* Register ring 0 without waiting for another writer. The registry
     * generation protects registration metadata, the per-ring generation
     * protects the ring payload, and ring_count is the final release-published
     * bound consumed by the monitor. */
    uint32_t registry_write_generation;
    if (sut_shm_seqlock_try_begin(&header->breadcrumb_registry_generation,
                                  &registry_write_generation)) {
        const uint32_t ring_index = sut_shm_atomic_u32_load_acquire(&crumbs->ring_count);
        if (ring_index < SUT_CRUMB_MAX_THREADS) {
            sut_crumb_ring_t* ring = &crumbs->rings[ring_index];
            uint32_t ring_write_generation;
            if (sut_shm_seqlock_try_begin(&ring->generation, &ring_write_generation)) {
                ring->tid = 1;
                sut_breadcrumb_t* e =
                    &ring->buf[ring->write_idx & (SUT_CRUMB_RING_CAPACITY - 1u)];
                e->timestamp_ns = now_ns();
                e->thread_id = ring->tid;
                e->category = (uint16_t)SUT_CRUMB_CAT_LIFECYCLE;
                e->severity = (uint16_t)SUT_CRUMB_SEV_INFO;
                strncpy(e->file, "crash_app.c", sizeof(e->file) - 1);
                e->line = (uint16_t)__LINE__;
                snprintf(e->message, sizeof(e->message), "scenario=%s", scenario);
                ring->write_idx++;
                ring->count++;
                sut_shm_seqlock_end(&ring->generation, ring_write_generation);
                sut_shm_atomic_u32_store_release(&crumbs->ring_count, ring_index + 1u);
            }
        }
        sut_shm_seqlock_end(&header->breadcrumb_registry_generation,
                            registry_write_generation);
    }

    /* Minimal context uses its own nonblocking seqlock. */
    uint32_t context_write_generation;
    if (sut_shm_seqlock_try_begin(&header->context_generation, &context_write_generation)) {
        strncpy(ctx->annotations[0].key, "active_tool", sizeof(ctx->annotations[0].key) - 1);
        strncpy(ctx->annotations[0].value, "e2e_producer",
                sizeof(ctx->annotations[0].value) - 1);
        ctx->annotation_count = 1;
        strncpy(ctx->session_id, "00000000-0000-4000-8000-000000000000",
                sizeof(ctx->session_id) - 1);
        ctx->session_start_ns = now_ns();
        sut_shm_seqlock_end(&header->context_generation, context_write_generation);
    }

    /* Readiness is the final publication step: consumers that acquire-load it
     * are guaranteed to observe an initialized heartbeat baseline. The ANR
     * scenario intentionally leaves that first heartbeat unchanged. */
    sut_shm_atomic_u64_store_release(&ctx->heartbeat_counter, 1u);
    sut_shm_atomic_u32_store_release(&header->producer_ready, SUT_SHM_PRODUCER_READY);

    munmap(base, (size_t)st.st_size);
}

int main(int argc, char* argv[]) {
    if (argc < 2) {
        fprintf(stderr,
                "usage: crash_app "
                "<sigsegv|sigabrt|sigterm|sigill|sigkill|wait|exit1|exit42|anr|clean|uninstrumented>\n");
        return 1;
    }
    const char* scenario = argv[1];
    if (strcmp(scenario, "uninstrumented") != 0) {
        populate_shm(scenario);
    }
    publish_fixture_state();

    if (strcmp(scenario, "sigsegv") == 0) {
        volatile int* np = NULL;
        *np = 42; /* SIGSEGV */
    } else if (strcmp(scenario, "sigabrt") == 0) {
        abort();
    } else if (strcmp(scenario, "sigterm") == 0) {
        /* Make this deterministic even when the test runner ignored or blocked
         * SIGTERM before spawning us: restore the default (uncaught) action and
         * unblock it before raising it in this process. */
        if (signal(SIGTERM, SIG_DFL) == SIG_ERR) {
            _Exit(125);
        }
        sigset_t signals;
        sigemptyset(&signals);
        sigaddset(&signals, SIGTERM);
        if (sigprocmask(SIG_UNBLOCK, &signals, NULL) != 0) {
            _Exit(125);
        }
        if (raise(SIGTERM) != 0) {
            _Exit(125);
        }
        _Exit(125); /* SIGTERM unexpectedly returned instead of terminating. */
    } else if (strcmp(scenario, "sigill") == 0) {
        if (signal(SIGILL, SIG_DFL) == SIG_ERR || raise(SIGILL) != 0) {
            _Exit(125);
        }
        _Exit(125);
    } else if (strcmp(scenario, "sigkill") == 0) {
        if (raise(SIGKILL) != 0) {
            _Exit(125);
        }
        _Exit(125);
    } else if (strcmp(scenario, "wait") == 0) {
        for (;;) {
            pause();
        }
    } else if (strcmp(scenario, "exit1") == 0) {
        return 1;
    } else if (strcmp(scenario, "exit42") == 0) {
        return 42;
    } else if (strcmp(scenario, "anr") == 0) {
        for (;;) {
            pause(); /* hang; the published heartbeat never advances → ANR */
        }
    } else if (strcmp(scenario, "clean") == 0) {
        return 0;
    } else if (strcmp(scenario, "uninstrumented") == 0) {
        /* Stay alive well past the E2E watchdog threshold, without mapping or
         * populating CRASH_MONITOR_SHM, then exit normally. */
        return sleep_ms(1500u) == 0 ? 0 : 125;
    } else {
        fprintf(stderr, "unknown scenario: %s\n", scenario);
        return 1;
    }
    return 0;
}
