use super::*;
use crate::pipeline::PluginContext;
use crate::platform::VmRegionInfo;
use crate::platform::mock::MockPlatform;

fn make_region(address: u64, size: u64, user_tag: u32, pages_resident: u32) -> VmRegionInfo {
    VmRegionInfo {
        address,
        size,
        protection: 0,
        user_tag,
        share_mode: 0,
        pages_resident,
        pages_swapped_out: 0,
    }
}

#[test]
fn test_plugin_dependency_metadata() {
    let collector = MemoryCollector::new(std::sync::Arc::new(MockPlatform::default()));
    assert!(collector.hard_dependencies().is_empty());
    assert!(collector.order_after().is_empty());
}

#[test]
fn test_collect_memory_map() {
    let mut plat = MockPlatform::default();
    plat.regions = vec![
        make_region(0x1000, 0x2000, 1, 10),
        make_region(0x5000, 0x1000, 30, 5),
    ];

    let (regions, quality) =
        collect_memory_map(&plat, 0, &PluginContext::without_deadline()).unwrap();
    assert_eq!(quality, VmRegionEnumerationQuality::Complete);
    assert_eq!(regions.len(), 2);
    assert_eq!(regions[0].address, 0x1000);
    assert_eq!(regions[1].address, 0x5000);
}

#[test]
fn test_memory_collector_preserves_partial_map_and_returns_quality_diagnostic() {
    let mut platform = MockPlatform::default();
    platform.regions = vec![make_region(0x1000, 0x2000, 1, 10)];
    platform.vm_region_quality = VmRegionEnumerationQuality::QueryAttemptLimit;
    let platform = std::sync::Arc::new(platform);
    let collector = MemoryCollector::new(platform);
    let event = CrashEvent {
        report_id: Default::default(),
        report_type: crate::pipeline::ReportType::Crash,
        termination: None,
        exception_type: Some(1),
        exception_code: None,
        exception_subcode: None,
        exception_codes: Vec::new(),
        crashed_thread: None,
        bail_on_suspend_failure: false,
        pid: 123,
        process_name: "partial-memory-test".to_string(),
        hang_duration_ms: None,
    };
    let mut data = CollectedData::default();

    let error = collector
        .collect(&event, 0, &mut data, &PluginContext::without_deadline())
        .unwrap_err();

    assert_eq!(data.raw.memory_map.len(), 1);
    assert!(error.contains("partial data"));
    assert!(error.contains("query attempt limit"));
}

#[test]
fn test_partial_memory_quality_is_exposed_in_pipeline_diagnostics() {
    let mut mock = MockPlatform::default();
    mock.regions = vec![make_region(0x1000, 0x2000, 1, 10)];
    mock.vm_region_quality = VmRegionEnumerationQuality::ConsecutiveErrorLimit;
    let platform = std::sync::Arc::new(mock);
    let tempdir = tempfile::tempdir().unwrap();
    let pipeline = crate::pipeline::Pipeline {
        enabled: true,
        triggers: crate::pipeline::TriggerPolicy::ALL_ENABLED,
        filters: vec![],
        collectors: vec![Box::new(MemoryCollector::new(platform.clone()))],
        pre_processors: vec![],
        post_processors: vec![],
        notifiers: vec![],
        shm: None,
        platform,
        output_dir: Some(tempdir.path().to_path_buf()),
    };
    let event = CrashEvent {
        report_id: Default::default(),
        report_type: crate::pipeline::ReportType::Crash,
        termination: None,
        exception_type: Some(1),
        exception_code: None,
        exception_subcode: None,
        exception_codes: Vec::new(),
        crashed_thread: None,
        bail_on_suspend_failure: false,
        pid: 124,
        process_name: "partial-memory-diagnostics-test".to_string(),
        hang_duration_ms: None,
    };

    let diagnostics = pipeline.handle_event(&event, 0);
    let status = diagnostics
        .plugins
        .iter()
        .find(|plugin| plugin.name == "MemoryCollector")
        .map(|plugin| &plugin.status);

    assert!(matches!(
        status,
        Some(crate::pipeline::PluginStatus::Error(error))
            if error.contains("partial data") && error.contains("consecutive query error limit")
    ));
    assert!(diagnostics.report_path.is_some());
}

#[test]
fn test_collect_memory_map_propagates_cancellation() {
    let plat = MockPlatform::default();
    let context = PluginContext::without_deadline();
    context.cancellation_token().cancel();

    match collect_memory_map(&plat, 0, &context) {
        Err(error) => assert_eq!(error, "plugin deadline reached"),
        Ok(_) => panic!("cancelled context should stop memory-map collection"),
    }
}

#[test]
fn test_collect_heap_summary_malloc_tags() {
    let regions = vec![
        make_region(0x1000, 0x4000, 1, 10), // malloc (tag 1)
        make_region(0x5000, 0x2000, 7, 5),  // malloc_tiny (tag 7)
        make_region(0x8000, 0x1000, 30, 3), // Stack (tag 30) — not malloc
    ];

    let plat = MockPlatform::default();
    let heap =
        collect_heap_summary(&plat, 0, &regions, &PluginContext::without_deadline()).unwrap();

    assert_eq!(heap.malloc_zones.len(), 2);

    // BTreeMap ordering: tag 1 before tag 7
    assert_eq!(heap.malloc_zones[0].name, "malloc");
    assert_eq!(heap.malloc_zones[0].region_count, 1);
    assert_eq!(heap.malloc_zones[0].resident_pages, 10);

    assert_eq!(heap.malloc_zones[1].name, "malloc_tiny");
    assert_eq!(heap.malloc_zones[1].region_count, 1);
    assert_eq!(heap.malloc_zones[1].resident_pages, 5);
}

#[test]
fn test_collect_heap_summary_no_malloc() {
    let regions = vec![make_region(0x8000, 0x1000, 30, 3)]; // Stack only

    let plat = MockPlatform::default();
    let heap =
        collect_heap_summary(&plat, 0, &regions, &PluginContext::without_deadline()).unwrap();

    assert!(heap.malloc_zones.is_empty());
}
