//! Fast-path micro-benchmarks (criterion): the per-request hot functions that run on every request
//! before any WASM filter — route matching, load-balancer pick, and ingress path normalization.
//!
//! These are deterministic, in-process, and network-free, so they isolate CPU cost with low noise
//! and are suitable for a CI regression gate (`--save-baseline main` / `--baseline main`). They
//! complement the end-to-end k6 macro scenarios: micro-cost × calls-per-request should roughly
//! explain the macro delta. The LB pick bench covers all three algorithms (round-robin, P2C
//! weighted-least-request, weighted Maglev — ADR 000035), which the macro suite only exercised for
//! round-robin.

use std::hint::black_box;
use std::net::{IpAddr, Ipv4Addr};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use plecto_control::{Control, HashInput, HttpRequest, Manifest, UpstreamRegistry};

/// `n` upstream instance address literals (stored as strings; never dialed here).
fn addrs(n: usize) -> String {
    (0..n)
        .map(|i| format!("\"10.0.0.{}:8080\"", i + 1))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A minimal but complete `[upstream.health]` block (required by the manifest schema).
const HEALTH: &str = "[upstream.health]\npath = \"/healthz\"\ninterval_ms = 500\ntimeout_ms = 300\nhealthy_threshold = 2\nunhealthy_threshold = 2\n";

/// An `UpstreamRegistry` holding one `pool` of `n` instances balanced by `algo`. `hash` is the
/// optional `[upstream.hash]` block (required for maglev).
fn registry(n: usize, algo: &str, hash: &str) -> UpstreamRegistry {
    let toml = format!(
        "[[upstream]]\nname = \"pool\"\naddresses = [{}]\nlb_algorithm = \"{algo}\"\n{HEALTH}{hash}\n",
        addrs(n)
    );
    let manifest = Manifest::from_toml(&toml).expect("parse upstream manifest");
    let reg = UpstreamRegistry::new();
    reg.reconcile(&manifest.upstreams, std::path::Path::new("."))
        .expect("reconcile pool");
    // Instances start pessimistic (unhealthy until probed); promote them so the bench measures a
    // real pick over the eligible set, not the eligible==0 fail-fast path.
    for group in reg.groups() {
        for inst in &group.endpoints().instances {
            inst.record_probe_success();
            inst.record_probe_success();
        }
    }
    reg
}

fn bench_lb_pick(c: &mut Criterion) {
    let mut g = c.benchmark_group("lb_pick");
    // Sweep the pool size: pick cost should stay ~flat (RR/P2C) or table-sized (maglev populate is
    // one-time; lookup is O(1)).
    for &n in &[3usize, 8, 32] {
        let rr = registry(n, "round_robin", "");
        let rr = rr.group("pool").unwrap();
        g.bench_with_input(BenchmarkId::new("round_robin", n), &n, |b, _| {
            b.iter(|| black_box(rr.pick(None)))
        });

        let p2c = registry(n, "least_request", "");
        let p2c = p2c.group("pool").unwrap();
        g.bench_with_input(BenchmarkId::new("least_request_p2c", n), &n, |b, _| {
            b.iter(|| black_box(p2c.pick(None)))
        });

        // Maglev needs a prime table >= instance count and a per-request hash key.
        let maglev = registry(
            n,
            "maglev",
            "[upstream.hash]\nkey = \"source_ip\"\ntable_size = 97",
        );
        let maglev = maglev.group("pool").unwrap();
        let key = IpAddr::V4(Ipv4Addr::new(203, 0, 113, 7));
        g.bench_with_input(BenchmarkId::new("maglev", n), &n, |b, _| {
            b.iter(|| black_box(maglev.pick(Some(HashInput::Ip(key)))))
        });
    }
    g.finish();
}

/// The per-pick `ArcSwap<Endpoints>` load (ADR 000044) under CONTINUOUS concurrent swap churn — a
/// background thread hammers `update_endpoints` with two address sets that share every instance
/// but one, while the foreground times `pick`. The shared instances are pre-promoted to healthy so
/// `pick` always has a real eligible set to choose from — the one rotating instance always starts
/// pessimistic (there is no active prober here) and simply never joins rotation, avoiding the
/// eligible==0 fail-fast trap the LB-pick bench's own history warns about (see its comment above).
/// This isolates the swap's steady-state cost, not the common idle tick (which `update_endpoints`
/// short-circuits to one atomic load + compare when nothing changed — not exercised here).
fn bench_pick_under_swap_churn(c: &mut Criterion) {
    let mut g = c.benchmark_group("pick_under_swap_churn");
    for &n in &[3usize, 8, 32] {
        let reg = registry(n, "round_robin", "");
        let group = reg.group("pool").unwrap();
        for inst in &group.endpoints().instances[..n - 1] {
            inst.record_probe_success();
            inst.record_probe_success();
        }

        let stable: Vec<(String, u32)> = (0..n - 1)
            .map(|i| (format!("10.0.0.{}:8080", i + 1), 1))
            .collect();
        let mut set_a = stable.clone();
        set_a.push(("10.0.9.1:8080".to_string(), 1));
        let mut set_b = stable;
        set_b.push(("10.0.9.2:8080".to_string(), 1));

        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let churner = {
            let group = group.clone();
            let stop = stop.clone();
            thread::spawn(move || {
                let mut toggle = false;
                while !stop.load(Ordering::Relaxed) {
                    group.update_endpoints(if toggle { &set_b } else { &set_a });
                    toggle = !toggle;
                }
            })
        };

        g.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| black_box(group.pick(None)))
        });

        stop.store(true, Ordering::Relaxed);
        churner.join().expect("the churn thread must not panic");
    }
    g.finish();
}

fn bench_find_route(c: &mut Criterion) {
    let mut g = c.benchmark_group("find_route");
    for &n in &[1usize, 16, 64] {
        let mut toml =
            format!("[[upstream]]\nname = \"pool\"\naddresses = [\"10.0.0.1:8080\"]\n{HEALTH}");
        for i in 0..n {
            toml.push_str(&format!(
                "\n[[route]]\nupstream = \"pool\"\n[route.match]\npath_prefix = \"/svc{i}\"\n"
            ));
        }
        let manifest = Manifest::from_toml(&toml).expect("parse route manifest");
        let control = Control::from_manifest(&manifest, Path::new(".")).expect("build control");
        let snapshot = control.snapshot();
        // Worst case: a request matching the LAST-declared route, so specificity ordering is fully
        // exercised.
        let request = HttpRequest {
            method: "GET".to_string(),
            path: format!("/svc{}/resource", n - 1),
            authority: "example.test".to_string(),
            scheme: "https".to_string(),
            headers: vec![],
        };
        g.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| black_box(snapshot.find_route(black_box(&request))))
        });
    }
    g.finish();
}

fn bench_normalize_path(c: &mut Criterion) {
    let mut g = c.benchmark_group("normalize_path");
    for (label, path) in [
        ("plain", "/api/v1/users/12345/orders"),
        ("dot_segments", "/api/../api/v1/./users/./12345"),
        (
            "with_query",
            "/api/v1/search?q=hello+world&page=2&sort=desc",
        ),
    ] {
        g.bench_function(label, |b| {
            b.iter(|| black_box(plecto_control::normalize_path(black_box(path))))
        });
    }
    g.finish();
}

criterion_group!(
    benches,
    bench_lb_pick,
    bench_pick_under_swap_churn,
    bench_find_route,
    bench_normalize_path
);
criterion_main!(benches);
