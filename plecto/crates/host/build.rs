//! Builds the `filter-hello` guest and componentizes it, so the host's tests can
//! load a real `plecto:filter` component (tdd-workflow Phase 0/1 fixture).
//!
//! Target policy (ADR 000010): no-WASI header-only filters build for
//! `wasm32-unknown-unknown` and are wrapped into a component with `wit-component`
//! (no WASI adapter needed — the component imports ONLY granted plecto
//! capabilities). The body / stream<u8> / wasi:http increment moves to
//! `wasm32-wasip2` once wasmtime 46 ships.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let crates = manifest.parent().unwrap(); // plecto/crates
    let guest = crates.join("filter-hello");
    let wit = crates.parent().unwrap().join("wit"); // plecto/wit
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());

    println!("cargo:rerun-if-changed={}", guest.join("src").display());
    println!(
        "cargo:rerun-if-changed={}",
        guest.join("Cargo.toml").display()
    );
    println!("cargo:rerun-if-changed={}", wit.display());

    // 1) Build the guest core module (no WASI imports on this target).
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());
    let target_dir = guest.join("target");
    let status = Command::new(&cargo)
        .current_dir(&guest)
        .env_remove("CARGO_TARGET_DIR")
        .args([
            "build",
            "--target",
            "wasm32-unknown-unknown",
            "--release",
            "--target-dir",
        ])
        .arg(&target_dir)
        .status()
        .expect("failed to spawn cargo to build filter-hello");
    assert!(
        status.success(),
        "building filter-hello (core module) failed"
    );

    let core = target_dir.join("wasm32-unknown-unknown/release/filter_hello.wasm");
    assert!(
        core.exists(),
        "guest core module not found at {}",
        core.display()
    );

    // 2) Wrap the core module into a Component (no adapter: imports only plecto).
    let core_bytes = std::fs::read(&core).expect("read guest core module");
    let component_bytes = wit_component::ComponentEncoder::default()
        .module(&core_bytes)
        .expect("ComponentEncoder::module")
        .validate(true)
        .encode()
        .expect("encode filter-hello component");

    let component = out_dir.join("filter_hello.component.wasm");
    std::fs::write(&component, &component_bytes).expect("write filter-hello component");
    println!(
        "cargo:rustc-env=FILTER_HELLO_COMPONENT={}",
        component.display()
    );
}
