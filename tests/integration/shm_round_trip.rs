//! Integration tests: write data to `SharedMemory` via raw pointers, read back through public API.

use std::mem::{offset_of, size_of};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crash_monitor::shm::*;
use nix::libc;

/// Unique PID per test to avoid shm name collisions.
fn unique_pid() -> u32 {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    std::process::id()
        .wrapping_mul(1_000_003)
        .wrapping_add(COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// Helper: write a C-style NUL-terminated string into a fixed-size byte array via raw pointer.
unsafe fn write_c_str(dst: *mut u8, s: &str, max_len: usize) {
    let bytes = s.as_bytes();
    let len = bytes.len().min(max_len - 1);
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, len);
        *dst.add(len) = 0;
    }
}

/// Helper: write a value of type T at the given byte pointer offset.
/// Uses `copy_nonoverlapping` to avoid `cast_ptr_alignment` lint on `*mut u8` → `*mut T`.
unsafe fn write_val<T: Copy>(base: *mut u8, offset: usize, val: T) {
    unsafe {
        let src = (&raw const val).cast::<u8>();
        std::ptr::copy_nonoverlapping(src, base.add(offset), size_of::<T>());
    }
}

unsafe fn store_u64_release(base: *mut u8, offset: usize, val: u64) {
    #[allow(clippy::cast_ptr_alignment)]
    let value = unsafe { &*base.add(offset).cast::<AtomicU64>() };
    value.store(val, Ordering::Release);
}

struct ForkChild {
    pid: libc::pid_t,
}

impl ForkChild {
    fn try_wait(&mut self) -> Option<libc::c_int> {
        let mut status = 0;
        let result = unsafe { libc::waitpid(self.pid, &raw mut status, libc::WNOHANG) };
        assert!(
            result >= 0,
            "waitpid failed: {}",
            std::io::Error::last_os_error()
        );
        if result == self.pid {
            self.pid = 0;
            Some(status)
        } else {
            None
        }
    }

    fn wait_until(&mut self, deadline: Instant) -> libc::c_int {
        loop {
            if let Some(status) = self.try_wait() {
                return status;
            }
            if Instant::now() >= deadline {
                let mut status = 0;
                unsafe {
                    libc::kill(self.pid, libc::SIGKILL);
                    libc::waitpid(self.pid, &raw mut status, 0);
                }
                self.pid = 0;
                panic!("forked SHM producer exceeded its deadline");
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

impl Drop for ForkChild {
    fn drop(&mut self) {
        if self.pid > 0 {
            let mut status = 0;
            unsafe {
                libc::kill(self.pid, libc::SIGKILL);
                libc::waitpid(self.pid, &raw mut status, 0);
            }
        }
    }
}

const FORK_STRESS_MAX_ITERATIONS: u32 = 100_000_000;

#[derive(Clone, Copy)]
struct ContextPublicationOffsets {
    generation: usize,
    heartbeat: usize,
    session_start: usize,
    build_number: usize,
}

unsafe fn run_forked_context_producer(base: *mut u8, offsets: ContextPublicationOffsets) -> ! {
    // The child never runs Rust destructors and has a hard process-level
    // deadline, so a failed parent assertion cannot leave it spinning.
    unsafe {
        libc::alarm(8);
        #[allow(clippy::cast_ptr_alignment)]
        let generation = &*base.add(offsets.generation).cast::<AtomicU32>();
        #[allow(clippy::cast_ptr_alignment)]
        let heartbeat = &*base.add(offsets.heartbeat).cast::<AtomicU64>();

        if generation
            .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            libc::_exit(4);
        }
        write_val::<u64>(base, offsets.session_start, 1);
        heartbeat.store(1, Ordering::Release);

        // The parent has its own three-second wait deadline and this process
        // has the alarm above. An iteration cap would turn snapshot speed and
        // scheduler behavior into an unrelated source of test failures.
        while heartbeat.load(Ordering::Acquire) != 2 {
            libc::sched_yield();
        }

        write_val::<u32>(base, offsets.build_number, 1);
        generation.store(2, Ordering::Release);
        heartbeat.store(4, Ordering::Release);
        let mut stopped = false;
        for sequence in 2..=FORK_STRESS_MAX_ITERATIONS {
            if heartbeat.load(Ordering::Acquire) == 3 {
                stopped = true;
                break;
            }
            let odd_generation = sequence * 2 - 1;
            if generation
                .compare_exchange(
                    odd_generation - 1,
                    odd_generation,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
            {
                libc::_exit(5);
            }
            write_val::<u64>(base, offsets.session_start, u64::from(sequence));
            for _ in 0..256 {
                std::hint::spin_loop();
            }
            write_val::<u32>(base, offsets.build_number, sequence);
            generation.store(odd_generation + 1, Ordering::Release);
        }
        if !stopped && heartbeat.load(Ordering::Acquire) != 3 {
            libc::_exit(6);
        }

        libc::alarm(0);
        libc::_exit(0);
    }
}

fn wait_for_live_heartbeat(
    shm: &SharedMemory,
    child: &mut ForkChild,
    expected: u64,
    deadline: Instant,
) {
    while shm.read_live_heartbeat() != expected {
        if let Some(status) = child.try_wait() {
            let ending = if libc::WIFEXITED(status) {
                format!("exited with code {}", libc::WEXITSTATUS(status))
            } else if libc::WIFSIGNALED(status) {
                format!("was signaled with {}", libc::WTERMSIG(status))
            } else {
                format!("ended with status {status}")
            };
            panic!("forked producer {ending} before heartbeat {expected}");
        }
        assert!(
            Instant::now() < deadline,
            "forked producer did not publish readiness"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn test_breadcrumbs_write_read() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    let base = shm.base_ptr();

    unsafe {
        // SutCrumbState is at SECTION2_OFFSET
        let crumb_state = base.add(SECTION2_OFFSET);

        // Set ring_count = 1 (at end of SutCrumbState: offset = rings_size)
        let ring_count_offset = size_of::<SutCrumbRing>() * CRUMB_MAX_THREADS;
        write_val::<u32>(crumb_state, ring_count_offset, 1);

        // Ring 0 is at crumb_state + 0
        let ring0 = crumb_state;

        // Write a SutBreadcrumb at ring0.buf[0]
        // SutBreadcrumb layout (64 bytes):
        //   timestamp_ns: u64 (0)
        //   thread_id: u32 (8)
        //   category: u16 (12)
        //   severity: u16 (14)
        //   file: [u8; 16] (16)
        //   line: u16 (32)
        //   pad: u16 (34)
        //   message: [u8; 28] (36)
        let entry = ring0; // buf starts at offset 0 of ring
        write_val::<u64>(entry, 0, 123_456_789); // timestamp_ns
        write_val::<u32>(entry, 8, 42); // thread_id
        write_val::<u16>(entry, 12, 3); // category
        write_val::<u16>(entry, 14, 2); // severity
        write_c_str(entry.add(16), "test.c", 16); // file
        write_val::<u16>(entry, 32, 99); // line
        write_c_str(entry.add(36), "hello crash", 28); // message

        // Set ring0.write_idx = 1, ring0.count = 1
        // write_idx is at offset: buf_size = 512 * 64 = 32768
        let buf_size = CRUMB_RING_CAPACITY * size_of::<SutBreadcrumb>();
        write_val::<u32>(ring0, buf_size, 1); // write_idx
        write_val::<u32>(ring0, buf_size + 4, 1); // count
    }

    let crumbs = shm
        .snapshot_owned_until(None)
        .expect("snapshot")
        .read_breadcrumbs();
    assert_eq!(crumbs.len(), 1);
    assert_eq!(crumbs[0].timestamp_ns, 123_456_789);
    assert_eq!(crumbs[0].thread_id, 42);
    assert_eq!(crumbs[0].category, 3);
    assert_eq!(crumbs[0].severity, 2);
    assert_eq!(crumbs[0].file, "test.c");
    assert_eq!(crumbs[0].line, 99);
    assert_eq!(crumbs[0].message, "hello crash");
}

#[test]
fn test_breadcrumbs_multi_ring_merge_sort() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    let base = shm.base_ptr();

    let ring_size = size_of::<SutCrumbRing>();
    let buf_size = CRUMB_RING_CAPACITY * size_of::<SutBreadcrumb>();

    unsafe {
        let crumb_state = base.add(SECTION2_OFFSET);

        // Set ring_count = 2
        let ring_count_offset = ring_size * CRUMB_MAX_THREADS;
        write_val::<u32>(crumb_state, ring_count_offset, 2);

        // Ring 0: one entry with timestamp 200
        let ring0 = crumb_state;
        let entry0 = ring0;
        write_val::<u64>(entry0, 0, 200); // timestamp_ns
        write_val::<u32>(entry0, 8, 1); // thread_id
        write_c_str(entry0.add(16), "a.c", 16);
        write_c_str(entry0.add(36), "second", 28);
        write_val::<u32>(ring0, buf_size, 1); // write_idx
        write_val::<u32>(ring0, buf_size + 4, 1); // count

        // Ring 1: one entry with timestamp 100
        let ring1 = crumb_state.add(ring_size);
        let entry1 = ring1;
        write_val::<u64>(entry1, 0, 100); // timestamp_ns
        write_val::<u32>(entry1, 8, 2); // thread_id
        write_c_str(entry1.add(16), "b.c", 16);
        write_c_str(entry1.add(36), "first", 28);
        write_val::<u32>(ring1, buf_size, 1); // write_idx
        write_val::<u32>(ring1, buf_size + 4, 1); // count
    }

    let crumbs = shm
        .snapshot_owned_until(None)
        .expect("snapshot")
        .read_breadcrumbs();
    assert_eq!(crumbs.len(), 2);
    // Sorted by timestamp: 100 first, 200 second
    assert_eq!(crumbs[0].timestamp_ns, 100);
    assert_eq!(crumbs[0].message, "first");
    assert_eq!(crumbs[0].thread_id, 2);
    assert_eq!(crumbs[1].timestamp_ns, 200);
    assert_eq!(crumbs[1].message, "second");
    assert_eq!(crumbs[1].thread_id, 1);
}

#[test]
fn test_context_write_read() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    let base = shm.base_ptr();

    unsafe {
        let ctx = base.add(CONTEXT_OFFSET);

        // annotation_count = 1
        write_val::<i32>(ctx, offset_of!(SutCrashContext, annotation_count), 1);

        // annotations[0] = { key: "active_tool", value: "brush" }; key[32], value[64]
        let ann0 = ctx.add(offset_of!(SutCrashContext, annotations));
        write_c_str(ann0, "active_tool", 32);
        write_c_str(ann0.add(32), "brush", 64);
    }

    let ctx = shm
        .snapshot_owned_until(None)
        .expect("snapshot")
        .read_context()
        .expect("read_context should succeed");
    assert_eq!(ctx.annotations.len(), 1);
    assert_eq!(
        ctx.annotations[0],
        ("active_tool".to_string(), "brush".to_string())
    );
}

#[test]
fn test_heartbeat_round_trip() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    assert_eq!(shm.read_live_heartbeat(), 0);

    unsafe {
        write_val::<u64>(
            shm.base_ptr(),
            CONTEXT_OFFSET + offset_of!(SutCrashContext, heartbeat_counter),
            999,
        );
    }

    assert_eq!(shm.read_live_heartbeat(), 999);
}

#[test]
fn test_settings_round_trip() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    let base = shm.base_ptr();

    unsafe {
        let settings = base.add(SETTINGS_OFFSET);

        write_val::<u32>(settings, 4, 1);
        write_c_str(settings.add(8), "mode", 16);
        write_c_str(settings.add(24), "round-trip", 32);
    }

    let s = shm
        .snapshot_owned_until(None)
        .expect("snapshot")
        .read_settings()
        .expect("read_settings should succeed");
    assert_eq!(s.schema_version, 1);
    assert_eq!(s.values, [("mode".into(), "round-trip".into())]);
}

#[test]
fn test_forked_context_producer_never_exposes_torn_fields() {
    let shm = SharedMemory::create(unique_pid()).expect("shm create");
    let base = shm.base_ptr();
    let offsets = ContextPublicationOffsets {
        generation: SECTION1_OFFSET + offset_of!(ShmHeader, context_generation),
        heartbeat: CONTEXT_OFFSET + offset_of!(SutCrashContext, heartbeat_counter),
        session_start: CONTEXT_OFFSET + offset_of!(SutCrashContext, session_start_ns),
        build_number: CONTEXT_OFFSET + offset_of!(SutCrashContext, build_number),
    };

    let pid = unsafe { libc::fork() };
    assert!(pid >= 0, "fork failed: {}", std::io::Error::last_os_error());
    if pid == 0 {
        unsafe { run_forked_context_producer(base, offsets) };
    }

    let mut child = ForkChild { pid };
    wait_for_live_heartbeat(&shm, &mut child, 1, Instant::now() + Duration::from_secs(3));

    let odd_snapshot = shm
        .snapshot_owned_until(Some(Instant::now() + Duration::from_secs(3)))
        .expect("odd snapshot must return without retrying");
    assert!(odd_snapshot.read_context().is_none());
    assert!(matches!(
        odd_snapshot.consistency_issues(),
        [ShmConsistencyIssue::Context {
            generation_before: 1,
            generation_after: 1,
        }]
    ));

    unsafe {
        store_u64_release(base, offsets.heartbeat, 2);
    }
    wait_for_live_heartbeat(&shm, &mut child, 4, Instant::now() + Duration::from_secs(3));

    let mut saw_changed_generation = false;
    for _ in 0..12 {
        let snapshot = shm
            .snapshot_owned_until(Some(Instant::now() + Duration::from_secs(3)))
            .expect("bounded concurrent snapshot");
        saw_changed_generation |= snapshot.consistency_issues().iter().any(|issue| {
            matches!(
                issue,
                ShmConsistencyIssue::Context {
                    generation_before,
                    generation_after,
                } if generation_before != generation_after
            )
        });
        if let Some(context) = snapshot.read_context() {
            assert_eq!(context.session_start_ns, u64::from(context.build_number));
        } else {
            assert!(
                snapshot
                    .consistency_issues()
                    .iter()
                    .any(|issue| matches!(issue, ShmConsistencyIssue::Context { .. }))
            );
        }
    }

    unsafe {
        store_u64_release(base, offsets.heartbeat, 3);
    }

    let status = child.wait_until(Instant::now() + Duration::from_secs(8));
    assert!(libc::WIFEXITED(status));
    assert_eq!(libc::WEXITSTATUS(status), 0);
    assert!(
        saw_changed_generation,
        "stress loop never exercised a before/after generation mismatch"
    );

    let final_context = shm
        .snapshot_owned_until(Some(Instant::now() + Duration::from_secs(3)))
        .expect("final stable snapshot")
        .read_context()
        .expect("final context must be published");
    assert_eq!(
        final_context.session_start_ns,
        u64::from(final_context.build_number)
    );
    assert!(final_context.build_number >= 1);
}
