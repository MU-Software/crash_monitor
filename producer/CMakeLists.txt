cmake_minimum_required(VERSION 3.20)
project(crash_monitor_producer LANGUAGES C)

add_library(crash_monitor_producer INTERFACE)
target_include_directories(
    crash_monitor_producer
    INTERFACE
        $<BUILD_INTERFACE:${CMAKE_CURRENT_SOURCE_DIR}>
        $<BUILD_INTERFACE:${CMAKE_CURRENT_SOURCE_DIR}/../schema>
        $<INSTALL_INTERFACE:include/crash-monitor>
)
target_compile_features(crash_monitor_producer INTERFACE c_std_11)

install(
    FILES
        crash_monitor_producer.h
        ../schema/crash_shm.h
        ../schema/crash_shm_atomic.h
    DESTINATION include/crash-monitor
)
