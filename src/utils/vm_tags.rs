//! macOS VM user tag label mapping.

/// Map VM user tags to human-readable names.
pub fn user_tag_label(tag: u32) -> &'static str {
    match tag {
        1 => "malloc",
        2 => "malloc_small",
        3 => "malloc_large",
        4 => "malloc_huge",
        5 => "sbrk",
        6 => "realloc",
        7 => "malloc_tiny",
        8 => "malloc_large_reusable",
        9 => "malloc_large_reused",
        10 => "malloc_nano",
        11 => "malloc_medium",
        30 => "Stack",
        31 => "Guard",
        33 => "shared_memory",
        35 => "dylib",
        36 => "objc_dispatchers",
        37 => "unshared_pmap",
        40 => "appkit",
        41 => "foundation",
        43 => "coreservices",
        44 => "carbon",
        45 => "java",
        46 => "coredata",
        47 => "coredata_objectids",
        50 => "iokit",
        51 | 73 => "libdispatch",
        52 => "accelerate",
        53 => "coreui",
        55 => "dyld",
        56 => "dyld_malloc",
        60 => "sqlite",
        61 => "javascript_core",
        62 => "javascript_jit_executable_allocator",
        63 => "javascript_jit_register_file",
        64 => "glsl",
        65 => "opencl",
        66 => "coreimage",
        67 => "webcore_purgeable_buffers",
        69 => "imageio",
        70 => "coreprofile",
        71 => "assetsd",
        72 => "os_alloc_once",
        74 => "neon",
        75 => "iosurface",
        76 => "libnetwork",
        77 => "audio",
        78 => "videobitstream",
        79 => "atoms",
        80 => "cm_xpc",
        81 => "cm_rpc",
        82 => "cm_memorypool",
        83 => "cm_readcache",
        85 => "lowvm_object",
        86 => "gpu_memory",
        87 => "cm_creadphotodatamodel",
        _ => "",
    }
}

/// Returns true if the `user_tag` is in the malloc family (tags 1-11, 56).
pub fn is_malloc_tag(tag: u32) -> bool {
    (1..=11).contains(&tag) || tag == 56
}

#[cfg(test)]
#[path = "../../tests/unit/utils/vm_tags_tests.rs"]
mod tests;
