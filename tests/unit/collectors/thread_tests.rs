use super::*;
use crate::pipeline::PluginContext;
use crate::platform::mock::{MockPlatform, MockThread};

/// Helper: create a 68-element u32 state vector with all zeros,
/// then set specific register values via (lo, hi) u32 pairs.
fn make_state(overrides: &[(usize, u32, u32)]) -> Vec<u32> {
    let mut state = vec![0u32; 68];
    for &(idx, lo, hi) in overrides {
        state[idx * 2] = lo;
        state[idx * 2 + 1] = hi;
    }
    state
}

#[test]
fn test_inspect_all_threads_success() {
    // Thread 100: fp=0 (index 29), pc=0xDEAD (index 32)
    // Thread 200: fp=0 (index 29), pc=0xBEEF (index 32)
    let mut plat = MockPlatform::default();
    plat.threads = vec![
        MockThread {
            port: 100,
            stable_id: 1_100,
            name: Some("main".into()),
            state: make_state(&[(32, 0xDEAD, 0)]),
        },
        MockThread {
            port: 200,
            stable_id: 1_200,
            name: None,
            state: make_state(&[(32, 0xBEEF, 0)]),
        },
    ];

    let result = inspect_all_threads(
        &plat,
        0,
        Some(100),
        false,
        &PluginContext::without_deadline(),
    );
    let result = result.threads;
    assert_eq!(result.len(), 2);

    assert_eq!(result[0].thread_port, 100);
    assert_eq!(result[0].thread_id, 1_100);
    assert!(result[0].crashed);
    assert!(result[0].registers.is_some());
    assert_eq!(result[0].name, Some("main".into()));

    assert_eq!(result[1].thread_port, 200);
    assert!(!result[1].crashed);
    assert!(result[1].registers.is_some());
    assert_eq!(result[1].name, None);
}

#[test]
fn test_stack_bytes_are_read_only_when_collection_policy_allows_them() {
    fn platform_with_stack(marker: u8) -> MockPlatform {
        let mut platform = MockPlatform::default();
        platform.threads = vec![MockThread {
            port: 100,
            stable_id: 1_100,
            name: Some("main".into()),
            state: make_state(&[(31, 0x1000, 0), (32, 0x2000, 0)]),
        }];
        platform
            .memory
            .insert(0x1000, vec![marker; MAX_STACK_BYTES]);
        platform
    }

    let minimal = platform_with_stack(0x5a);
    let threads = inspect_all_threads(
        &minimal,
        0,
        Some(100),
        false,
        &PluginContext::without_deadline(),
    );
    let threads = threads.threads;
    assert!(threads[0].stack_capture.is_none());

    let consented = platform_with_stack(0x5a);
    let threads = inspect_all_threads(
        &consented,
        0,
        Some(100),
        true,
        &PluginContext::without_deadline(),
    );
    let threads = threads.threads;
    let stack = threads[0]
        .stack_capture
        .as_ref()
        .expect("explicitly authorized stack bytes");
    assert_eq!(stack.bytes.len(), MAX_STACK_BYTES);
    assert!(stack.bytes.iter().all(|byte| *byte == 0x5a));
}

#[test]
fn test_thread_cap_preserves_crashed_thread_and_releases_surplus_ports() {
    let mut plat = MockPlatform::default();
    plat.threads = (0..MAX_CAPTURED_THREADS + 3)
        .map(|index| MockThread {
            port: u32::try_from(index + 1).unwrap(),
            stable_id: u64::try_from(index + 10_001).unwrap(),
            name: None,
            state: make_state(&[]),
        })
        .collect();
    let crashed_port = u32::try_from(MAX_CAPTURED_THREADS + 3).unwrap();

    let result = inspect_all_threads(
        &plat,
        0,
        Some(crashed_port),
        false,
        &PluginContext::without_deadline(),
    );

    assert_eq!(result.threads.len(), MAX_CAPTURED_THREADS);
    assert!(
        result
            .threads
            .iter()
            .any(|thread| thread.thread_port == crashed_port)
    );
    assert_eq!(plat.deallocated_ports().len(), 3);
    assert_eq!(result.budget_diagnostics.len(), 1);
}

#[test]
fn total_stack_budget_prioritizes_crashed_thread_and_truncates_deterministically() {
    let stack_count = MAX_TOTAL_STACK_BYTES / MAX_STACK_BYTES;
    let mut plat = MockPlatform::default();
    plat.threads = (0..=stack_count)
        .map(|index| MockThread {
            port: u32::try_from(index + 1).unwrap(),
            stable_id: u64::try_from(index + 1).unwrap(),
            name: None,
            state: make_state(&[(31, 0x1000, 0), (32, 0x2000, 0)]),
        })
        .collect();
    plat.memory.insert(0x1000, vec![0x5a; MAX_STACK_BYTES]);
    let crashed_port = u32::try_from(stack_count + 1).unwrap();

    let result = inspect_all_threads(
        &plat,
        0,
        Some(crashed_port),
        true,
        &PluginContext::without_deadline(),
    );

    assert_eq!(result.threads[0].thread_port, crashed_port);
    assert!(result.threads[0].stack_capture.is_some());
    assert_eq!(
        result
            .threads
            .iter()
            .filter_map(|thread| thread.stack_capture.as_ref())
            .map(|stack| stack.bytes.len())
            .sum::<usize>(),
        MAX_TOTAL_STACK_BYTES
    );
    assert_eq!(result.budget_diagnostics.len(), 1);
}

#[test]
fn test_get_registers_arm64() {
    // Set x0 = 0x1_0000_0042 (index 0: lo=0x42, hi=0x1)
    // fp (index 29): lo=0xF0, hi=0
    // sp (index 31): lo=0x00FF, hi=0x0001
    // pc (index 32): lo=0xABCD, hi=0x0002
    let mut plat = MockPlatform::default();
    plat.threads = vec![MockThread {
        port: 10,
        stable_id: 1_010,
        name: None,
        state: make_state(&[
            (0, 0x42, 0x1),
            (29, 0xF0, 0),
            (31, 0x00FF, 0x0001),
            (32, 0xABCD, 0x0002),
        ]),
    }];

    let regs = get_registers(&plat, 10, &PluginContext::without_deadline()).unwrap();
    assert_eq!(regs["x0"], 0x1_0000_0042);
    assert_eq!(regs["fp"], 0xF0);
    assert_eq!(regs["sp"], 0x0001_0000_00FF);
    assert_eq!(regs["pc"], 0x0002_0000_ABCD);
}

#[test]
fn truncated_register_state_is_rejected_before_fixed_index_access() {
    let mut plat = MockPlatform::default();
    plat.threads = vec![MockThread {
        port: 10,
        stable_id: 1_010,
        name: None,
        state: vec![0; 66],
    }];
    let error = get_registers(&plat, 10, &PluginContext::without_deadline()).unwrap_err();
    assert!(error.contains("expected 67..=68"));
}

#[test]
fn test_walk_backtrace_chain() {
    let mut plat = MockPlatform::default();

    // Frame at 0x1000: next_fp=0x2000 (u64 LE), lr=0xAAAA (u64 LE)
    let mut frame1 = Vec::new();
    frame1.extend_from_slice(&0x2000u64.to_le_bytes()); // next_fp
    frame1.extend_from_slice(&0xAAAAu64.to_le_bytes()); // lr
    plat.memory.insert(0x1000, frame1);

    // Frame at 0x2000: next_fp=0x0 (terminates), lr=0xBBBB
    let mut frame2 = Vec::new();
    frame2.extend_from_slice(&0x0u64.to_le_bytes()); // next_fp = 0 → stop after reading lr
    frame2.extend_from_slice(&0xBBBBu64.to_le_bytes()); // lr
    plat.memory.insert(0x2000, frame2);

    let bt = walk_backtrace(&plat, 0, 0x1000, 128, &PluginContext::without_deadline()).unwrap();
    assert_eq!(bt, vec![0xAAAA, 0xBBBB]);
}

#[test]
fn test_walk_backtrace_null_fp() {
    let plat = MockPlatform::default();
    let frames = walk_backtrace(&plat, 0, 0, 128, &PluginContext::without_deadline()).unwrap();
    assert!(frames.is_empty());
}

#[test]
fn test_walk_backtrace_checked_add_stops_on_address_overflow() {
    let plat = MockPlatform::default();
    let frames = walk_backtrace(
        &plat,
        0,
        u64::MAX - 7,
        128,
        &PluginContext::without_deadline(),
    )
    .unwrap();
    assert!(frames.is_empty());
}

#[test]
fn test_read_u64_aborts_on_partial_vm_read() {
    let mut platform = MockPlatform::default();
    platform.memory.insert(0x1000, vec![1, 2, 3, 4]);

    let error = read_u64(&platform, 0, 0x1000).unwrap_err();
    assert!(error.contains("partial VM read"));
    assert!(error.contains("requested 8 bytes, received 4"));
}

#[test]
fn test_compute_read_size_within_region() {
    let mut plat = MockPlatform::default();
    plat.regions = vec![crate::platform::VmRegionInfo {
        address: 0x1000,
        size: 0x10000,
        protection: 0,
        user_tag: 0,
        share_mode: 0,
        pages_resident: 0,
        pages_swapped_out: 0,
    }];

    let size = compute_read_size(&plat, 0, 0x1000);
    // region_top = 0x1000 + 0x10000 = 0x11000
    // available = 0x11000 - 0x1000 = 0x10000 = 65536
    // MAX_STACK_BYTES = 64 * 1024 = 65536
    // min(65536, 65536) = 65536
    assert_eq!(size, MAX_STACK_BYTES);

    // SP in the middle of the region
    let size2 = compute_read_size(&plat, 0, 0x5000);
    // available = 0x11000 - 0x5000 = 0xC000 = 49152
    // min(49152, 65536) = 49152
    assert_eq!(size2, 0xC000);
}
