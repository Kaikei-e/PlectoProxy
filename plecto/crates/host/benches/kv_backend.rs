//! Contended-write micro-benchmark for the `KvBackend` seam: hot-path host-state writes
//! (`host-counter` / `host-ratelimit`) converge on redb's single global writer, so this is
//! the first ceiling a filter-heavy deployment meets. The redb backend's group commit must
//! hold its per-op cost roughly flat as writer concurrency grows; `MemoryBackend` is the
//! contention-free reference ceiling. Wall-clock, informational (bench.yml `micro` policy).

use std::sync::Arc;
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use plecto_host::{KvBackend, MemoryBackend, RedbBackend};

/// Partition `iters` across `threads` so the total work equals Criterion's `iters`
/// (no `div_ceil` over-count when `iters % threads != 0`).
fn split_iters_across_threads(threads: u64, iters: u64) -> Vec<u64> {
    let threads = threads.max(1);
    let base = iters / threads;
    let rem = iters % threads;
    (0..threads).map(|t| base + u64::from(t < rem)).collect()
}

/// `threads` workers split `iters` increments of ONE shared key — worst-case write
/// contention: every op is a read-modify-write behind the same serialization point.
fn contended_increments(backend: &Arc<dyn KvBackend>, threads: u64, iters: u64) -> Duration {
    let per_thread = split_iters_across_threads(threads, iters);
    let start = Instant::now();
    std::thread::scope(|s| {
        for n in per_thread {
            let backend = Arc::clone(backend);
            s.spawn(move || {
                for _ in 0..n {
                    backend.increment(b"bench-counter", 1);
                }
            });
        }
    });
    start.elapsed()
}

fn bench_contended_writes(c: &mut Criterion) {
    let mut group = c.benchmark_group("kv_backend_contended_increment");
    for threads in [1u64, 8] {
        group.bench_with_input(
            BenchmarkId::new("redb", threads),
            &threads,
            |b, &threads| {
                let dir = tempfile::tempdir().unwrap();
                let backend: Arc<dyn KvBackend> =
                    Arc::new(RedbBackend::open(dir.path().join("bench.redb")).unwrap());
                b.iter_custom(|iters| contended_increments(&backend, threads, iters));
            },
        );
        group.bench_with_input(
            BenchmarkId::new("memory", threads),
            &threads,
            |b, &threads| {
                let backend: Arc<dyn KvBackend> = Arc::new(MemoryBackend::default());
                b.iter_custom(|iters| contended_increments(&backend, threads, iters));
            },
        );
    }
    group.finish();
}

criterion_group!(benches, bench_contended_writes);
criterion_main!(benches);
