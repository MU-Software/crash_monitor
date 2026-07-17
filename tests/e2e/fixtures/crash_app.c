/**
 * crash_app.c — self-contained E2E crash producer for crash_monitor.
 *
 * Standalone: depends ONLY on the shared-memory schema (schema/crash_shm.h),
 * NOT on any host application's crash reporter. It maps the monitor-created
 * region from $CRASH_MONITOR_SHM, writes a breadcrumb + minimal context via the
 * schema layout, then triggers the requested scenario.
 *
 * Usage: crash_app <sigsegv|sigabrt|anr|clean>
 *   sigsegv  — NULL pointer dereference
 *   sigabrt  — abort()
 *   anr      — hang forever (heartbeat never advances → ANR)
 *   clean    — normal exit (no report expected)
 *
 * The region header (magic + version, 64 bytes) is monitor-owned and, by design,
 * NOT part of the schema (see the monitor's src/shm/types.rs). A producer only
 * needs two facts about it, mirrored below: it is 64 bytes and starts with the
 * magic word. Everything else is derived from the schema struct sizes.
 */

#include "crash_shm.h"

#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <time.h>
#include <unistd.h>

/* Monitor-owned region header — authoritative definition in src/shm/types.rs. */
#define SHM_HEADER_SIZE 64u
#define SHM_MAGIC 0x434D4F4Eu /* "CMON" (Crash MONitor) */

static uint64_t now_ns(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000000ULL + (uint64_t)ts.tv_nsec;
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
    void* base = mmap(NULL, (size_t)st.st_size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    close(fd);
    if (base == MAP_FAILED) return;

    /* Sanity: the monitor stamps the magic when it creates the region. */
    if (((const uint32_t*)base)[0] != SHM_MAGIC) return;

    uint8_t* p = (uint8_t*)base;
    sut_crumb_state_t* crumbs = (sut_crumb_state_t*)(p + SHM_HEADER_SIZE);
    sut_crash_context_t* ctx = (sut_crash_context_t*)(p + SHM_HEADER_SIZE + sizeof(sut_crumb_state_t));

    /* One breadcrumb into the thread-0 ring, then publish ring_count so the
     * monitor sees a complete ring (release-store pairs with its acquire read). */
    sut_crumb_ring_t* ring = &crumbs->rings[0];
    ring->tid = 1;
    sut_breadcrumb_t* e = &ring->buf[ring->write_idx & (SUT_CRUMB_RING_CAPACITY - 1)];
    e->timestamp_ns = now_ns();
    e->thread_id = ring->tid;
    e->category = (uint16_t)SUT_CRUMB_CAT_LIFECYCLE;
    e->severity = 0;
    strncpy(e->file, "crash_app.c", sizeof(e->file) - 1);
    e->line = (uint16_t)__LINE__;
    snprintf(e->message, sizeof(e->message), "scenario=%s", scenario);
    ring->write_idx++;
    ring->count++;
    __atomic_store_n(&crumbs->ring_count, 1u, __ATOMIC_RELEASE);

    /* Minimal context. heartbeat_counter is intentionally left at 0: the `anr`
     * scenario never advances it, which is exactly what the watchdog detects. */
    strncpy(ctx->active_tool, "e2e_producer", sizeof(ctx->active_tool) - 1);
    strncpy(ctx->session_id, "00000000-0000-4000-8000-000000000000", sizeof(ctx->session_id) - 1);
    ctx->session_start_ns = now_ns();
}

int main(int argc, char* argv[]) {
    if (argc < 2) {
        fprintf(stderr, "usage: crash_app <sigsegv|sigabrt|anr|clean>\n");
        return 1;
    }
    const char* scenario = argv[1];
    populate_shm(scenario);

    if (strcmp(scenario, "sigsegv") == 0) {
        volatile int* np = NULL;
        *np = 42; /* SIGSEGV */
    } else if (strcmp(scenario, "sigabrt") == 0) {
        abort();
    } else if (strcmp(scenario, "anr") == 0) {
        for (;;) {
            pause(); /* hang; heartbeat never advances → ANR */
        }
    } else if (strcmp(scenario, "clean") == 0) {
        return 0;
    } else {
        fprintf(stderr, "unknown scenario: %s\n", scenario);
        return 1;
    }
    return 0;
}
