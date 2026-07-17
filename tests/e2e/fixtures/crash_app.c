/**
 * E2E test child for crash_monitor.
 *
 * Usage: crash_app <scenario>
 *   sigsegv  — NULL pointer dereference
 *   sigabrt  — abort()
 *   anr      — infinite loop (heartbeat stops)
 *   clean    — normal exit
 *
 * The monitor spawns this binary with CRASH_MONITOR_SHM env set.
 * This app calls sut_crash_reporter_init() to map the shm region.
 */

#include "sut_crash_reporter.h"
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

int main(int argc, char* argv[]) {
    if (argc < 2) {
        fprintf(stderr, "usage: crash_app <sigsegv|sigabrt|anr|clean>\n");
        return 1;
    }

    /* Initialize crash reporter — maps shm from CRASH_MONITOR_SHM env */
    sut_crash_reporter_init();
    sut_crash_session_begin();
    SUT_CRUMB(SUT_CRUMB_CAT_LIFECYCLE, "crash_app started scenario=%s", argv[1]);

    if (strcmp(argv[1], "sigsegv") == 0) {
        volatile int* p = NULL;
        *p = 42; /* SIGSEGV */
    } else if (strcmp(argv[1], "sigabrt") == 0) {
        abort();
    } else if (strcmp(argv[1], "anr") == 0) {
        /* Infinite loop — heartbeat stops advancing */
        for (;;) {
            sleep(1);
        }
    } else if (strcmp(argv[1], "clean") == 0) {
        sut_crash_session_end(0);
    } else {
        fprintf(stderr, "unknown scenario: %s\n", argv[1]);
        return 1;
    }

    return 0;
}
