/* Public C producer SDK for crash_monitor shared memory. */
#ifndef CRASH_MONITOR_PRODUCER_H_
#define CRASH_MONITOR_PRODUCER_H_

#include "crash_shm_atomic.h"
#include <stddef.h>
#include <stdint.h>
#include <string.h>

typedef struct crash_monitor_producer {
    sut_shm_region_t* region;
} crash_monitor_producer_t;

static inline void crash_monitor_copy_text(char* dst, size_t size, const char* src) {
    if (size == 0u) return;
    size_t length = src == NULL ? 0u : strlen(src);
    if (length >= size) length = size - 1u;
    if (length != 0u) memcpy(dst, src, length);
    memset(dst + length, 0, size - length);
}

static inline int crash_monitor_producer_init(crash_monitor_producer_t* producer, void* mapping,
                                               size_t mapping_size) {
    if (producer == NULL || mapping == NULL || mapping_size != SUT_SHM_TOTAL_SIZE) return -1;
    sut_shm_region_t* region = (sut_shm_region_t*)mapping;
    if (region->header.magic != SUT_SHM_MAGIC || region->header.version != SUT_SHM_VERSION ||
        region->header.ring_capacity_per_thread != SUT_CRUMB_RING_CAPACITY ||
        region->header.max_threads != SUT_CRUMB_MAX_THREADS) return -2;
    producer->region = region;
    return 0;
}

static inline uint64_t crash_monitor_heartbeat(crash_monitor_producer_t* producer) {
    return __atomic_add_fetch(&producer->region->context.heartbeat_counter, 1u, __ATOMIC_RELEASE);
}

static inline void crash_monitor_mark_ready(crash_monitor_producer_t* producer) {
    (void)crash_monitor_heartbeat(producer);
    sut_shm_atomic_u32_store_release(&producer->region->header.producer_ready,
                                     SUT_SHM_PRODUCER_READY);
}

static inline int crash_monitor_add_breadcrumb(crash_monitor_producer_t* producer,
                                                uint32_t thread_slot, uint32_t thread_id,
                                                uint64_t timestamp_ns, uint16_t category,
                                                uint16_t severity, const char* file, uint16_t line,
                                                const char* message) {
    if (thread_slot >= SUT_CRUMB_MAX_THREADS || category > SUT_CRUMB_CATEGORY_MAX ||
        severity > SUT_CRUMB_SEVERITY_MAX) return -1;
    sut_crumb_ring_t* ring = &producer->region->breadcrumbs.rings[thread_slot];
    uint32_t odd;
    if (!sut_shm_seqlock_try_begin(&ring->generation, &odd)) return 1;
    const uint32_t index = ring->write_idx % SUT_CRUMB_RING_CAPACITY;
    sut_breadcrumb_t* crumb = &ring->buf[index];
    memset(crumb, 0, sizeof(*crumb));
    crumb->timestamp_ns = timestamp_ns;
    crumb->thread_id = thread_id;
    crumb->category = category;
    crumb->severity = severity;
    crumb->line = line;
    crash_monitor_copy_text(crumb->file, sizeof(crumb->file), file);
    crash_monitor_copy_text(crumb->message, sizeof(crumb->message), message);
    ring->tid = thread_id;
    ring->write_idx = index + 1u;
    ring->count += 1u;
    sut_shm_seqlock_end(&ring->generation, odd);
    uint32_t count = sut_shm_atomic_u32_load_acquire(&producer->region->breadcrumbs.ring_count);
    if (count <= thread_slot) {
        sut_shm_atomic_u32_store_release(&producer->region->breadcrumbs.ring_count,
                                         thread_slot + 1u);
    }
    return 0;
}

static inline int crash_monitor_set_annotation(crash_monitor_producer_t* producer,
                                                const char* key, const char* value) {
    uint32_t odd;
    if (!sut_shm_seqlock_try_begin(&producer->region->header.context_generation, &odd)) return 1;
    sut_crash_context_t* context = &producer->region->context;
    if (context->annotation_count < 0 || context->annotation_count >= SUT_CRASH_MAX_ANNOTATIONS) {
        sut_shm_seqlock_end(&producer->region->header.context_generation, odd);
        return -1;
    }
    sut_crash_annotation_t* annotation = &context->annotations[context->annotation_count++];
    crash_monitor_copy_text(annotation->key, sizeof(annotation->key), key);
    crash_monitor_copy_text(annotation->value, sizeof(annotation->value), value);
    sut_shm_seqlock_end(&producer->region->header.context_generation, odd);
    return 0;
}

static inline int crash_monitor_set_extension(crash_monitor_producer_t* producer,
                                               const char* key, const char* value) {
    uint32_t odd;
    if (!sut_shm_seqlock_try_begin(&producer->region->header.settings_generation, &odd)) return 1;
    sut_crash_settings_snapshot_t* extension = &producer->region->settings;
    if (extension->entry_count >= SUT_PRODUCER_EXTENSION_MAX_ENTRIES) {
        sut_shm_seqlock_end(&producer->region->header.settings_generation, odd);
        return -1;
    }
    extension->schema_version = SUT_PRODUCER_EXTENSION_VERSION;
    sut_producer_extension_entry_t* entry = &extension->entries[extension->entry_count++];
    crash_monitor_copy_text(entry->key, sizeof(entry->key), key);
    crash_monitor_copy_text(entry->value, sizeof(entry->value), value);
    sut_shm_seqlock_end(&producer->region->header.settings_generation, odd);
    return 0;
}

static inline int crash_monitor_register_attachment(crash_monitor_producer_t* producer,
                                                     const char* label, const char* path) {
    uint32_t odd;
    if (!sut_shm_seqlock_try_begin(&producer->region->header.attachments_generation, &odd)) return 1;
    sut_shm_attachment_section_t* section = &producer->region->attachments;
    if (section->count >= SUT_SHM_MAX_ATTACHMENTS) {
        sut_shm_seqlock_end(&producer->region->header.attachments_generation, odd);
        return -1;
    }
    sut_shm_attachment_slot_t* slot = &section->slots[section->count++];
    crash_monitor_copy_text(slot->label, sizeof(slot->label), label);
    crash_monitor_copy_text(slot->path, sizeof(slot->path), path);
    sut_shm_seqlock_end(&producer->region->header.attachments_generation, odd);
    return 0;
}

static inline int crash_monitor_publish_screenshot(crash_monitor_producer_t* producer,
                                                   uint32_t slot, uint64_t timestamp_ns,
                                                   uint32_t tier, const uint8_t* rgba,
                                                   size_t rgba_size) {
    if (slot >= SUT_SCREENSHOT_SLOTS || rgba == NULL || rgba_size != SUT_SCREENSHOT_BYTES) return -1;
    uint32_t odd;
    if (!sut_shm_seqlock_try_begin(&producer->region->screenshots.valid[slot], &odd)) return 1;
    memcpy(producer->region->screenshots.data[slot], rgba, rgba_size);
    producer->region->screenshots.timestamp[slot] = timestamp_ns;
    producer->region->screenshots.tier[slot] = tier;
    sut_shm_seqlock_end(&producer->region->screenshots.valid[slot], odd);
    return 0;
}

#endif /* CRASH_MONITOR_PRODUCER_H_ */
