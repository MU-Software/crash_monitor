/* Deterministic runtime checks for the C shared-memory atomic helper contract. */

#include "crash_shm_atomic.h"

#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>

#define CHECK(condition)                                                                    \
    do {                                                                                    \
        if (!(condition)) {                                                                 \
            fprintf(stderr, "shm atomic contract failed at line %d: %s\n", __LINE__,       \
                    #condition);                                                            \
            return EXIT_FAILURE;                                                            \
        }                                                                                   \
    } while (0)

int main(void) {
    uint32_t value32 = 0;
    uint64_t value64 = 0;

    sut_shm_atomic_u32_store_release(&value32, UINT32_C(0xA5A55A5A));
    CHECK(sut_shm_atomic_u32_load_acquire(&value32) == UINT32_C(0xA5A55A5A));

    sut_shm_atomic_u64_store_release(&value64, UINT64_C(0x0123456789ABCDEF));
    CHECK(sut_shm_atomic_u64_load_acquire(&value64) == UINT64_C(0x0123456789ABCDEF));

    uint32_t generation = 0;
    uint32_t odd_generation = 0;
    CHECK(sut_shm_seqlock_try_begin(&generation, &odd_generation));
    CHECK(odd_generation == 1);
    CHECK(sut_shm_atomic_u32_load_acquire(&generation) == 1);

    uint32_t rejected_generation = UINT32_C(0xDEADBEEF);
    CHECK(!sut_shm_seqlock_try_begin(&generation, &rejected_generation));
    CHECK(rejected_generation == UINT32_C(0xDEADBEEF));
    CHECK(sut_shm_atomic_u32_load_acquire(&generation) == 1);

    sut_shm_seqlock_end(&generation, odd_generation);
    CHECK(sut_shm_atomic_u32_load_acquire(&generation) == 2);

    sut_shm_atomic_u32_store_release(&generation, UINT32_MAX - 1u);
    CHECK(sut_shm_seqlock_try_begin(&generation, &odd_generation));
    CHECK(odd_generation == UINT32_MAX);
    CHECK(sut_shm_atomic_u32_load_acquire(&generation) == UINT32_MAX);
    sut_shm_seqlock_end(&generation, odd_generation);
    CHECK(sut_shm_atomic_u32_load_acquire(&generation) == 0);

    return EXIT_SUCCESS;
}
