//! Integration test: `stack` subcommand on a sample report file.

use base64::Engine;
use std::io::Write;
use tempfile::NamedTempFile;

#[test]
fn test_stack_valid_report() {
    let stack_data = vec![0x48u8, 0x65, 0x6c, 0x6c, 0x6f]; // "Hello"
    let encoded = base64::engine::general_purpose::STANDARD.encode(&stack_data);

    let json = serde_json::json!({
        "header": {
            "version": 1,
            "timestamp": "2026-04-05T12:00:00+09:00",
            "pid": 1234,
            "process": "test",
            "collector": "mbb_monitor",
            "type": "crash"
        },
        "threads": [{
            "index": 0,
            "id": 100,
            "name": "main",
            "crashed": true,
            "registers": {},
            "backtrace": [],
            "stack_memory": {
                "sp": "0x16d4fe000",
                "size": 5,
                "hex_dump": encoded
            }
        }],
        "loaded_images": [],
        "memory_map": [],
        "attachments": []
    });

    let mut f = NamedTempFile::new().unwrap();
    f.write_all(json.to_string().as_bytes()).unwrap();
    f.flush().unwrap();

    let exit_code = mbb_monitor::cli::stack::run(f.path().to_str().unwrap(), 0);
    assert_eq!(exit_code, 0);
}

#[test]
fn test_stack_thread_out_of_range() {
    let json = serde_json::json!({
        "header": {
            "version": 1,
            "timestamp": "2026-04-05T12:00:00+09:00",
            "pid": 1234,
            "process": "test",
            "collector": "mbb_monitor",
            "type": "crash"
        },
        "threads": [{
            "index": 0,
            "id": 100,
            "name": "main",
            "crashed": false,
            "registers": {},
            "backtrace": []
        }],
        "loaded_images": [],
        "memory_map": [],
        "attachments": []
    });

    let mut f = NamedTempFile::new().unwrap();
    f.write_all(json.to_string().as_bytes()).unwrap();
    f.flush().unwrap();

    let exit_code = mbb_monitor::cli::stack::run(f.path().to_str().unwrap(), 5);
    assert_eq!(exit_code, 1);
}

#[test]
fn test_stack_no_memory() {
    let json = serde_json::json!({
        "header": {
            "version": 1,
            "timestamp": "2026-04-05T12:00:00+09:00",
            "pid": 1234,
            "process": "test",
            "collector": "mbb_monitor",
            "type": "crash"
        },
        "threads": [{
            "index": 0,
            "id": 100,
            "name": "main",
            "crashed": false,
            "registers": {},
            "backtrace": []
        }],
        "loaded_images": [],
        "memory_map": [],
        "attachments": []
    });

    let mut f = NamedTempFile::new().unwrap();
    f.write_all(json.to_string().as_bytes()).unwrap();
    f.flush().unwrap();

    // No stack memory — exit 0 (informational, not an error)
    let exit_code = mbb_monitor::cli::stack::run(f.path().to_str().unwrap(), 0);
    assert_eq!(exit_code, 0);
}
