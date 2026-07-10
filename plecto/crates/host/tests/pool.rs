//! Trusted instance-pool concurrency (tdd-workflow Phase 0, ADR 000012).
//!
//! M1's remaining piece (foundation plan §6.3): the trusted runtime is no longer a single
//! instance behind one `Mutex` (concurrency=1) but a fixed-capacity, lazily-filled pool of
//! reusable initialized instances, checked out per request. These E2E tests drive real
//! `filter-hello` components through the pool and assert the client-visible lifecycle:
//!   - genuine concurrency — under contention the pool builds a second instance (the pooling
//!     allocator finally earns its keep), observed via the host `init-calls` counter;
//!   - saturation is fail-closed — when every instance is checked out, a further request waits
//!     a bounded time then surfaces `RunError::Unavailable` (never a pass-through, never an
//!     unbounded hang);
//!   - recycling bounds linear-memory state accumulation (§6.6 footgun) — after serving a
//!     configured number of requests an instance is discarded and rebuilt (init re-runs).
//!
//! The pool-wide circuit breaker and per-instance trap discard are exercised by the serial
//! trap tests in `e2e.rs`, which now run on the pool.

use std::sync::Barrier;
use std::thread;

use plecto_host::test_support::{TestSigner, bound_sbom};
use plecto_host::{
    Header, Host, HttpRequest, LoadOptions, LoadedFilter, LogLine, RequestDecision, RequestTrace,
    RunError, SignedArtifact,
};

fn component_bytes() -> Vec<u8> {
    std::fs::read(env!("FILTER_HELLO_COMPONENT")).expect("read filter-hello component")
}

/// Sign filter-hello with a fresh ephemeral key, build a `Host` trusting exactly that key, and
/// load the filter under the given options. The `Host` is returned because it owns the epoch
/// ticker and must outlive the filter.
fn signed_load(opts: LoadOptions) -> (Host, LoadedFilter) {
    let bytes = component_bytes();
    let signer = TestSigner::new().unwrap();
    let component_signature = signer.sign(&bytes).unwrap();
    let sbom = bound_sbom(&bytes);
    let sbom_signature = signer.sign(&sbom).unwrap();
    let host = Host::new(signer.trust_policy().unwrap()).unwrap();
    let artifact = SignedArtifact {
        component_bytes: &bytes,
        component_signature: &component_signature,
        sbom: &sbom,
        sbom_signature: &sbom_signature,
    };
    let filter = host.load("filter-hello", &artifact, opts).unwrap();
    (host, filter)
}

fn request(headers: &[(&str, &str)]) -> HttpRequest {
    HttpRequest {
        method: "GET".to_string(),
        path: "/".to_string(),
        authority: "example.test".to_string(),
        scheme: "https".to_string(),
        headers: headers
            .iter()
            .map(|(n, v)| Header {
                name: (*n).to_string(),
                value: v.as_bytes().to_vec(),
            })
            .collect(),
    }
}

/// The filter logs `init-calls=N` each request (a HOST counter incremented once per `init`),
/// so this counts how many distinct instances the pool has ever built (it is monotonic).
fn init_calls(logs: &[LogLine]) -> u64 {
    logs.iter()
        .find_map(|l| {
            l.message
                .strip_prefix("init-calls=")
                .and_then(|n| n.parse().ok())
        })
        .expect("filter-hello logs init-calls=N every request")
}

/// Iterations for the bounded busy loop (`x-plecto-busy`) used to keep an instance CHECKED OUT
/// long enough for another thread to observe contention. Tens of milliseconds of cheap work —
/// long enough to overlap, short of any sane per-request deadline.
const BUSY_OVERLAP_ITERS: u64 = 50_000_000;
/// A much longer hold for the saturation test: hundreds of milliseconds even on a fast CPU, so
/// the bounded checkout wait (tens of ms) provably times out while the holder is still busy.
const BUSY_HOLD_ITERS: u64 = 1_200_000_000;

#[test]
fn trusted_pool_serves_concurrent_requests() {
    // Under contention the pool must run more than one instance at once: while thread A is busy
    // holding its instance, thread B finds the idle set empty and (live < cap) builds a SECOND
    // instance — so `init` runs again and the host `init-calls` counter climbs past 1. A single
    // serial slot (the v0.1 placeholder) could never build a second instance, so this falsifies
    // "concurrency=1". A generous per-request deadline keeps the busy loop from tripping epoch.
    let pool_size = 4;
    let (_host, filter) = signed_load(
        LoadOptions::trusted()
            .with_trusted_pool_size(pool_size)
            .with_request_deadline_ms(30_000),
    );

    let threads = 4;
    let rounds = 2;
    let barrier = Barrier::new(threads);
    thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                for _ in 0..rounds {
                    // align all threads so their checkouts contend in the same window
                    barrier.wait();
                    let (decision, _logs) = filter
                        .on_request(
                            &request(&[("x-plecto-busy", &BUSY_OVERLAP_ITERS.to_string())]),
                            &RequestTrace::root(),
                        )
                        .expect("a concurrent trusted request must succeed");
                    assert!(
                        matches!(decision, RequestDecision::Continue),
                        "a busy-but-benign request should continue"
                    );
                }
            });
        }
    });

    // a final request observes how many instances the pool built: more than one ⇒ real concurrency.
    let (_decision, logs) = filter
        .on_request(&request(&[]), &RequestTrace::root())
        .unwrap();
    assert!(
        init_calls(&logs) >= 2,
        "under contention the pool must build >1 instance (init ran {} times)",
        init_calls(&logs)
    );
}

#[test]
fn trusted_pool_saturation_is_fail_closed() {
    // With capacity 1, one in-flight request holds the only instance. A concurrent request finds
    // the pool saturated, waits the bounded checkout timeout, then fails CLOSED with
    // `RunError::Unavailable` — it must neither hang forever nor pass through to upstream.
    let (_host, filter) = signed_load(
        LoadOptions::trusted()
            .with_trusted_pool_size(1)
            .with_request_deadline_ms(30_000)
            .with_checkout_timeout_ms(10),
    );

    thread::scope(|s| {
        // holder: occupy the single instance for well over the checkout timeout.
        let holder = s.spawn(|| {
            filter
                .on_request(
                    &request(&[("x-plecto-busy", &BUSY_HOLD_ITERS.to_string())]),
                    &RequestTrace::root(),
                )
                .expect("the holder's own request still succeeds");
        });

        // give the holder time to check the instance out and enter its busy loop (the eager
        // instance is already built at load, so checkout is just a pop — a few ms at most).
        thread::sleep(std::time::Duration::from_millis(60));

        // now the pool is saturated: this checkout waits 10ms then fails closed.
        let contended = filter.on_request(&request(&[]), &RequestTrace::root());
        assert!(
            matches!(contended, Err(RunError::Unavailable)),
            "a saturated trusted pool must fail closed with Unavailable, got {contended:?}"
        );

        holder.join().unwrap();
    });

    // once the holder finishes, the pool is usable again (no permanent breakage).
    let (decision, _logs) = filter
        .on_request(&request(&[]), &RequestTrace::root())
        .expect("the pool recovers once the in-flight request returns");
    assert!(matches!(decision, RequestDecision::Continue));
}

#[test]
fn trusted_instance_recycled_after_max_requests() {
    // §6.6 footgun mitigation: a trusted instance is not zeroized between requests, so to bound
    // accidental linear-memory state accumulation the pool RECYCLES it (discard + rebuild, so
    // init re-runs) after a configured number of requests. With capacity 1 and a recycle bound
    // of 2, `init-calls` must step up every two requests as the instance is rebuilt.
    let (_host, filter) = signed_load(
        LoadOptions::trusted()
            .with_trusted_pool_size(1)
            .with_max_requests_per_instance(2),
    );

    let mut seen = Vec::new();
    for _ in 0..5 {
        let (_decision, logs) = filter
            .on_request(&request(&[]), &RequestTrace::root())
            .unwrap();
        seen.push(init_calls(&logs));
    }
    // eager instance serves reqs 1-2, recycled; rebuild serves 3-4, recycled; rebuild serves 5.
    assert_eq!(
        seen,
        vec![1, 1, 2, 2, 3],
        "an instance must be recycled (rebuilt, re-init) every 2 requests"
    );
}

#[test]
fn trusted_pool_has_no_deadlock_under_churn() {
    // Liveness: more concurrent callers than the pool capacity forces the bounded-wait checkout
    // path repeatedly. Every request must still complete (no deadlock, no lost wakeup) — fast
    // benign requests, so holders free their instance quickly and waiters make progress.
    let (_host, filter) = signed_load(
        LoadOptions::trusted()
            .with_trusted_pool_size(2)
            .with_checkout_timeout_ms(5_000),
    );

    let threads = 6;
    let rounds = 30;
    thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                for _ in 0..rounds {
                    let (decision, _logs) = filter
                        .on_request(&request(&[]), &RequestTrace::root())
                        .expect("every churned request must complete");
                    assert!(matches!(decision, RequestDecision::Continue));
                }
            });
        }
    });
}
