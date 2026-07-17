use super::*;

#[test]
fn test_exception_type_name() {
    assert_eq!(exception_type_name(1), "EXC_BAD_ACCESS");
    assert_eq!(exception_type_name(10), "EXC_CRASH");
}

#[test]
fn test_exception_type_name_unknown() {
    assert_eq!(exception_type_name(99), "EXC_UNKNOWN");
}

#[test]
fn test_kern_return_name() {
    assert_eq!(kern_return_name(1), "KERN_INVALID_ADDRESS");
    assert_eq!(kern_return_name(99), "KERN_UNKNOWN");
}

#[test]
fn test_exception_to_signal() {
    assert_eq!(exception_to_signal(1), "SIGSEGV");
    assert_eq!(exception_to_signal(3), "SIGFPE");
    assert_eq!(exception_to_signal(10), "SIGABRT");
}

#[test]
fn test_mach_result() {
    // kr=0 (KERN_SUCCESS) → Ok(())
    assert!(mach_result("test_fn", 0).is_ok());

    // kr=5 → Err with kern_return=5
    let err = mach_result("test_fn", 5).unwrap_err();
    assert_eq!(err.kern_return, 5);
    assert_eq!(err.function, "test_fn");
}

#[test]
fn test_exception_type_name_all() {
    assert_eq!(exception_type_name(1), "EXC_BAD_ACCESS");
    assert_eq!(exception_type_name(2), "EXC_BAD_INSTRUCTION");
    assert_eq!(exception_type_name(3), "EXC_ARITHMETIC");
    assert_eq!(exception_type_name(4), "EXC_EMULATION");
    assert_eq!(exception_type_name(5), "EXC_SOFTWARE");
    assert_eq!(exception_type_name(6), "EXC_BREAKPOINT");
    assert_eq!(exception_type_name(10), "EXC_CRASH");
    assert_eq!(exception_type_name(11), "EXC_RESOURCE");
    assert_eq!(exception_type_name(12), "EXC_GUARD");
    assert_eq!(exception_type_name(0), "EXC_UNKNOWN");
    assert_eq!(exception_type_name(7), "EXC_UNKNOWN");
    assert_eq!(exception_type_name(255), "EXC_UNKNOWN");
}

#[test]
fn test_kern_return_name_all() {
    assert_eq!(kern_return_name(1), "KERN_INVALID_ADDRESS");
    assert_eq!(kern_return_name(2), "KERN_PROTECTION_FAILURE");
    assert_eq!(kern_return_name(0), "KERN_UNKNOWN");
    assert_eq!(kern_return_name(3), "KERN_UNKNOWN");
    assert_eq!(kern_return_name(999), "KERN_UNKNOWN");
}

#[test]
fn test_exception_to_signal_all() {
    assert_eq!(exception_to_signal(1), "SIGSEGV");
    assert_eq!(exception_to_signal(2), "SIGILL");
    assert_eq!(exception_to_signal(3), "SIGFPE");
    assert_eq!(exception_to_signal(6), "SIGTRAP");
    assert_eq!(exception_to_signal(10), "SIGABRT");
    assert_eq!(exception_to_signal(0), "SIGUNKNOWN");
    assert_eq!(exception_to_signal(4), "SIGUNKNOWN");
    assert_eq!(exception_to_signal(5), "SIGUNKNOWN");
    assert_eq!(exception_to_signal(99), "SIGUNKNOWN");
}

#[test]
fn test_mach_result_success() {
    assert!(mach_result("allocate_port", 0).is_ok());
}

#[test]
fn test_mach_result_failure() {
    let err = mach_result("allocate_port", 5).unwrap_err();
    assert_eq!(err.function, "allocate_port");
    assert_eq!(err.kern_return, 5);
}

#[test]
fn test_mach_error_display() {
    let err = MachError {
        function: "mach_port_allocate",
        kern_return: 42,
    };
    let msg = format!("{err}");
    assert_eq!(msg, "mach_port_allocate failed: kr=42");
}
