/*
 * crash_shm_atomic.h -- C publication helpers for crash_shm.h.
 *
 * Shared fields stay plain uint32_t/uint64_t in the schema for bindgen. Access
 * to a field named as atomic by the schema contract must go through these
 * helpers. They require GCC/Clang __atomic builtins and never fall back to a
 * process-local lock.
 */
#ifndef CRASH_SHM_ATOMIC_H_
#define CRASH_SHM_ATOMIC_H_

#include "crash_shm.h"

#include <stdbool.h>
#include <stdatomic.h>

#if !defined(__clang__) && !defined(__GNUC__)
#error "crash_shm_atomic.h requires GCC/Clang lock-free __atomic builtins"
#endif

_Static_assert(sizeof(_Atomic(uint32_t)) == sizeof(uint32_t),
               "C atomic uint32_t size is not ABI-compatible");
_Static_assert(_Alignof(_Atomic(uint32_t)) == _Alignof(uint32_t),
               "C atomic uint32_t alignment is not ABI-compatible");
_Static_assert(sizeof(_Atomic(uint64_t)) == sizeof(uint64_t),
               "C atomic uint64_t size is not ABI-compatible");
_Static_assert(_Alignof(_Atomic(uint64_t)) == _Alignof(uint64_t),
               "C atomic uint64_t alignment is not ABI-compatible");
_Static_assert(__atomic_always_lock_free(sizeof(uint32_t), 0),
               "shared uint32_t atomics must always be lock-free");
_Static_assert(__atomic_always_lock_free(sizeof(uint64_t), 0),
               "shared uint64_t atomics must always be lock-free");

/* mmap supplies a page-aligned base. Assert both the structure-relative and
 * full-region offsets used for every atomic publication word. */
_Static_assert(offsetof(sut_shm_header_t, breadcrumb_registry_generation) %
                       _Alignof(_Atomic(uint32_t)) ==
                   0,
               "breadcrumb registry generation is not atomically aligned");
_Static_assert(offsetof(sut_shm_header_t, context_generation) %
                       _Alignof(_Atomic(uint32_t)) ==
                   0,
               "context generation is not atomically aligned");
_Static_assert(offsetof(sut_shm_header_t, settings_generation) %
                       _Alignof(_Atomic(uint32_t)) ==
                   0,
               "settings generation is not atomically aligned");
_Static_assert(offsetof(sut_shm_header_t, attachments_generation) %
                       _Alignof(_Atomic(uint32_t)) ==
                   0,
               "attachments generation is not atomically aligned");
_Static_assert(offsetof(sut_crumb_ring_t, generation) % _Alignof(_Atomic(uint32_t)) == 0,
               "breadcrumb ring generation is not atomically aligned");
_Static_assert(offsetof(sut_crumb_state_t, ring_count) % _Alignof(_Atomic(uint32_t)) == 0,
               "breadcrumb ring_count is not atomically aligned");
_Static_assert(offsetof(sut_crash_context_t, heartbeat_counter) %
                       _Alignof(_Atomic(uint64_t)) ==
                   0,
               "heartbeat counter is not atomically aligned");
_Static_assert(offsetof(sut_screenshot_section_t, valid) % _Alignof(_Atomic(uint32_t)) == 0,
               "screenshot generations are not atomically aligned");
_Static_assert((sizeof(sut_shm_header_t) + offsetof(sut_crumb_state_t, ring_count)) %
                       _Alignof(_Atomic(uint32_t)) ==
                   0,
               "mapped breadcrumb ring_count is not atomically aligned");
_Static_assert((sizeof(sut_shm_header_t) + offsetof(sut_crumb_state_t, rings) +
                offsetof(sut_crumb_ring_t, generation)) %
                       _Alignof(_Atomic(uint32_t)) ==
                   0,
               "mapped breadcrumb ring generation is not atomically aligned");
_Static_assert(sizeof(sut_crumb_ring_t) % _Alignof(_Atomic(uint32_t)) == 0,
               "breadcrumb ring stride breaks atomic alignment");
_Static_assert((sizeof(sut_shm_header_t) + sizeof(sut_crumb_state_t) +
                offsetof(sut_crash_context_t, heartbeat_counter)) %
                       _Alignof(_Atomic(uint64_t)) ==
                   0,
               "mapped heartbeat counter is not atomically aligned");
_Static_assert((sizeof(sut_shm_header_t) + sizeof(sut_crumb_state_t) +
                sizeof(sut_crash_context_t) + sizeof(sut_crash_settings_snapshot_t) +
                sizeof(sut_shm_attachment_section_t) +
                offsetof(sut_screenshot_section_t, valid)) %
                       _Alignof(_Atomic(uint32_t)) ==
                   0,
               "mapped screenshot generations are not atomically aligned");

static inline uint32_t sut_shm_atomic_u32_load_acquire(const uint32_t* value) {
    return __atomic_load_n(value, __ATOMIC_ACQUIRE);
}

static inline void sut_shm_atomic_u32_store_release(uint32_t* value, uint32_t desired) {
    __atomic_store_n(value, desired, __ATOMIC_RELEASE);
}

static inline uint64_t sut_shm_atomic_u64_load_acquire(const uint64_t* value) {
    return __atomic_load_n(value, __ATOMIC_ACQUIRE);
}

static inline void sut_shm_atomic_u64_store_release(uint64_t* value, uint64_t desired) {
    __atomic_store_n(value, desired, __ATOMIC_RELEASE);
}

/* Attempt to claim a seqlock exactly once. This function never waits or spins.
 * On success, odd_generation receives the odd writer token that must be passed
 * to sut_shm_seqlock_end after all payload writes are complete. */
static inline bool sut_shm_seqlock_try_begin(uint32_t* generation, uint32_t* odd_generation) {
    uint32_t observed = sut_shm_atomic_u32_load_acquire(generation);
    if ((observed & 1u) != 0u) return false;

    const uint32_t desired = observed + 1u;
    if (!__atomic_compare_exchange_n(generation, &observed, desired, false, __ATOMIC_ACQ_REL,
                                     __ATOMIC_ACQUIRE)) {
        return false;
    }

    *odd_generation = desired;
    return true;
}

static inline void sut_shm_seqlock_end(uint32_t* generation, uint32_t odd_generation) {
    sut_shm_atomic_u32_store_release(generation, odd_generation + 1u);
}

#endif /* CRASH_SHM_ATOMIC_H_ */
