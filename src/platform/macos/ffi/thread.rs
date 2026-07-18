//! FFI functions for Mach thread inspection (excluded from coverage).

use mach2::port::mach_port_t;
use mach2::thread_act::thread_get_state;

use crate::platform::macos::thread::extract_thread_name;
use crate::platform::macos::types::{
    ARM_THREAD_STATE64, ARM_THREAD_STATE64_COUNT, MachError, mach_result,
};

// thread_info() is not exported by mach2 -- manual FFI declaration (same pattern as
// posix_spawnattr_setexceptionports_np in spawn.rs).
unsafe extern "C" {
    fn thread_info(
        target_act: mach_port_t,
        flavor: u32,
        thread_info_out: *mut i32,
        thread_info_out_cnt: *mut u32,
    ) -> i32;
}

/// `THREAD_EXTENDED_INFO` flavor for `thread_info()`.
const THREAD_EXTENDED_INFO: u32 = 5;
const THREAD_IDENTIFIER_INFO: u32 = 4;

/// Matches `thread_extended_info` from `<mach/thread_info.h>`.
#[repr(C)]
#[allow(clippy::struct_field_names)] // matches macOS kernel header naming (thread_extended_info)
struct ThreadExtendedInfo {
    pth_user_time: u64,
    pth_system_time: u64,
    pth_cpu_usage: i32,
    pth_policy: i32,
    pth_run_state: i32,
    pth_flags: i32,
    pth_sleep_time: i32,
    pth_curpri: i32,
    pth_priority: i32,
    pth_maxpriority: i32,
    pth_name: [u8; 64],
}

#[repr(C)]
struct ThreadIdentifierInfo {
    thread_id: u64,
    thread_handle: u64,
    dispatch_qaddr: u64,
}

/// Get the system-wide stable thread identifier rather than exposing the
/// monitor-local Mach port name as report identity.
pub fn get_thread_identifier(thread: mach_port_t) -> Result<u64, MachError> {
    let mut info = ThreadIdentifierInfo {
        thread_id: 0,
        thread_handle: 0,
        dispatch_qaddr: 0,
    };
    let mut count =
        (std::mem::size_of::<ThreadIdentifierInfo>() / std::mem::size_of::<u32>()) as u32;
    // SAFETY: thread_info writes at most `count` natural words to this layout,
    // which mirrors the SDK's thread_identifier_info_data_t.
    let kr = unsafe {
        thread_info(
            thread,
            THREAD_IDENTIFIER_INFO,
            std::ptr::from_mut(&mut info).cast::<i32>(),
            &raw mut count,
        )
    };
    mach_result("thread_info(THREAD_IDENTIFIER_INFO)", kr)?;
    Ok(info.thread_id)
}

/// Get ARM64 register state from a thread.
///
/// # Errors
/// Returns `MachError` if `thread_get_state` fails or the returned count is unexpected.
pub fn get_thread_state(
    thread: mach_port_t,
) -> Result<[u32; ARM_THREAD_STATE64_COUNT as usize], MachError> {
    let mut state = [0u32; ARM_THREAD_STATE64_COUNT as usize];
    let mut count = ARM_THREAD_STATE64_COUNT;

    // SAFETY: thread_get_state reads register state into our buffer.
    let kr = unsafe {
        thread_get_state(
            thread,
            ARM_THREAD_STATE64,
            state.as_mut_ptr().cast::<u32>(),
            &raw mut count,
        )
    };
    mach_result("thread_get_state", kr)?;

    if count != ARM_THREAD_STATE64_COUNT {
        return Err(MachError {
            function: "thread_get_state(unexpected count)",
            kern_return: -1,
        });
    }

    Ok(state)
}

/// Get the name of a thread via `thread_info(THREAD_EXTENDED_INFO)`.
/// Returns `Ok(None)` if the thread has no name set.
///
/// # Errors
/// Returns `MachError` if the `thread_info` kernel call fails.
pub fn get_thread_name(thread: mach_port_t) -> Result<Option<String>, MachError> {
    let mut info: ThreadExtendedInfo = unsafe { std::mem::zeroed() };
    let mut count = (std::mem::size_of::<ThreadExtendedInfo>() / std::mem::size_of::<u32>()) as u32;

    // SAFETY: thread_info writes into the provided buffer up to count u32 words.
    let kr = unsafe {
        thread_info(
            thread,
            THREAD_EXTENDED_INFO,
            std::ptr::from_mut(&mut info).cast::<i32>(),
            &raw mut count,
        )
    };
    mach_result("thread_info", kr)?;

    Ok(extract_thread_name(&info.pth_name))
}
