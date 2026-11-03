//! Root `cargo build --release` runs `make build` (→ `bin/smartnet`).
//!
//! This is a convenience **hook**, not the authoritative build. Cargo re-runs it only when this
//! package's own inputs change (this `build.rs` and `src/`) or one of the Makefiles listed below;
//! editing a source file under `smartnet/` or `calibri-lite/` does **not** re-run
//! it, because those crates are not part of this package. Use `make build` (which always rebuilds
//! the workspaces incrementally) after changing their sources. Watching the workspace directories
//! here is deliberately avoided: `target/` lives inside them, so a directory watch would hash
//! gigabytes on every build.
//!
//! A future fix for automatic detection should watch only source subtrees (e.g. `smartnet/crates`),
//! never the workspace roots.

fn main() {
    if std::env::var("PROFILE").as_deref() != Ok("release") {
        return;
    }
    let root = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    println!("cargo:rerun-if-changed=Makefile");
    println!("cargo:rerun-if-changed=smartnet/Makefile");
    println!("cargo:rerun-if-changed=calibri-lite/Makefile");
    let status = std::process::Command::new("make")
        .arg("-C")
        .arg(&root)
        .args(["build", "PROFILE=release"])
        .status()
        .expect("failed to spawn make");
    if !status.success() {
        panic!("make build failed");
    }
}
