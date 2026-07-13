//! Builds the example `plecto:filter` guests and componentizes them, so the host's tests and the
//! server's examples can load real `plecto:filter` components (tdd-workflow Phase 0/1 fixtures).
//!
//! Target policy (ADR 000010): no-WASI header-only filters build for
//! `wasm32-unknown-unknown` and are wrapped into a component with `wit-component`
//! (no WASI adapter needed — the component imports ONLY granted plecto
//! capabilities). The body / stream<u8> / wasi:http increment moves to
//! `wasm32-wasip2` once wasmtime 46 ships.
//!
//! The fixture builds below run ONLY behind the `test-support` feature (this crate's own
//! `[dev-dependencies]` turn it on for `cargo test`/`--examples`/`cargo bench`; dependent crates do
//! the same in their own dev-dependencies). A plain production build of this crate — or of
//! `plecto-control`/`plecto-server`, which depend on it as a normal dependency — never needs the
//! `wasm32-unknown-unknown` target or the guest sources under `examples/filters/` / `bench/filters/`.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let crates = manifest.parent().unwrap(); // plecto/crates
    let plecto = crates.parent().unwrap(); // plecto
    let repo_root = plecto.parent().unwrap(); // repo root (holds bench/)
    // The vendored copy `bindgen!` actually resolves (crates/host/wit/) — not the canonical
    // `plecto/wit/` (kept in sync by `scripts/check_wit_vendoring.py`, run in CI).
    let wit = manifest.join("wit");
    // The example filter guests live OUTSIDE the workspace, under examples/filters/ (each its own
    // workspace), built here for wasm32 and componentized. Benchmark-only guests live under bench/.
    let filters = plecto.join("examples").join("filters");
    let bench_filters = repo_root.join("bench").join("filters");
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_string());

    println!("cargo:rerun-if-changed={}", wit.display());

    // Fixture guests (test_support's env!() consts): built ONLY when `test-support` is on, so a
    // plain production build of this crate (or of a dependent crate's default build) never touches
    // wasm32-unknown-unknown or these guest sources (some of which live outside `plecto/`).
    let test_support = std::env::var("CARGO_FEATURE_TEST_SUPPORT").is_ok();
    if test_support {
        // Each guest crate → a componentized `plecto:filter`, exposed to `test_support` via an env
        // var. filter-hello is the conformance fixture; filter-apikey is the real-world example
        // (auth gate).
        build_component(
            &cargo,
            &filters.join("filter-hello"),
            &out_dir,
            "filter_hello",
            "FILTER_HELLO_COMPONENT",
        );
        build_component(
            &cargo,
            &filters.join("filter-apikey"),
            &out_dir,
            "filter_apikey",
            "FILTER_APIKEY_COMPONENT",
        );
        // filter-quickstart is the minimal starter filter behind the `quickstart` example.
        build_component(
            &cargo,
            &filters.join("filter-quickstart"),
            &out_dir,
            "filter_quickstart",
            "FILTER_QUICKSTART_COMPONENT",
        );
        // filter-cors is the CORS reference filter (ADR 000073 / F2 shelf) — the living proof of
        // the 0.3.0 response-side contract (request context on on-response + preflight SC).
        build_component(
            &cargo,
            &filters.join("filter-cors"),
            &out_dir,
            "filter_cors",
            "FILTER_CORS_COMPONENT",
        );
        // filter-compat-v02 is pinned to the FROZEN 0.2.0 contract (wit/v0.2.0/) — the V02
        // adapter rail's living fixture (ADR 000073). Test-only, so it lives under
        // crates/host/fixtures/, not examples/.
        build_component(
            &cargo,
            &manifest.join("fixtures").join("filter-compat-v02"),
            &out_dir,
            "filter_compat_v02",
            "FILTER_COMPAT_V02_COMPONENT",
        );
        // filter-noop is the "pure WASM no-op" rung of the benchmark cost ladder (no host-API
        // calls). It is benchmark-only, so it lives under bench/filters/, not examples/.
        build_component(
            &cargo,
            &bench_filters.join("filter-noop"),
            &out_dir,
            "filter_noop",
            "FILTER_NOOP_COMPONENT",
        );
        // filter-resp: ADR 000073 response-context read + optional `replace` rungs (no host-API).
        build_component(
            &cargo,
            &bench_filters.join("filter-resp"),
            &out_dir,
            "filter_resp",
            "FILTER_RESP_COMPONENT",
        );
    }

    // The wasip2 fixture guests below are ALSO test fixtures (their bytes are only ever read by
    // `test_support`'s cfg-gated accessors and the feature-gated integration tests), so they too
    // build only under `test-support` — a production `--features capabilities` build must not
    // require wasm32-wasip2 or the guest sources. release.yml's binaries job compiles in a plain
    // rust:bookworm container with no wasm targets installed; gating on the capability feature
    // alone made that job the first place the gap could surface (E0463, v0.3.2 first attempt),
    // because ci.yml's release-parity environment does install the targets.

    // Experimental streaming body filter (feature `streaming-body`, OFF by default): build the
    // filter-streaming guest for wasm32-wasip2, which emits a Component directly (no wit-component
    // wrap). Only when the feature is on, so the default build never touches wasm32-wasip2.
    if test_support && std::env::var("CARGO_FEATURE_STREAMING_BODY").is_ok() {
        build_wasip2_component(
            &cargo,
            &filters.join("filter-streaming"),
            &out_dir,
            "filter_streaming",
            "FILTER_STREAMING_COMPONENT",
        );
    }

    // Outbound HTTP capability (ADR 000036, feature `outbound-http`, OFF by default): the
    // ext_authz-style guest imports wasi:http/outgoing-handler, so it builds for wasm32-wasip2.
    // filter-jwt (ADR 000070) is the same target: JWKS-at-init uses outbound; the static PEM/JWK
    // path simply never calls it.
    if test_support && std::env::var("CARGO_FEATURE_OUTBOUND_HTTP").is_ok() {
        build_wasip2_component(
            &cargo,
            &filters.join("filter-extauthz"),
            &out_dir,
            "filter_extauthz",
            "FILTER_EXTAUTHZ_COMPONENT",
        );
        build_wasip2_component(
            &cargo,
            &filters.join("filter-jwt"),
            &out_dir,
            "filter_jwt",
            "FILTER_JWT_COMPONENT",
        );
    }

    // Outbound TCP capability (ADR 000060, feature `outbound-tcp`, OFF by default): the TCP-gate
    // guest imports wasi:sockets, so it builds for wasm32-wasip2.
    if test_support && std::env::var("CARGO_FEATURE_OUTBOUND_TCP").is_ok() {
        build_wasip2_component(
            &cargo,
            &filters.join("filter-tcp-gate"),
            &out_dir,
            "filter_tcp_gate",
            "FILTER_TCP_GATE_COMPONENT",
        );
        // filter-ratelimit-redis (ADR 000061): the global-layer reference filter, also outbound-TCP
        // (wasi:sockets) and so also wasm32-wasip2.
        build_wasip2_component(
            &cargo,
            &filters.join("filter-ratelimit-redis"),
            &out_dir,
            "filter_ratelimit_redis",
            "FILTER_RATELIMIT_REDIS_COMPONENT",
        );
    }
}

/// Build one guest crate for `wasm32-wasip2`, which already emits a Component Model component (the
/// target links with `wasm-component-ld`), and emit `cargo:rustc-env=<env_var>=<path>`. Used for the
/// async `stream<u8>` streaming guest, which — unlike the no-WASI header-only filters — imports WASI.
fn build_wasip2_component(cargo: &str, guest: &Path, out_dir: &Path, stem: &str, env_var: &str) {
    println!("cargo:rerun-if-changed={}", guest.join("src").display());
    println!(
        "cargo:rerun-if-changed={}",
        guest.join("Cargo.toml").display()
    );
    // A lockfile-only guest dep bump must also retrigger the fixture build.
    println!(
        "cargo:rerun-if-changed={}",
        guest.join("Cargo.lock").display()
    );

    let target_dir = guest.join("target");
    let status = Command::new(cargo)
        .current_dir(guest)
        .env_remove("CARGO_TARGET_DIR")
        .args([
            "build",
            "--target",
            "wasm32-wasip2",
            "--release",
            "--target-dir",
        ])
        .arg(&target_dir)
        .status()
        .unwrap_or_else(|e| panic!("failed to spawn cargo to build {}: {e}", guest.display()));
    if !status.success() {
        panic!(
            "building {} for wasm32-wasip2 failed.\n\
             The wasip2 fixture guests (streaming-body / outbound-http / outbound-tcp test\n\
             fixtures) need the wasm32-wasip2 target:\n    \
             rustup target add wasm32-wasip2",
            guest.display()
        );
    }

    // wasm32-wasip2 emits a component directly — no wit-component wrapping. Copy the final
    // artifact into OUT_DIR (like `build_component`'s wrapped output) so the referenced path
    // lives in cargo-managed build output — `cargo clean` cleans it, and a concurrent build of
    // the guest workspace cannot rewrite the file the host build points at.
    let component = target_dir.join(format!("wasm32-wasip2/release/{stem}.wasm"));
    assert!(
        component.exists(),
        "guest component not found at {}",
        component.display()
    );
    let published = out_dir.join(format!("{stem}-wasip2.wasm"));
    std::fs::copy(&component, &published).unwrap_or_else(|e| {
        panic!(
            "copying {} to {} failed: {e}",
            component.display(),
            published.display()
        )
    });
    println!("cargo:rustc-env={env_var}={}", published.display());
}

/// Build one guest crate for `wasm32-unknown-unknown`, wrap the core module into a Component (no
/// WASI adapter — it imports only granted plecto capabilities), and emit
/// `cargo:rustc-env=<env_var>=<path>` so `test_support` can read the component bytes. `stem` is the
/// crate's wasm output name (the crate name with `-` → `_`).
fn build_component(cargo: &str, guest: &Path, out_dir: &Path, stem: &str, env_var: &str) {
    println!("cargo:rerun-if-changed={}", guest.join("src").display());
    println!(
        "cargo:rerun-if-changed={}",
        guest.join("Cargo.toml").display()
    );
    // A lockfile-only guest dep bump must also retrigger the fixture build.
    println!(
        "cargo:rerun-if-changed={}",
        guest.join("Cargo.lock").display()
    );

    // 1) Build the guest core module (no WASI imports on this target).
    let target_dir = guest.join("target");
    let status = Command::new(cargo)
        .current_dir(guest)
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
        .unwrap_or_else(|e| panic!("failed to spawn cargo to build {}: {e}", guest.display()));
    if !status.success() {
        panic!(
            "building {} for wasm32-unknown-unknown failed.\n\
             The usual cause is a missing WASM target. It is installed automatically by \
             plecto/rust-toolchain.toml; if you build outside that toolchain, run:\n    \
             rustup target add wasm32-unknown-unknown",
            guest.display()
        );
    }

    let core = target_dir.join(format!("wasm32-unknown-unknown/release/{stem}.wasm"));
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
        .unwrap_or_else(|e| panic!("encode {stem} component: {e}"));

    let component = out_dir.join(format!("{stem}.component.wasm"));
    std::fs::write(&component, &component_bytes).expect("write guest component");
    println!("cargo:rustc-env={env_var}={}", component.display());
}
