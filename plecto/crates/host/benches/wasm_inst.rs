//! Instruction-count twin of the criterion `wasm` bench (gungraun / callgrind): the SAME
//! per-request hot calls, measured in executed instructions instead of wall-clock. Instruction
//! counts are invariant to CPU frequency, thermal state and neighbour load (typical run-to-run
//! variance well under 0.1 %), so "did the contract surface get more expensive?" is judged here
//! deterministically; wall-clock stays in criterion (an IPC regression does not show up in
//! instruction counts — both layers are needed).
//!
//! Running needs valgrind and a version-matched `gungraun-runner` on PATH; the target is gated
//! behind the `instruction-bench` feature so a plain `cargo bench` never requires them:
//!
//!   cargo install gungraun-runner   # once, version-matched to the dev-dependency
//!   cargo bench -p plecto-host --features instruction-bench --bench wasm_inst
//!   # judge against a named baseline, mirroring the criterion flow:
//!   ... --bench wasm_inst -- --save-baseline=main   /   -- --baseline=main
//!   (values must be `=`-attached: a space-separated value is parsed as a benchmark filter)

use std::hint::black_box;

use gungraun::{library_benchmark, library_benchmark_group, main};
use plecto_host::test_support::{TestSigner, bound_sbom, filter_hello_component};
use plecto_host::{
    Header, Host, HttpRequest, LoadOptions, LoadedFilter, RequestTrace, SignedArtifact,
};

/// Everything one `on_request` call needs. Built by the (unmeasured) gungraun setup; the `Host`
/// rides along so its epoch ticker outlives the measured call.
struct Fx {
    _host: Host,
    filter: LoadedFilter,
    req: HttpRequest,
    trace: RequestTrace,
}

fn fixture(opts: LoadOptions) -> Fx {
    let component = filter_hello_component();
    let signer = TestSigner::new().unwrap();
    let component_sig = signer.sign(&component).unwrap();
    let sbom = bound_sbom(&component);
    let sbom_sig = signer.sign(&sbom).unwrap();
    let host = Host::new(signer.trust_policy().unwrap()).unwrap();
    let filter = host
        .load(
            "bench",
            &SignedArtifact {
                component_bytes: &component,
                component_signature: &component_sig,
                sbom: &sbom,
                sbom_signature: &sbom_sig,
            },
            opts,
        )
        .unwrap();
    let req = HttpRequest {
        method: "GET".to_string(),
        path: "/api/data".to_string(),
        authority: "example.test".to_string(),
        scheme: "https".to_string(),
        headers: vec![Header {
            name: "x-req".to_string(),
            value: b"1".to_vec(),
        }],
    };
    let trace = RequestTrace::root();
    let _ = filter.on_request(&req, &trace); // burn one-time lazy paths out of the count
    Fx {
        _host: host,
        filter,
        req,
        trace,
    }
}

fn pooled_fixture() -> Fx {
    fixture(LoadOptions::trusted())
}

fn fresh_fixture() -> Fx {
    fixture(LoadOptions::untrusted())
}

#[library_benchmark]
#[bench::trusted_pooled(setup = pooled_fixture)]
#[bench::untrusted_fresh(setup = fresh_fixture)]
fn on_request(fx: Fx) {
    let _ = black_box(fx.filter.on_request(black_box(&fx.req), &fx.trace));
}

library_benchmark_group!(name = wasm_inst, benchmarks = [on_request]);
main!(library_benchmark_groups = wasm_inst);
