//! FFI functions for Mach VM memory operations (excluded from coverage).

use mach2::port::mach_port_t;
use mach2::task::task_info;
use mach2::task_info::task_info_t;
use mach2::task_info::{TASK_VM_INFO, task_vm_info};
use mach2::vm::{mach_vm_deallocate, mach_vm_read, mach_vm_region};
use mach2::vm_page_size::vm_page_size;
use mach2::vm_region::{VM_REGION_EXTENDED_INFO, vm_region_extended_info};

use crate::platform::macos::memory::{
    VmEnumAction, VmEnumerationState, VmRegionEnumerationQuality,
};
use crate::platform::macos::types::{MachError, TaskVmSummary, VmRegionInfo, mach_result};

use super::types::self_task;

/// Read bytes from a remote task's address space.
///
/// # Errors
/// Returns `MachError` if the `mach_vm_read` kernel call fails.
pub fn vm_read(task: mach_port_t, address: u64, size: usize) -> Result<Vec<u8>, MachError> {
    let mut data_ptr: usize = 0;
    let mut data_cnt: u32 = 0;

    // SAFETY: mach_vm_read reads from the target task; we deallocate the buffer after copying.
    let kr = unsafe {
        mach_vm_read(
            task,
            address,
            size as u64,
            &raw mut data_ptr,
            &raw mut data_cnt,
        )
    };
    mach_result("mach_vm_read", kr)?;

    if data_ptr == 0 || data_cnt == 0 {
        return Ok(Vec::new());
    }

    let result =
        unsafe { std::slice::from_raw_parts(data_ptr as *const u8, data_cnt as usize) }.to_vec();

    unsafe {
        mach_vm_deallocate(self_task(), data_ptr as u64, u64::from(data_cnt));
    }

    Ok(result)
}

/// Query VM region info at or after `address`. Returns the region info and updates `address`
/// to the actual start of the found region.
///
/// # Errors
/// Returns `MachError` if the `mach_vm_region` kernel call fails.
pub fn vm_region_query(task: mach_port_t, address: &mut u64) -> Result<VmRegionInfo, MachError> {
    let mut size: u64 = 0;
    let mut info: vm_region_extended_info = unsafe { std::mem::zeroed() };
    let mut info_count = vm_region_extended_info::count();
    let mut object_name: mach_port_t = 0;

    // SAFETY: mach_vm_region reads region info; address is updated to the region start.
    let kr = unsafe {
        mach_vm_region(
            task,
            &raw mut *address,
            &raw mut size,
            VM_REGION_EXTENDED_INFO,
            std::ptr::from_mut(&mut info).cast::<i32>(),
            &raw mut info_count,
            &raw mut object_name,
        )
    };
    mach_result("mach_vm_region", kr)?;

    Ok(VmRegionInfo {
        address: *address,
        size,
        protection: info.protection,
        user_tag: info.user_tag,
        share_mode: info.share_mode,
        pages_resident: info.pages_resident,
        pages_swapped_out: info.pages_swapped_out,
    })
}

/// Enumerate all VM regions in the target task's address space.
/// Returns the regions and their completeness quality.
/// Individual region query failures are skipped (logged to stderr).
///
/// # Errors
/// Returns an error if the host VM page size is unavailable or invalid.
pub fn enumerate_vm_regions(
    task: mach_port_t,
    mut checkpoint: impl FnMut() -> Result<(), String>,
) -> Result<(Vec<VmRegionInfo>, VmRegionEnumerationQuality), String> {
    let mut regions = Vec::new();
    let mut state = VmEnumerationState::new(host_page_size()?);

    loop {
        if checkpoint().is_err() {
            return Ok((regions, VmRegionEnumerationQuality::CaptureDeadline));
        }
        if let Some(quality) = state.pre_query_stop() {
            eprintln!("[monitor] VM region enumeration partial: {quality}");
            return Ok((regions, quality));
        }

        let address = state.address();
        let mut query_addr = address;
        let (query_result, maybe_info) = match vm_region_query(task, &mut query_addr) {
            Ok(info) => (Ok((info.size, query_addr)), Some(info)),
            Err(error) => (Err(error.kern_return), None),
        };

        match state.process_query(query_result) {
            VmEnumAction::AddRegion => {
                if let Some(info) = maybe_info {
                    regions.push(info);
                }
                if checkpoint().is_err() {
                    return Ok((regions, VmRegionEnumerationQuality::CaptureDeadline));
                }
            }
            VmEnumAction::AddTerminalRegion(quality) => {
                if let Some(info) = maybe_info {
                    regions.push(info);
                }
                eprintln!("[monitor] VM region enumeration partial: {quality}");
                return Ok((regions, quality));
            }
            VmEnumAction::Done(quality) => {
                if !quality.is_complete() {
                    eprintln!("[monitor] VM region enumeration partial: {quality}");
                }
                return Ok((regions, quality));
            }
            VmEnumAction::SkipPage => {
                eprintln!("[monitor] vm_region skip at {address:#x}");
                if checkpoint().is_err() {
                    return Ok((regions, VmRegionEnumerationQuality::CaptureDeadline));
                }
            }
            VmEnumAction::Stop(quality) => {
                eprintln!("[monitor] VM region enumeration partial: {quality}");
                return Ok((regions, quality));
            }
        }
    }
}

fn host_page_size() -> Result<u64, String> {
    // SAFETY: libSystem initializes this process-wide Mach VM constant before
    // application code runs. Reading it does not mutate kernel or process state.
    let raw_page_size = unsafe { vm_page_size };
    let page_size = u64::try_from(raw_page_size)
        .map_err(|_| "host VM page size does not fit in u64".to_string())?;
    if page_size == 0 {
        Err("host VM page size is zero".to_string())
    } else {
        Ok(page_size)
    }
}

/// Read `task_info` with a given flavor, writing into the provided buffer.
///
/// # Errors
/// Returns `MachError` if the `task_info` kernel call fails.
pub fn get_task_info<T>(task: mach_port_t, flavor: u32, info: &mut T) -> Result<(), MachError> {
    let mut count = (std::mem::size_of::<T>() / std::mem::size_of::<u32>()) as u32;

    // SAFETY: task_info writes into the provided buffer up to count u32 words.
    let kr = unsafe {
        task_info(
            task,
            flavor,
            std::ptr::from_mut::<T>(info) as task_info_t,
            &raw mut count,
        )
    };
    mach_result("task_info", kr)
}

/// Get high-level VM statistics for a task.
///
/// # Errors
/// Returns `MachError` if the `task_info` kernel call fails.
pub fn get_task_vm_info(task: mach_port_t) -> Result<TaskVmSummary, MachError> {
    let mut info: task_vm_info = unsafe { std::mem::zeroed() };
    get_task_info(task, TASK_VM_INFO, &mut info)?;
    Ok(TaskVmSummary {
        virtual_size: info.virtual_size,
        resident_size: info.resident_size,
        phys_footprint: info.phys_footprint,
        internal: info.internal,
        compressed: info.compressed,
    })
}
