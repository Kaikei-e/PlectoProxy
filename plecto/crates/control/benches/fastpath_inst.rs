//! Instruction-count twin of the criterion `fastpath` bench (gungraun / callgrind), limited to
//! the LB pick — the per-request fast-path call whose ADR-surface cost the perf gate judges.
//! Instruction counts are frequency/thermal/neighbour-invariant, so the pass/fail comparison is
//! deterministic where wall-clock criterion drifts double digits on an unpinned host; wall-clock
//! stays in criterion (IPC regressions are invisible to instruction counts — both are needed).
//!
//! Running needs valgrind and a version-matched `gungraun-runner` on PATH; the target is gated
//! behind the `instruction-bench` feature so a plain `cargo bench` never requires them:
//!
//!   cargo bench -p plecto-control --features instruction-bench --bench fastpath_inst
//!   # judge against a named baseline, mirroring the criterion flow:
//!   ... --bench fastpath_inst -- --save-baseline main   /   -- --baseline main

use std::hint::black_box;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;

use gungraun::{library_benchmark, library_benchmark_group, main};
use plecto_control::{HashInput, Manifest, UpstreamGroup, UpstreamRegistry};

/// A minimal but complete `[upstream.health]` block (required by the manifest schema).
const HEALTH: &str = "[upstream.health]\npath = \"/healthz\"\ninterval_ms = 500\ntimeout_ms = 300\nhealthy_threshold = 2\nunhealthy_threshold = 2\n";

/// One healthy 3-instance `pool` balanced by `algo` — the same construction as the criterion
/// bench, done in the (unmeasured) gungraun setup. The `Arc` keeps the group alive after the
/// registry that built it is dropped.
fn group(algo: &str, hash: &str) -> Arc<UpstreamGroup> {
    let addrs = (0..3)
        .map(|i| format!("\"10.0.0.{}:8080\"", i + 1))
        .collect::<Vec<_>>()
        .join(", ");
    let toml = format!(
        "[[upstream]]\nname = \"pool\"\naddresses = [{addrs}]\nlb_algorithm = \"{algo}\"\n{HEALTH}{hash}\n"
    );
    let manifest = Manifest::from_toml(&toml).expect("parse upstream manifest");
    let reg = UpstreamRegistry::new();
    reg.reconcile(&manifest.upstreams, std::path::Path::new("."))
        .expect("reconcile pool");
    // Instances start pessimistic (unhealthy until probed); promote them so the bench measures a
    // real pick over the eligible set, not the eligible==0 fail-fast path.
    for g in reg.groups() {
        for inst in &g.endpoints().instances {
            inst.record_probe_success();
            inst.record_probe_success();
        }
    }
    reg.group("pool").expect("pool group")
}

fn rr_group() -> Arc<UpstreamGroup> {
    group("round_robin", "")
}

fn p2c_group() -> Arc<UpstreamGroup> {
    group("least_request", "")
}

fn maglev_group() -> Arc<UpstreamGroup> {
    group(
        "maglev",
        "[upstream.hash]\nkey = \"source_ip\"\ntable_size = 97",
    )
}

#[library_benchmark]
#[bench::round_robin(setup = rr_group)]
#[bench::least_request_p2c(setup = p2c_group)]
fn lb_pick(group: Arc<UpstreamGroup>) {
    let _ = black_box(group.pick(None));
}

#[library_benchmark]
#[bench::source_ip(setup = maglev_group)]
fn lb_pick_maglev(group: Arc<UpstreamGroup>) {
    let key = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7));
    let _ = black_box(group.pick(Some(HashInput::Ip(key))));
}

library_benchmark_group!(name = fastpath_inst, benchmarks = [lb_pick, lb_pick_maglev]);
main!(library_benchmark_groups = fastpath_inst);
