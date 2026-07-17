//! Generate the Rust mirror of the C<->Rust shared-memory layout from the
//! schema header (single source of truth), so the two never drift.

use std::path::PathBuf;

fn main() {
    let manifest = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    // Schema lives inside this crate/submodule (`schema/crash_shm.h`), so the
    // path is stable whether built in the monorepo or as a standalone repo.
    let header = PathBuf::from(&manifest).join("schema/crash_shm.h");
    let header = header.to_str().expect("header path is valid UTF-8");

    println!("cargo:rerun-if-changed={header}");

    // A host project may bake its data-dir namespace via this env at build time
    // (consumed by `option_env!` in utils/paths.rs). Rebuild when it changes so
    // the baked default never goes stale. Unset → generic `.crash_monitor`.
    println!("cargo:rerun-if-env-changed=CRASH_MONITOR_DATA_DIR_NAME");

    let bindings = bindgen::Builder::default()
        .header(header)
        // Only our shm types/constants — never drag in system headers.
        .allowlist_type("sut_.*")
        .allowlist_var("SUT_.*")
        // The breadcrumb category enum is unused in Rust (category is read as
        // u16); blocklisting it drops bindgen's redundant typedef alias.
        .blocklist_type("sut_crumb_category.*")
        // No Default derive; the shm structs are read via ptr::read_volatile.
        .derive_default(false)
        .generate()
        .expect("bindgen failed to generate crash_shm bindings");

    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    bindings
        .write_to_file(out.join("crash_shm_bindings.rs"))
        .expect("failed to write crash_shm bindings");
}
