use std::path::PathBuf;
use std::process::Command;

const PANIC_STRATEGY_ERROR: &str =
    "crash_monitor requires panic=unwind because cooperative plugin isolation uses catch_unwind";

#[test]
fn crate_roots_reject_abort_strategy_at_compile_time() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let tempdir = tempfile::tempdir().unwrap();

    for root in ["src/lib.rs", "src/main.rs"] {
        let output = Command::new("rustc")
            .current_dir(&manifest_dir)
            .args([
                "--crate-type=lib",
                "--edition=2024",
                "--emit=metadata",
                "-C",
                "panic=abort",
            ])
            .arg(root)
            .arg("-o")
            .arg(tempdir.path().join(root.replace('/', "-") + ".rmeta"))
            .output()
            .unwrap();

        assert!(!output.status.success(), "{root} accepted panic=abort");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(PANIC_STRATEGY_ERROR),
            "{root} did not emit the panic strategy compile error:\n{stderr}"
        );
    }
}
