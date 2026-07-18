#include "crash_monitor_producer.h"
#include <stdlib.h>

#define CHECK(value) do { if (!(value)) return EXIT_FAILURE; } while (0)

int main(void) {
    sut_shm_region_t* region = calloc(1, SUT_SHM_TOTAL_SIZE);
    CHECK(region != NULL);
    region->header.magic = SUT_SHM_MAGIC;
    region->header.version = SUT_SHM_VERSION;
    region->header.ring_capacity_per_thread = SUT_CRUMB_RING_CAPACITY;
    region->header.max_threads = SUT_CRUMB_MAX_THREADS;
    region->settings.schema_version = SUT_PRODUCER_EXTENSION_VERSION;
    crash_monitor_producer_t producer;
    CHECK(crash_monitor_producer_init(&producer, region, SUT_SHM_TOTAL_SIZE) == 0);
    crash_monitor_mark_ready(&producer);
    CHECK(region->header.producer_ready == SUT_SHM_PRODUCER_READY);
    CHECK(crash_monitor_add_breadcrumb(&producer, 0, 7, 9, SUT_CRUMB_CAT_APPLICATION_0,
                                       SUT_CRUMB_SEV_INFO, "demo.c", 12, "started") == 0);
    CHECK(crash_monitor_set_annotation(&producer, "mode", "test") == 0);
    CHECK(crash_monitor_set_extension(&producer, "renderer", "metal") == 0);
    CHECK(crash_monitor_register_attachment(&producer, "log", "/tmp/demo.log") == 0);
    free(region);
    return EXIT_SUCCESS;
}
