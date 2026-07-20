# Plecto Proxy Performance

An honest performance snapshot of Plecto Proxy's two halves: the **native load-balancing fast
path** and the **WASM extension plane** (per-request filters, host-enforced rate limiting, the
request-body hook). The goal is **transparency about method**, not a leaderboard. Every number
here is an internal **regression baseline** — not a capacity guide, and not a comparison against
other proxies.

All components — load generator, Plecto Proxy, the upstream instances, and any tooling — run
**co-resident on a single commodity developer host over loopback**, so absolute figures are
bounded by that host and by the generator, not by Plecto Proxy in isolation. Read them as **relative**
signals — ratios, curve shapes and time-constants, not headline throughput.

## Measurement setup

- **Core isolation by pinning.** Plecto Proxy (and its in-process backends) is pinned to one dedicated
  set of CPU cores; **every** load generator is pinned to a separate, disjoint set. The generator
  therefore never steals a core from the proxy — the run measures Plecto Proxy, not the generator
  fighting it. (Done with `taskset`; no privileged host tuning.)
- **No host tuning.** CPU governor / turbo are left at their defaults — no fixed-frequency lock.
  Absolute throughput shifts run-to-run with clock; the **ratios, shapes and time-constants** are
  the durable signal, so those are what we read.
- **Generators, by phase.** [k6](https://grafana.com/docs/k6/latest/) drives the closed-loop
  concurrency sweep (`constant-vus`), the mixed short-circuit run, and the rate-limit / body
  scenarios; **`plecto-loadgen openloop`** is the **authoritative** open-loop tail driver
  (constant arrival rate with **schedule-based latency** — the wrk2 / Gil Tene model; see
  [`bench/methodology.md`](../bench/methodology.md)); `OPENLOOP_GEN=k6` keeps the older
  `constant-arrival-rate` path for A/B. `plecto-loadgen` also runs the fault-injection timeline,
  the endpoint-set swap timeline, the round-robin count, and the WebSocket / TLS-handshake
  scenarios; and [oha](https://github.com/hatoo/oha) drives the single-route ceiling (plain h1,
  WASM W1, TLS) runs. Different generators have different ceilings — **numbers are comparable
  within a section, and across same-generator sections, but not blindly across all of them**.
  Each section names its generator.
- **Warm-up excluded.** Every measured window starts after a short warm-up (default 5 s) that
  sends load but is not recorded: in-script for k6 and plecto-loadgen, a discarded pre-run for
  oha. Cold-start seconds (route tables, upstream pools, allocator state) never enter a
  percentile. The rate-limit enforcement / fairness runs are the deliberate exception — their
  initial token-bucket burst *is* the measured signal.
- **Ceilings vs tails.** Closed-loop full-throttle runs (oha, `constant-vus`) are read as
  *throughput ceilings*; their latencies are queueing-at-saturation, not service latency
  ("never measure latency at max load"). Honest tails come from the fixed-rate runs:
  **`plecto-loadgen openloop`** (schedule-latency) and oha `-q` + `--latency-correction`, both
  coordinated-omission-safe. The plain-h1 ceiling reports **RR** (keep-alive) and **CRR**
  (cold TCP/req) KPIs in `ceiling.csv`.
- **Fully local.** Generators, proxy and upstreams talk only over loopback; generator telemetry and
  the optional dashboard's phone-home are disabled. Nothing leaves the host during a load run —
  load traffic stays on loopback; `REQUIRE_OFFLINE=1` can refuse a default IPv4 route for a
  netns-style lab (see [`bench/methodology.md`](../bench/methodology.md)).
- **PMU not collected.** The runbook's optional micro-architectural attribution (cycles/req, IPC,
  LLC / branch misses via `perf`) needs a lowered `kernel.perf_event_paranoid` (privileged); it
  was not enabled on this run, so the WASM / rate-limit tax is reported as throughput / latency /
  **µs-per-req**, not a cycles breakdown.

## TL;DR

> **Measurement history** (newest first). **2026-07-20 (v0.5.1/v0.5.2 patch confirmation)** — a
> full refresh: T1 `gate` (**PASS**, every invariant in band), a full `bash bench/perf/run-perf.sh
> all` (T2), and `v03` (T3). Measured at commit `c635ed3` (tag **v0.5.1**); tag **v0.5.2** landed
> on top moments later as an unintended early release — version strings and three reference-filter
> patch bumps only (`filter-cors` / `filter-apikey` / `filter-extauthz` 0.1.1 → 0.1.2), no
> `plecto-server` / `plecto-control` / `plecto-host` source changed, so every figure below stands
> for v0.5.2 as shipped too. The entire load run executed inside an unprivileged network namespace
> (`unshare -rn`, `ip link set lo up`, no default route) rather than relying only on the runbook's
> own `REQUIRE_OFFLINE=1` self-check — a kernel-enforced guarantee that nothing left the host during
> the run, verified beforehand (`curl http://example.com` fails at DNS resolution inside the
> namespace, before any route is even consulted). **New finding this pass** — ADR 000092's
> per-source-IP connection cap (**256** concurrent connections/IP, landed 2026-07-15, after the
> prior 07-11 snapshot) now intersects several k6 open-loop scenarios whose `preAllocatedVUs` pool
> exceeds 256, because the generator and Plecto Proxy share one loopback source IP on this harness.
> Confirmed two ways: the closed-loop **sweep** fails cleanly above the threshold (0 % at VU ≤ 200,
> **28 % / 47 %** at VU 400/800 — reproduced identically with and without the netns sandbox, ruling
> the isolation method out as the cause), and the **rate-limit enforcement / fairness (hot key)**
> scenarios silently drop **43–49 %** of offered load from their own accepted/limited accounting (a
> refused connection returns no HTTP status, so k6's `status === 200 | 429` branches never see it).
> Affected numbers are flagged inline below; every oha-driven section (ceiling, WASM ladder, TLS,
> footprint — all `-c 50`), the low-VU k6 scenarios (body, rate-limit overhead — `VUS=50`), and every
> `plecto-loadgen` scenario (open-loop, round-robin, ejection, swap, WebSocket — all ≤ 64 workers)
> stay well under the cap and are clean, comparable figures. **2026-07-11 (v0.3.0 feature costs)** —
> targeted `bash bench/perf/run-perf.sh v03` (not a full `all` refresh): fills the previously-unmeasured
> ADR 000073 response-context / `replace` rungs and the ADR 000074/075 compression opt-in row.
> Method: same adjacent-delta ladder + oha fixed-rate CO-safe tails as the WASM plane; see
> [`bench/methodology.md`](../bench/methodology.md) § v0.3.0 response / compression. Track
> **µs/req** (and fixed-rate p50), not %-of-baseline. **Older generations** (2026-07-11 release
> confirmation … 07-02) live in [`HISTORY.md`](HISTORY.md) — this TL;DR keeps only the newest two.
> **µs/req deltas are what to track across snapshots**, not raw throughput — and the tracked
> invariant set is machine-checked by the T1 gate (`bash bench/perf/run-perf.sh gate`, bands in
> `bench/perf/gate_tolerances.toml`).

**Load-balancing fast path** (plaintext HTTP/1.1, 3 upstreams, trivial 0 ms backend; k6 / loadgen / oha):

- Closed-loop throughput peaks at **~150.5k req/s** (VU 100 this run) with **p99 ≈ 1.2–2.4 ms** and
  zero failures through VU 200. **VU 400/800 now show 28 % / 47 % "failed"** — this is **not** proxy
  overload: it is the sweep's own concurrency (400/800 simultaneous connections, all from the
  generator's single loopback IP) crossing ADR 000092's new **256-connections-per-source-IP**
  admission cap (landed 2026-07-15, after the prior snapshot) — see the measurement-history callout
  above and [the sweep section](#throughput--latency-vs-concurrency) for the reproduction. Under the
  cap (VU ≤ 200) the curve still declines gracefully with no cliff.
- Open-loop at the auto **105.3k/s** (70 % of closed-loop peak) **achieves 105 342/s exactly** with
  **p50 2.3 ms, p95 19.1 ms, p99 32.4 ms, p99.9 44.9 ms, 0 dropped, 0 % failed** — schedule-latency
  (`plecto-loadgen openloop`, 64 workers — well under the per-IP cap, unaffected).
- Round-robin across three upstreams is **even to within one request** (33.3 % each, 120,000 reqs).
- **Resilience is as designed**: ejecting one upstream drops its share to zero in ~1 s and the
  survivors absorb the load with **no client-visible errors**; a *total* outage **fails closed
  with HTTP 503** and the pool **recovers within ~1 s** of health returning.
- TLS termination (**aws-lc-rs**, ADR 000051): within-TLS, keep-alive **~118k** (~48 % of the
  plaintext ceiling) vs handshake/req **~27.7k** (~23 % of keep-alive) and h2 **~98.4k** (~83 % of
  keep-alive) — the path is **crypto-/TLS-I/O-bound**, ordering clean. A resumption-isolated
  measurement (carried from 07-05, not re-run this pass) puts a **true full handshake at ~22.1k/s**
  vs **~29.8k/s resumed (93 %)** — see [TLS](#tls-termination).
- A **kept-alive** connection (**RR**) serves **~248.2k req/s** this run; forcing a **TCP
  handshake per request** (**CRR**) costs **~45 % throughput and +0.54 ms p99** — connection
  reuse is still load-bearing (see [the plain HTTP/1.1 ceiling](#plain-http11-ceiling)).

**WASM extension plane** (the cost of running a decision as a sandboxed component; oha / k6):

- A **cost ladder** isolates each cost by adjacent delta (oha, `-c 50` — well under the per-IP cap,
  clean). This run's full-throttle ceiling is clean (`baseline` **>** every WASM rung), so the raw
  floor reads directly: **baseline → noop-pooled costs ~53 % throughput** full-throttle
  (**≈ 4.5 µs/req** inverse-throughput delta, matching the interleaved T1 gate's **4.41 µs**
  dispatch-floor invariant); the **fixed-rate tail** (2,402 req/s, the portable queueing-honest read)
  puts it at **+0.15 ms p50 / +0.31 ms p99** over native. A **real filter's own work**
  (`filter-apikey` on top of the pooled no-op) is **≈ 0.44 µs** — matching the gate's **0.44 µs**
  apikey-cost invariant almost exactly; running that filter **fresh-per-request** instead of pooled
  costs **~27–28×** throughput — the price of re-paying `init` every request.
- These macro deltas **reconcile with the criterion [micro-benchmarks](#0-micro-benchmarks-in-process-criterion)**
  in direction and order of magnitude, and both agree with the **T1 gate's PASS** verdict
  (`bash bench/perf/run-perf.sh gate`, every invariant in `bench/perf/gate_tolerances.toml`'s band).
- **v0.3.0 response / compression (opt-in `v03` phase, re-run 2026-07-20):** reading the
  as-forwarded request snapshot on `on-response` costs **≈ +0.10 µs/req** over pooled no-op this
  pass (small, at the edge of full-throttle measurement noise — see the caveat in
  [the section itself](#v030-response-ladder--compression)); gzip on a 4 KiB compressible body costs
  **≈ −32 % ceiling / +2.1 µs/req** vs the same body uncompressed, matching the prior pass closely.
- A rejected request (**HTTP 401 short-circuit**) is decided in **~0.20 ms and never reaches the
  backend** — bad traffic that *does* get a response is shed **~82× faster** than good traffic is
  forwarded through a 15 ms backend. **This run's accept/reject split (76 % / 24 %) is not
  comparable to the ~90 %/10 % design mix**: the driving k6 scenario pre-allocates 300 VUs, above
  the per-IP cap, so some share of the "rejected" bucket is very likely refused connections
  (no HTTP status at all), not genuine 401s — see
  [the section](#short-circuit-rejecting-bad-traffic-at-the-edge).

**Host-enforced rate limiting** (token bucket, spec host-configured in the manifest; k6):

- The rate-limited route costs **~2.8 µs/req** (~29 % throughput, p99 unchanged) over a no-filter
  baseline when the bucket never denies (`VUS=50`, well under the per-IP cap, clean) — the filter
  dispatch floor plus the host-native bucket consult (and its multi-tenant quota check).
- Offered **5× over the configured rate**, the **allowed throughput still converges correctly to the
  bucket's refill rate** (≈ 1.0k/s for a 1000-token/s bucket — unaffected, computed only over
  successfully-connected requests). **The measured "shed" fraction (59.6 %) is unreliable this
  pass**: this scenario's `preAllocatedVUs` (500, for a 5,000 req/s offer) exceeds the per-IP cap, so
  roughly **49 % of attempted requests never produced an HTTP status at all** and silently vanish
  from both the accepted and limited counters — the historical **79.3 %** shed figure remains the
  trustworthy reference until the harness accounts for refused connections separately.
- Buckets are **per key**: a hot key offered 4× its limit is throttled to its refill rate (same
  caveat as enforcement above — its scenario also exceeds the per-IP cap) while a **light key on the
  same filter passes untouched (0 % shed)** — its low-rate scenario stays under the cap, clean, and
  confirms no cross-key starvation regardless.

**Request-body hook** (buffer-then-decide, ADR 000025; export-presence zero-copy bypass, ADR 000038; k6):

- A filter that **reads** the body (`/body`, filter-hello) costs **~47 % throughput at 1 KB** and
  scales with payload: **~59 % at 100 KB**, **~66 % at 1 MB**, versus the streaming passthrough
  (`VUS=50`, well under the per-IP cap, clean).
  A **header-only filter** (`/body-headeronly`) **streams the body through**: at 1 MB it lands
  **within ~0.5 % of `/baseline`** (ADR 000038, within noise); at 100 KB the gap widened to **~12 %**
  this pass (host noise on this run, not a new mechanism) and at 1 KB the gap is the ordinary
  **WASM dispatch floor** on a tiny request, not a body cost.
- RSS at 1 MB × 50 VUs (`MALLOC_ARENA_MAX=4`): **~106 MB `/baseline` · ~187 MB `/body` · ~101 MB
  `/body-headeronly`**. The header-only bypass stays near baseline; the buffer stays bounded (16 MiB
  cap, fail-closed 413).

## Scope & honesty notes

- **Machine specs intentionally omitted.** Single commodity host, loopback, everything
  co-resident. Absolute throughput is contended and clock-variable; treat figures as relative /
  regression signals.
- **Generator-bound where noted.** The closed-loop sweep tops out near the *generator's* ceiling on
  its cores, not the proxy's: absolute peaks move with host/generator noise (this run ~150.5k k6
  peak vs ~248.2k oha ceiling keep-alive — different generators, different ceilings). The sweep
  curve's *shape* is the signal, not
  its absolute peak (below the per-IP admission cap — see the [TL;DR callout](#tldr) for VU ≥ 400).
  Open-loop tails use `plecto-loadgen` so they are no longer k6-VU-bound.
- **Trivial upstreams** (tiny static responses, 0 ms latency by default) deliberately isolate
  **proxy + LB + filter overhead** rather than backend work. A 15 ms synthetic backend is used
  where realistic proportions matter (WASM short-circuit); a sized-body backend for the body sweep.
- The LB figures are **plaintext HTTP/1.1**, except the dedicated [TLS run](#tls-termination).
- **No comparative claims.** Mature proxies are referenced only for shared methodology, never ranking.
- Charts rendered with matplotlib → WebP; an optional InfluxDB + Grafana stack (`INFLUX=1`) provides
  live dashboards during k6 runs (its images are a one-time setup pull; the load stays on loopback).

---

# 0. Micro-benchmarks (in-process, criterion)

A deterministic, network-free layer (`cargo bench`, criterion) that isolates the **per-function** cost
of the hot path with low noise — complementary to the end-to-end macro scenarios below, and the basis
for the CI regression gate (`--save-baseline` / `--baseline`). Micro-cost × calls-per-request should
roughly explain the macro deltas, and it does (the WASM ladder is the worked example).

**Fast path** (`crates/control/benches/fastpath.rs`):

| bench | cost | note |
| --- | --- | --- |
| LB pick — round-robin | 44 → 36 → 31 ns (32 → 8 → 3 instances) | ~O(1) over the eligible set |
| LB pick — P2C weighted-least-request | 41 → 49 → 79 ns (3 → 8 → 32 instances) | two eligibility passes + the sampled compare |
| LB pick — weighted Maglev | ~27 ns (3, 8) → ~36 ns (32) | + one table lookup |
| LB pick under swap churn (`pick_under_swap_churn`) | 99 → 65 → 67 ns (3 → 8 → 32 instances) | round-robin pick while a background thread continuously calls `update_endpoints` (ADR 000044) — the per-pick `ArcSwap<Endpoints>` load cost under worst-case concurrent churn |
| route match (`find_route`) | 65 ns → 249 ns (1 → 64 routes) | scans by specificity, allocation-free |
| ingress path normalization | ~55–94 ns clean / ~184 ns dot-segments | ADR 000027; a clean path is borrowed, no allocation |

All three LB algorithms are covered here; the macro suite only load-tests round-robin. The `n=3`
`pick_under_swap_churn` cell reads slower than `n=8`/`n=32` — under continuous churn the eligible
set is tiny (2 instances) relative to the fixed cost of the concurrent `update_endpoints`
allocation contending for the same cache lines every tick; reported as measured, not smoothed.

**Extension plane** (`crates/host/benches/wasm.rs`):

| bench | cost | isolates |
| --- | --- | --- |
| `on_request` — pooled instance | ~2.24 µs/req | dispatch + call (init amortized) |
| `on_request` — fresh instance / request | ~42.5 µs/req | + per-request instantiation (the pool's value) |
| cold `load` (verify + instantiate + init) | ~22.6 ms | cosign signature + SBOM verification dominates |

The ~19× pooled→fresh gap here is the same one the [macro ladder](#the-wasm-cost-ladder--isolating-each-cost)
shows end-to-end (~28× this snapshot, with the HTTP layer and its own run-to-run noise around it) —
the two layers agree in direction and order of magnitude, so a divergence between them would be a
real bug. (Both tables freshly re-run 2026-07-20, no `--save-baseline`/`--baseline` comparison this
pass — absolute values only, unpinned governor; day-to-day drift of ±10–20 % is expected per
[`bench/methodology.md`](../bench/methodology.md) § Measurement tiers.)

---

# 1. Load-balancing fast path

Subject: one Plecto Proxy route forwarding to an upstream pool of **3 instances**, round-robin pick
over the healthy set, active health probe every **500 ms** with eject after **2** consecutive
failures (≈ ~1 s to detect). The three upstream nodes are three loopback backends, so the run
needs no external network.

## Plain HTTP/1.1 ceiling

The canonical reference figure every other section in this report reads from — measured **once**,
on `bench-server`'s filter-less `/baseline` route (oha; keep-alive vs a fresh TCP handshake per
request, `--disable-keepalive`). Before the `bench-server` harness merge this same route was
measured independently by three different processes (the WASM ladder's own server, the TLS run's
plaintext control, and a standalone churn run) — three numbers for one thing, differing only by
host noise. The `ceiling` phase now produces `ceiling.csv`; the [WASM ladder](#the-wasm-cost-ladder--isolating-each-cost)'s
`baseline` row and [TLS termination](#tls-termination)'s `plain (h1)` row cite it instead of
re-measuring.

![Plain HTTP/1.1 ceiling](img/ceiling.webp)

| Variant | KPI | req/s | p50 | p99 |
| --- | --- | --- | --- | --- |
| keep-alive       | RR  | 248,187 | 0.19 ms | 0.47 ms |
| cold (TCP/req)   | CRR | 135,448 | 0.32 ms | 1.01 ms |

*(Re-measured 2026-07-20 (v0.5.1/v0.5.2 patch confirmation) — `bash bench/perf/run-perf.sh all` /
`ceiling`, inside a network-isolated sandbox (`unshare -rn`; see the [TL;DR](#tldr) callout).
Absolute keep-alive sits within host-noise band of the 07-11 pass (241.3k → 248.2k); cold/keep-alive
**ratio** and the RR/CRR split are the durable signal.)*

A TCP handshake per request costs **~45 % throughput and +0.54 ms p99** even on loopback (where the
handshake is nearly free) — over a real network the gap widens with RTT. Connection reuse is
load-bearing; this is the plaintext analogue of the [TLS handshake-per-request row](#tls-termination) below.

> **A note on a latency bug this scenario caught.** An early body run showed a ~40 ms p99 cliff on
> medium streamed bodies — the signature of a delayed-ACK stall. The upstream client had Nagle's
> algorithm on (no `TCP_NODELAY`), so a streamed request body sent in several writes stalled on the
> peer's delayed-ACK timer. Disabling Nagle on the upstream sockets — standard practice for L7
> proxies — removed it (100 KB streamed p99 42.9 ms → 4.2 ms). The numbers here are post-fix.

## Throughput & latency vs concurrency

Closed-loop sweep (k6 `constant-vus`) — a fixed number of virtual users, each issuing its next
request only after the previous response. Rising concurrency walks the load curve.

![Throughput vs concurrency](img/throughput_vs_concurrency.webp)
![Latency percentiles vs concurrency](img/latency_vs_concurrency.webp)

| VUs | req/s | p50 | p95 | p99 | p99.9 | failed |
| --- | --- | --- | --- | --- | --- | --- |
| 50  | **148,077** | 0.23 ms | 0.70 ms | 1.15 ms | 2.30 ms | 0% |
| 100 | 150,489 | 0.47 ms | 1.38 ms | 2.42 ms | 4.98 ms | 0% |
| 200 | 142,040 | 0.82 ms | 2.41 ms | 4.18 ms | 8.77 ms | 0% |
| 400 | 111,229 | 0.83 ms | 4.12 ms | 7.10 ms | 13.62 ms | **27.9%** |
| 800 | 97,452  | 1.10 ms | 5.36 ms | 9.54 ms | 18.34 ms | **47.1%** |

Throughput peaks at **~150.5k at VU 100 this run** (the k6 generator's own ceiling on its cores —
which VU count wins the peak is host/generator noise, not a proxy change) and, **through VU 200**,
declines gracefully with latency rising in proportion and zero failures — the shape this section has
always shown.

> **A newly-added admission-control cap now surfaces at VU >= 400 (measured 2026-07-20).** This pass
> is the first since ADR 000092 landed (2026-07-15, commit `36006c7`): Plecto Proxy now refuses a new
> connection outright once a single source IP holds `MAX_CONNECTIONS_PER_IP` = **256** concurrent
> connections (`crates/server/src/conn_limit.rs`, amending [[000027]]) - a CWE-770/CWE-400 hardening
> measure so one source can no longer monopolize every connection permit. `constant-vus` runs every
> VU as a genuinely concurrent connection from the **same** loopback source IP as the proxy itself,
> so VU 400 and VU 800 are, from the cap's point of view, one source opening 400 / 800 connections -
> well past the 256 threshold the 07-11 snapshot never crossed (its highest rung was also VU 800, but
> that pre-dates the cap by ten days). The refused connections surface as k6 `status !== 200` (no
> HTTP response at all - `res.status` reads `0`), which is exactly what the `failed` column counts.
> Confirmed as the cap, not host noise or the netns sandbox: the failure fractions reproduce within
> 0.1 pp whether this phase runs inside the isolated network namespace or in the host's normal
> namespace (28.0 %/47.0 % outside vs 27.9 %/47.1 % inside, back-to-back on the same host state), and
> the threshold crossing lines up exactly with the cap - 0 % at VU 200 (under 256), a jump at VU 400
> (over 256). This is a **benchmark-harness / new-feature interaction**, not a regression in the load
> balancer itself: every other phase in this report keeps its own concurrency under 256 (see the
> [TL;DR callout](#tldr)) and reads clean. Fixing the harness (lowering `lb-sweep-step.js`'s VU
> ceiling, or driving VUs from multiple source addresses) is a follow-up, not done in this pass.

## Tail latency under open-loop load

Open-loop sends at a **constant arrival rate** regardless of how fast responses come back, so
queueing surfaces in the tail instead of being hidden — the *coordinated-omission-safe* model.

| Model | target | achieved | p50 | p95 | p99 | p99.9 | dropped | failed |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| open-loop, 0 ms backend (`plecto-loadgen`) | 105,342/s | **105,342/s** | 2.27 ms | 19.06 ms | 32.45 ms | 44.90 ms | **0** | 0% |

The auto target (70 % of the closed-loop peak, **105.3k/s** this run) is **achieved exactly** with
**zero dropped slots** under schedule-latency measurement (`plecto-loadgen openloop`, 64 workers —
well under ADR 000092's per-IP cap, unaffected; wrk2 model — see
[`bench/methodology.md`](../bench/methodology.md)). A co-resident Rust generator sustains the
auto rate without inventing its own queueing tail. p50 is a couple of milliseconds (honest schedule
lag under load); the ~32 ms p99 is the queueing tail to track.

## Round-robin distribution

![Round-robin distribution](img/rr_distribution.webp)

Over a steady window with all three upstreams healthy, **120,000** requests split **40,000 /
40,000 / 40,000** — even to a single request (33.3 % each). Round-robin holds under load.
(Re-measured 2026-07-20; `plecto-loadgen rr`, 48 workers — well under the per-IP cap, unaffected.)

## Resilience: ejection & fail-closed

A steady open-loop rate (~4k req/s, `plecto-loadgen ejection` with a 5 s unrecorded warm-up so
t=0 is already steady state) while a controller drives a fault timeline (`eject b` → `rejoin b` →
`eject all` → `restore all`) and the driver buckets each upstream's served-count and the 503/s
every second:

![Load balancing under fault injection](img/ejection_timeline.webp)

- **Even baseline.** ~4k req/s split three ways while healthy (1,333/1,334/1,333 this run).
- **Graceful ejection.** When **b** is driven unhealthy its share falls to zero within ~1 s (a
  one-second mixed transition bucket, then clean) and the survivors (a + c) absorb the full load
  **with zero failed requests** — this run they split it **evenly** (2,000/2,000), round-robin over
  two survivors landing on an even split.
- **Fail-closed, not fail-open.** With **every** instance unhealthy, Plecto Proxy returns **HTTP 503**
  promptly (no hang, no blind forward); the 503/s line jumps to the full offered rate (4,000/s here).
- **Fast recovery.** Restoring health returns instances to rotation within ~1 s (a one-second mixed
  bucket, then clean).

## Endpoint-set swap under load (ADR 000044)

A different axis than the ejection run above: instead of an existing instance's *health* flipping,
the upstream's *configured address set itself* changes — the shape a periodic-DNS re-resolution
swap takes (`resolve_interval_ms`), reproduced here via `swap-bench`'s SIGHUP reload path (the same
`ArcSwap<Endpoints>` replacement, ADR 000044). Subject: a 4-instance harness (`a, b, c, d`) starting
with the pool `[a, b, c]`; `plecto-loadgen swap` holds a steady open-loop rate while, mid-run, the
manifest is rewritten to `[a, b, d]` (dropping `c`, adding the spare `d`) and reloaded via SIGHUP —
the same fixed-rate timeline + per-instance bucketing the ejection run uses, generalized to a
changing label set (`bench/perf/run-perf.sh`'s `swap` phase).

The per-pick cost this introduces — an `ArcSwap<Endpoints>` load on every LB pick, not just on a
reload — is isolated in the companion criterion micro-benchmark,
[`pick_under_swap_churn`](#0-micro-benchmarks-in-process-criterion), under continuous concurrent
swap churn (the worst case; an unchanged tick short-circuits to one atomic load + compare and isn't
exercised there).

![Endpoint-set swap under load](img/swap_timeline.webp)

> Re-measured 2026-07-20 (v0.5.1/v0.5.2 patch confirmation): a steady ~4k req/s open-loop while, at
> t=15 s (post-warmup), the manifest is rewritten `[a, b, c]` → `[a, b, d]` and SIGHUP-reloaded
> (64 workers — well under the per-IP cap, unaffected; same shape as every prior pass).

- **Zero client-visible failures.** All 240,000 responses over the 60 s run succeeded — **0 %
  failed** — even through the swap itself. Unlike a health-based ejection, nothing here ever needs
  to fail closed: `a` and `b` are unchanged addresses, so `reconcile` reuses their `Arc`s and
  health outright (ADR 000017's reuse rule), and only `d` starts pessimistic.
- **The swap completes within one second.** The transition second (t=15) shows a brief mixed
  bucket (`a=1568, b=1566, c=13, d=853`) as in-flight requests to `c` finish and the reconciled
  pool takes over mid-second; by t=16 the split is already clean — `c=0`, and `a` / `b` / `d` even
  at ~1,333 each — the same ~1 s time constant [ejection](#resilience-ejection--fail-closed) shows,
  because both paths funnel through the same `ArcSwap<Endpoints>` replacement.
- This confirms the read the [per-pick micro-benchmark](#0-micro-benchmarks-in-process-criterion)
  predicts: the swap itself is cheap and instantaneous from the client's perspective — the cost
  ADR 000044 introduces is the small continuous per-pick `ArcSwap` load, not a client-visible
  disruption at swap time.

## TLS termination

The same single-backend pass-through, re-run with rustls TLS termination, decomposed so the cost
of each layer is separable (oha; h1 client isolates the record/handshake split from h2
multiplexing). `plain (h1)` is the [plain HTTP/1.1 ceiling](#plain-http11-ceiling)'s keep-alive row,
not re-measured here.

![TLS vs plain](img/tls_vs_plain.webp)

| Variant | req/s | p50 | p99 | isolates |
| --- | --- | --- | --- | --- |
| plain (h1)               | 248,187 | 0.19 ms | 0.47 ms | [ceiling](#plain-http11-ceiling) keep-alive |
| TLS h1, keep-alive       | 118,246 | 0.40 ms | 0.77 ms | record layer + TLS I/O path |
| TLS h1, handshake/req    | 27,718  | 1.60 ms | 4.93 ms | oha, shared `ClientConfig` — see caveat below |
| TLS (h2)                 | 98,418  | 0.47 ms | 1.05 ms | h2 multiplexing over TLS |

The decomposition is the point. This run's ordering is clean — plain h1 keep-alive (248.2k) sits
above the TLS keep-alive rung (118.2k, ~48 % of plaintext): **within-TLS ratios** are the signal:
handshake/req is **~23 % of TLS keep-alive**, and **h2 is clean** (98.4k/s, ~83 % of TLS
keep-alive, p99 1.05 ms). The TLS-terminated path remains **crypto-/TLS-I/O-bound**;
native-path optimisations don't reach it. A client that funnels many VUs over a handful of
multiplexed connections can make h2 *look* far worse (head-of-line queueing, not server work);
measuring with a connection-per-concurrency client removes that artifact.

*(Re-measured 2026-07-20 (v0.5.1/v0.5.2 patch confirmation) on **aws-lc-rs** (ADR 000051), `-c 50` —
well under the per-IP cap, unaffected. Qualitative story unchanged across every snapshot so far.)*

### Full vs resumed handshake (ADR 000052)

*(Not re-run this pass either — `bench/perf/run-perf.sh`'s `tls` phase doesn't drive this
rung automatically; the numbers below are carried over unchanged from 2026-07-05.)*

The `handshake/req` row above no longer isolates a *true* full handshake: oha shares one rustls
`ClientConfig` across connections, and against a server issuing stateless TLS 1.3 session tickets
its "cold" connections silently resume once warm. `plecto-loadgen tls --mode full|resumed` gives
each rung explicit resumption control instead:

| Client resumption | req/s | p50 | p99 | resumed |
| --- | --- | --- | --- | --- |
| full (disabled)      | 22,099 | 2.06 ms | 4.36 ms | 0 % |
| resumed (enabled)     | 29,768 | 1.54 ms | 3.26 ms | 93.0 % |

A true full handshake (22.1k/s) is **~17 % slower** than the old `handshake/req` row (26.5k/s) —
confirming it really was partly resumed. Enabling client resumption recovers **~35 % throughput**
over a true full handshake — **≈11.7 µs/connection** (45.3 → 33.6 µs). That saving is the
certificate chain + signature generation/verification, **not** the ECDHE exchange: rustls's client
hardcodes `psk_dhe_ke` and never offers plain `psk_ke` (`client/hs.rs`, RFC 8446 §4.2.9 — "such
connections don't have forward secrecy"), so every resumed handshake here still runs a fresh ECDHE
exchange for forward secrecy. The ~11.7 µs matches that: skipping only the asymmetric
sign/verify + cert bytes is a much smaller saving than skipping ECDHE too would be. The residual
7 % full handshakes are cold-cache misses under concurrent load.

---

# 2. WASM extension plane

Plecto Proxy runs each request's *decision* — auth, rewriting, rate limiting, policy — as a sandboxed
**WebAssembly Component Model filter**, not native proxy code. This measures what that costs,
changing only **how the decision runs**. The bundled `bench/harnesses/bench-server` serves a **ladder** of
routes — all forwarding to the **same** backend — so each adjacent delta isolates one cost (the full
table is in [the cost ladder](#the-wasm-cost-ladder--isolating-each-cost) below): a native `/baseline`,
a pure no-op WASM filter pooled vs fresh (`/noop-pooled`, `/noop-fresh`), and the real `filter-apikey`
pooled vs fresh (`/trusted`, `/ondemand`).

`filter-apikey` is a real `plecto:filter` component: it reads `x-api-key`, stamps
`x-authenticated-user` on a valid key and forwards, or returns a typed `short-circuit` **401** on a
missing/invalid key. It is cosign-signed and loaded through the production verify-then-load path
(fail-closed). `filter-noop` returns `continue` with **no host-API calls** — it exists only to expose
the irreducible dispatch floor.

## The WASM cost ladder — isolating each cost

![Throughput by decision path](img/wasm_throughput.webp)
![Per-request latency by decision path](img/wasm_latency.webp)

> W1 — fixed 50 connections, 0 ms backend, valid key (oha, warm-up burned in a discarded 5 s
> pre-run). Full-throttle: read these rows as **throughput ceilings**; the honest latencies are in
> the fixed-rate tail table below.

Five routes forward to the **same** backend, so each **adjacent delta isolates exactly one cost**. A
pure **no-op** WASM filter (no host-API calls) is the key addition — it separates "the WASM tax" from
"a real filter's work", which older reports conflated.

| Route | Decision path | req/s | p50 | p99 |
| --- | --- | --- | --- | --- |
| `/baseline` | native fast path (no filter) | 248,187 | 0.19 ms | 0.47 ms |
| `/noop-pooled` | a **pure no-op** WASM filter, pooled | 117,024 | 0.41 ms | 0.80 ms |
| `/noop-fresh` | the same no-op, **fresh instance / request** | 4,152 | 10.72 ms | 29.74 ms |
| `/trusted` | the real `filter-apikey`, pooled | 111,259 | 0.43 ms | 0.82 ms |
| `/ondemand` | `filter-apikey`, fresh instance / request | 4,004 | 11.32 ms | 29.13 ms |

*(Re-measured 2026-07-20 (v0.5.1/v0.5.2 patch confirmation), `-c 50` — well under the per-IP cap,
clean. `/baseline` is sourced from [ceiling.csv](#plain-http11-ceiling); the other four rungs are
measured together later in the same `all` run. This run's ordering is clean — `baseline` > every
WASM rung, no under-read artifact — so the full-throttle floor reads directly; the fixed-rate tails
below remain the honest queueing-free read.)*

- **baseline → noop-pooled** = the **irreducible extension-plane dispatch cost**. Full-throttle,
  this run shows a **~53 % throughput** cost (248.2k → 117.0k, **≈ 4.5 µs/req** inverse-throughput
  delta — matching the T1 gate's interleaved **4.41 µs** dispatch-floor invariant); the fixed-rate
  tails put the queueing-free floor at **+0.15 ms p50 / +0.31 ms p99**. Every WASM filter pays this
  floor.
- **noop-pooled → noop-fresh** = the **per-request instantiation cost**, cleanly isolated from any
  host work: throughput collapses **~28×** (117.0k → 4.2k). This is what pooling buys.
- **noop-pooled → trusted** = a **real filter's own work** on top of the no-op (header parse +
  host-KV lookup + counter): **−4.9 % (~0.44 µs this run)** — matching the T1 gate's **0.44 µs**
  apikey-cost invariant almost exactly (and inside the historical interleaved A/B band, 0.3–1.2 µs).
  The apikey filter is cheap; the dispatch floor still dominates it.
- **noop-fresh and ondemand are the same order of magnitude** (4.2k vs 4.0k req/s), confirming
  instantiation dominates the fresh path — the filter's per-request work is noise next to re-paying
  `init` (~42.5 µs, this pass's fresh criterion figure) every request.

### The same ladder at one fixed rate — honest tails

> W1b — every rung offered the **same** fixed **2,402 req/s** this run (60 % of the slowest rung's
> ceiling, `/ondemand` at 4,004/s), 50 connections, oha `-q` + `--latency-correction`
> (coordinated-omission-safe). Identical offered load, so the latency columns are directly
> comparable — but this rate sits noticeably closer to the fresh path's ~4k/s knee (see the
> mechanism note below) than most prior snapshots' fixed rate did, and the fresh rungs' tails show it.

| Route | achieved | p50 | p90 | p99 |
| --- | --- | --- | --- | --- |
| `/baseline` | 2,402/s | 0.28 ms | 0.47 ms | 0.78 ms |
| `/noop-pooled` | 2,402/s | 0.43 ms | 0.64 ms | 1.09 ms |
| `/trusted` | 2,402/s | 0.46 ms | 0.67 ms | 1.06 ms |
| `/noop-fresh` | 2,402/s | 1.18 ms | 2.87 ms | 25.79 ms |
| `/ondemand` | 2,402/s | 1.17 ms | 5.64 ms | 25.76 ms |

At a rate every rung sustains, the pooled dispatch floor costs **+0.15 ms p50 / +0.31 ms p99** over
native and the real pooled filter **+0.18 ms p50 / +0.28 ms p99** — sub-millisecond to ~1 ms at p99,
consistent with prior snapshots. The fresh rungs live at **p99 ~25.8 ms** this run — a striking jump
from the last snapshot's 4.4–6.4 ms at a nearby rate (2.15k/s), but fully consistent with the
already-documented knee mechanism below: the fresh path's tail is known to be sharply
rate-dependent near ~4k/s (documented p99 4.7 ms at 2k/s vs ~650 ms at 4.2k/s), and this pass's
derived rate (2.40k/s, from a slightly higher `/ondemand` floor) sits further up that same steep
curve than 2.15k/s did — not a new phenomenon, just a different point on it. Per-request
instantiation is still not a tail you can operate behind near or above that knee.

> **The fresh tail is a kernel-side knee, not CPU queueing (measured 2026-07-06).** A fresh
> instance is an mmap at instantiate and an munmap at drop, every request
> (`Allocation::OnDemand`); munmap serializes on the process's `mmap_lock` and IPIs every core
> running the process (TLB shootdown). `/proc/interrupts` deltas during fixed-rate runs: the fresh
> rung takes **~31–35 TLB shootdowns/req vs ~0.3 pooled — ~100×**. The resulting tail is sharply
> rate-dependent — p99 **1.4 ms at 1k/s, 4.7 ms at 2k/s, ~650 ms at 4.2k/s, ~1.2 s at 6k/s** (with
> shootdowns/req itself doubling as concurrency rises) — a knee near ~4k/s, roughly *half* the
> rung's closed-loop ceiling. The 07-05 snapshot's W1b fixed rate (60 % of that run's slowest
> ceiling, 4,189 req/s) landed almost exactly on that knee, which is why its fresh rows' absolute
> tails were chaotic across earlier snapshots (440 → 738 ms between runs; 83 ms vs 648 ms at the
> same rate on the same host minutes apart) while the pooled rows stayed stable; snapshots at
> 07-09 (2.9k/s), 07-11 earlier pass (1.9k/s) and 07-11 release confirmation (2.15k/s) sat clear of
> the knee and read correspondingly clean fresh tails — **2026-07-20's 2.40k/s sits far enough up
> the same curve that the fresh p99 (~25.8 ms) is visibly worse than those three**, illustrating
> just how steep this region is: a ~12 % rate increase (2.15k → 2.40k/s) produced a ~4–6× tail
> increase, not a proportional one. Avoiding precisely this
> per-request mmap/munmap churn is why wasmtime's pooling
> allocator pre-maps slots and batches decommits — the trusted path rides that. Stated portably:
> fresh-per-request has a clean-tail operating ceiling around ~2k/s on this host, and that — not
> the ~40 µs — is the pooling decision's real justification.

**The µs/req deltas are the invariants to track for regressions, not the percentages** (which widen or
shrink whenever the *baseline* moves). These macro deltas **reconcile with the in-process
[micro-benchmarks](#0-micro-benchmarks-in-process-criterion)** — with one disclosed asymmetry: this
run's clean full-throttle ordering gives a real baseline→noop-pooled inverse-throughput delta of
**~4.5 µs/req** (4.03 → 8.55 µs); criterion clocks the pooled per-request call at ~2.24 µs of that,
leaving **~2.3 µs** as the `spawn_blocking` handoff (sync wasmtime, `!Send` store) that a route
with no filters skips entirely. The fresh ~42.5 µs, by contrast, is the *uncontended* cost — criterion
instantiates sequentially, so it never pays the `mmap_lock` contention or cross-core shootdowns the
concurrent macro run exposes (the knee above). The layers agree once that kernel-side term is named.

## Short-circuit: rejecting bad traffic at the edge

![Accept vs reject latency](img/wasm_shortcircuit.webp)

> W2 — fixed 2000 req/s, 15 ms backend, ~90 % valid / ~10 % bad keys (k6). 90,785 accepted, 29,241
> rejected (76.4 % / 24.4 % this run — see the caveat below).

| Path | p50 | p95 | p99 |
| --- | --- | --- | --- |
| accept (200, forwarded) | 16.40 ms | 17.23 ms | 17.60 ms |
| reject (401, short-circuited) | 0.20 ms | 0.42 ms | 0.62 ms |

Accepted requests cost the 15 ms backend plus the small pooled-filter + proxy overhead. Rejected
requests are decided **at the edge in ~0.20 ms** and never reach the upstream: traffic that gets a
response at all is shed **~82x faster** than good traffic is forwarded, and is harmless to the
backend it would otherwise hit. (Filter faults or deadline overruns **fail closed** - 502/504 - exercised by the test suite,
not this benchmark.)

> **This run's accept/reject split does not match the ~90 %/10 % key mix - likely the same per-IP
> cap as the sweep finding above (measured 2026-07-20).** The script's own key roll is a
> client-side `Math.random()` draw that should be ~10 % bad regardless of server behavior, so a jump
> to 24.4 % rejected is not the filter rejecting more valid keys - direct verification (`oha -c 50`
> against `/trusted` with only a valid key, 10 s, this same host state) shows **0 non-200 responses**
> out of 28,565. The far more likely explanation: this scenario's k6 `constant-arrival-rate` executor
> pre-allocates **300 VUs** (`bench/k6-wasm/mixed.js`), above ADR 000092's 256-connections-per-source-IP
> cap; a VU whose connection is refused gets `res.status === 0`, which this script's `else` branch
> (anything not `=== 200`) counts as "rejected" alongside genuine 401s. The **latency figures above
> remain valid** for the requests that did get a real response; the **accept/reject count split is
> not comparable** to prior snapshots' ~90/10 until the harness separates connection failures from
> genuine short-circuits.

## v0.3.0 response ladder + compression

The 2026-07-20 `all` pass measured every route with ADR 000073/074/075 **present but
unused**. This section fills the gap: what those features cost **when exercised**. Opt-in phase
(`bash bench/perf/run-perf.sh v03`) — not part of `all`, so a full refresh stays heavy while this
row can be re-run alone. Same generators and CO-safe tail pattern as
[the WASM cost ladder](#the-wasm-cost-ladder--isolating-each-cost).

### Response-context read vs `replace` (ADR 000073)

> R1 — fixed 50 connections, 0 ms backend, tiny response (oha). Lean `filter-resp` (no host-API
> calls): `/resp-ctx` always *reads* the as-forwarded request snapshot then `continue`;
> `/resp-replace` does the same read then `replace`s with a synthesised **418** (marker header
> `x-plecto-resp-replace`). Control is same-session `/noop-pooled` (ignores the snapshot).

| Route | Decision path | req/s | p50 | p99 | µs/req |
| --- | --- | --- | --- | --- | --- |
| `/noop-pooled` | on-response unused params → continue | 122,531 | 0.39 ms | 0.77 ms | 8.16 |
| `/resp-ctx` | read as-forwarded snapshot → continue | 120,995 | 0.40 ms | 0.77 ms | 8.27 |
| `/resp-replace` | read + `replace` (418, 23 B body) | 114,137 | 0.42 ms | 0.85 ms | 8.76 |

*(Re-measured 2026-07-20 via `v03`. Same-process adjacent deltas — do not splice onto an older
`wasm` CSV's noop row.)*

- **noop-pooled → resp-ctx ≈ +0.10 µs/req** this run — the cost of *using* the ADR 000073 request
  snapshot on `on-response` (path length + header scan), with the same continue/forward path. This
  pass's delta is much smaller than the prior snapshot's (~0.90 µs) and is now at the edge of
  full-throttle measurement noise (the ceiling itself moves a few µs/req run to run — see the
  caveat on baseline→noop-pooled's own volatility earlier in this report); track the fixed-rate p50
  below alongside this figure, not this number alone.
- **noop-pooled → resp-replace ≈ +0.60 µs/req** net at full throttle — `replace` synthesises a
  tiny body and **drops** the upstream payload on the wire (verified: `Content-Encoding` N/A,
  23-byte 418). That wire-shape change can *under-* or *over-read* replace's guest/host work
  relative to resp-ctx run to run; do **not** read either pass's number as a pure CPU claim. The
  regression signal for replace is "still within ~1 µs of the pooled no-op on this host," not
  a %-of-baseline, and that holds this pass too.
- Fixed-rate tails at **68 481/s** (60 % of this ladder's slowest ceiling; oha `-q`
  `--latency-correction`): all three rungs hold the offered rate; **p50 stays ~0.97–1.05 ms**. p99
  at this rate is host-noise-dominated on this session (35–82 ms) — same caveat as other
  high-rate fixed runs near a host knee; prefer µs/req + p50 for this row.

### Native response compression (ADR 000074 / 000075)

> R2 — same oha shape; backend **4 096 B** repeating `text/plain` (above the 1024-byte
> `min_length` default). Both routes send `Accept-Encoding: gzip`. `/baseline` has no
> `[route.compression]` → identity; `/compress` pins `algorithms = ["gzip"]`. Wire check:
> identity 4096 B vs gzip **45 B** + `Content-Encoding: gzip` + `Vary: accept-encoding`.

| Route | Transform | req/s | p50 | p99 | µs/req |
| --- | --- | --- | --- | --- | --- |
| `/baseline` | identity (AE advertised, opt-in off) | 228,452 | 0.20 ms | 0.52 ms | 4.38 |
| `/compress` | gzip (level 5, ADR 000075 defaults) | 154,668 | 0.31 ms | 0.61 ms | 6.47 |

- **baseline → compress ≈ −32.3 % ceiling / +2.09 µs/req** for this highly compressible 4 KiB
  filler — matching the prior snapshot's +2.11 µs/req almost exactly. Real HTML/JSON ratios and CPU
  will differ; this row is a **regression floor** for the opt-in path, not a capacity guide for
  production payloads. RFC 9411 §7.3-style: one object size, sustainable throughput, method disclosed.
- Fixed-rate at **92,800/s** (60 % of compress ceiling): both hold the rate; p50 ≈ 0.96–0.97 ms;
  p99 again host-noise-band at this offered load — µs/req from the ceiling table is the
  durable signal.

**Criterion note (not re-run here).** Day-to-day criterion absolute drift (±10–20 %) is expected
without a locked governor. To attribute ADR 000073 contract-surface cost in-process, use a
same-host baseline pair — `cargo bench -p plecto-host -- --save-baseline pre-adr73` on the
pre-landing commit, then `--baseline pre-adr73` after — per
[criterion baselines](https://bheisler.github.io/criterion.rs/book/user_guide/command_line_options.html)
([`bench/methodology.md`](../bench/methodology.md)).

## Outbound ext_authz (ADR 000036)

A filter can call an external authorization service per request over the lent, SSRF-guarded outbound
capability (`filter-extauthz`). Per-request cost is three parts, only the first two Plecto Proxy's: the
WASM tax (the same [cost ladder](#the-wasm-cost-ladder--isolating-each-cost)), the outbound gate
(allowlist + SSRF classification — nanoseconds, negligible), and the network round-trip to the authz
endpoint, which dominates and is the *operator's* latency, not Plecto Proxy's.

Load numbers are deferred rather than faked: the SSRF guard blocks loopback by design, so a hermetic
mock authz needs a non-loopback endpoint (environment-specific), and the connector currently opens a
new connection per call (pooling is a follow-up). The capability itself is verified end-to-end by
the host's `outbound-http` test suite (allowlist deny + DNS-rebinding SSRF block).

## Host-enforced rate limiting

Plecto Proxy's rate limiter is a **host-native token bucket** (ADR 000026): the bucket spec
(`capacity` / `refill_tokens` / `refill_interval_ms`) is configured **in the operator's manifest**,
not by the filter — an untrusted filter passes only `(key, cost)` and so cannot widen its own limit.
The refill + counting stay host-side (the WASM boundary is not crossed on the hot path); the filter
only decides *whether* to consult the limiter and *on what key*. Driven through `bench/harnesses/bench-server`
(`filter-hello`, pooled); a `429` carries `retry-after-ms`.

> **Scope: single node.** Every run below drives one `plecto` instance. The bucket is **node-local**
> ([ADR 000053](../docs/ADR/000053.md)) — the enforcement and fairness numbers describe what one
> instance guarantees, not a multi-replica fleet. Behind a load balancer fanning out to N replicas,
> the fleet's effective allowed rate scales with N unless the front LB pins a key to one replica; see
> the [hardening guide](../docs/hardening.md) for the operational formula.
>
> **Scope: in-memory state backend.** These numbers (and every host-state number in this report) run
> the default `[state] backend = "memory"`. With `backend = "redb"`, the backend write happens
> **inside the process-wide quota lock** (`charge_and_apply` — the price of closing the CWE-770
> accounting race), so every host-kv / counter / rate-limit call across all filters serializes
> behind that disk write. Persistent-state throughput under concurrency is structurally different
> and **unmeasured here**.

### Overhead — the cost of consulting the bucket

> R1 — 50 VUs, 0 ms backend, a **never-deny** bucket spread across 1000 keys (k6). `/baseline` vs
> `/ratelimit`.

| Route | req/s | p50 | p99 |
| --- | --- | --- | --- |
| /baseline (no filter) | 149,803 | 0.22 ms | 1.28 ms |
| /ratelimit (bucket) | 106,027 | 0.39 ms | 1.16 ms |

The rate-limited route adds **~2.8 µs/req** over the no-filter baseline (~29 % of its throughput;
p99 stays in the same ~1.2 ms band — the µs/req is the inverse-throughput delta at 50 VUs, well
under ADR 000092's per-IP cap, unaffected). That is the whole hot-path tax with no rejections — the
filter dispatch floor (the same one the
[WASM ladder](#the-wasm-cost-ladder--isolating-each-cost) isolates) plus the host-native bucket
consult, including the per-call host-state quota check (ADR 000027) that keeps a multi-tenant
filter's bucket count bounded.

### Enforcement — does it actually hold the rate?

![Rate-limit enforcement](img/ratelimit_enforce.webp)

> R2 — a **tight** bucket (refill 1000 tok/s, burst 2000), offered **5000 req/s** open-loop at one
> key for 30 s (k6).

| offered | allowed (200) | shed (429) | accept p99 | 429 p99 |
| --- | --- | --- | --- | --- |
| 5,000/s | **1,033/s** | 59.6%* | 1.91 ms | 0.70 ms |

Offered 5× over the limit, the **allowed throughput still converges correctly to the bucket's refill
rate** (≈ 1.0k/s — the configured 1000 tok/s plus the burst amortised over the run, unaffected —
computed only over successfully-connected requests) — **the same 1,033/s as prior snapshots**,
falling out of the bucket's own math (refill vs offered rate), not host timing.

> **\* The 59.6 % shed figure is unreliable this pass — measurement gap, not a proxy change
> (measured 2026-07-20).** `bench/k6-wasm/ratelimit-enforce.js` only counts `status === 200` as
> `accepted` and `status === 429` as `limited`; anything else (including a refused connection,
> `status === 0`) is silently dropped from both counters. This scenario's `preAllocatedVUs` is
> `max(200, RATE/10)` = **500** for a 5,000 req/s offer — almost double ADR 000092's 256-per-IP cap,
> and every VU shares the generator's one loopback source IP. Over the 30 s window this run's
> `accepted + limited` totals **76,804**, against **150,000** attempted (5,000/s × 30 s) — **48.8 %
> of attempts never produced an HTTP status at all** and are simply missing from the accounting, not
> folded into either bucket. The **79.3 %** shed figure from the 07-11 snapshot (measured before
> ADR 000092 landed) remains the trustworthy reference for "how much this bucket sheds at 5×
> offered" until the harness accounts for refused connections as their own category.

### Fairness — one key cannot starve another

![Rate-limit fairness](img/ratelimit_fairness.webp)

> R3 — same tight bucket; two keys concurrently: a **hot** key offered 4000/s and a **light** key
> offered 500/s (k6).

| key | offered | allowed (200) | shed (of accounted) |
| --- | --- | --- | --- |
| hot | 4,000/s | 1,033/s | 54.5%* |
| light | 500/s | 145/s* | 0%* |

State is **per key**: the hot key's *allowed* rate still converges cleanly to its own refill rate
(1.0k/s — the qualitative fairness claim, "a hot key cannot exceed its own bucket," holds regardless
of the caveat below). The **shed percentages and the light key's absolute throughput are not
reliable this pass**: both scenarios run inside the *same* k6 process (one loopback source IP), and
`hot`'s own `preAllocatedVUs` (`max(200, 4000/10)` = **400**) alone exceeds ADR 000092's 256-per-IP
cap — before `light`'s own 100 VUs are even added. The light key's low absolute allowed rate (145/s
against a 500/s offer, despite **zero** 429s) is the visible symptom: it isn't being throttled by
the bucket (0 % shed is a genuine, valid read of the bucket's own fairness — it never denies the
light key), but it also isn't reaching its offered rate, most likely because it is starved of
connections by `hot`'s saturation of the shared per-IP budget, not by anything rate-limit-specific.
Read this section's **qualitative** claim (light is never rejected by the bucket) as solid; read the
**quantitative** allowed-rate numbers for both keys as contaminated by the same connection-cap
interaction as [enforcement](#enforcement--does-it-actually-hold-the-rate) above, pending a harness
fix.

## Request body handling

The request-side **body hook** (`on-request-body`, ADR 000025) follows a *buffer-then-decide* model:
for a filtered route carrying a body, the host buffers it (bounded — 16 MiB cap, fail-closed 413),
runs the filter's `on-request-body`, and forwards the possibly-transformed body — or short-circuits
before upstream. `filter-hello` uppercases the body (a real transform) or 403s on a `deny-body`
marker. A bodyless request, a filter-less route, and — since ADR 000038 — a route whose filters are
**all header-only** (none exports `on-request-body`) keep the zero-copy streaming path: the host
decides from the component's exports whether any filter reads the body, and buffers only then.

![Request body hook](img/body.webp)

> B — 50 VUs, POST a `SIZE`-byte body at 1 KB / 100 KB / 1 MB (k6), to `/body` (filter-hello buffers +
> transforms), `/body-headeronly` (a header-only filter — body streams through, ADR 000038), and
> `/baseline` (no filter). `MALLOC_ARENA_MAX=4`, the shipped allocator default (ADR 000038).

| size | route | req/s | throughput | p99 |
| --- | --- | --- | --- | --- |
| 1 KB   | /baseline        | 144,836 | 148 MB/s  | 1.16 ms |
| 1 KB   | /body            | 76,191  | 78 MB/s   | 1.44 ms |
| 1 KB   | /body-headeronly | 81,328  | 83 MB/s   | 1.39 ms |
| 100 KB | /baseline        | 45,839  | 4694 MB/s | 4.06 ms |
| 100 KB | /body            | 18,645  | 1909 MB/s | 5.64 ms |
| 100 KB | /body-headeronly | 40,399  | 4137 MB/s | 4.39 ms |
| 1 MB   | /baseline        | 6,104   | 6400 MB/s | 31.9 ms |
| 1 MB   | /body            | 2,106   | 2208 MB/s | 39.8 ms |
| 1 MB   | /body-headeronly | 6,076   | 6371 MB/s | 31.2 ms |

A filter that **reads** the body pays for it, growing with payload: **~47 % throughput at 1 KB** (the
buffer + WASM transform dominate the small request), **~59 % at 100 KB**, **~66 % at 1 MB** (a
full-body copy + uppercase per request). A **header-only filter takes the zero-copy bypass** — the
body never enters guest memory: at 1 MB it lands **within ~0.5 % of `/baseline`** (ADR
000038, within noise); at 100 KB the gap widened to **~12 %** this pass (`VUS=50`, well under the
per-IP cap — host noise on this run, not a new mechanism); at 1 KB it reads well below baseline —
the ordinary **WASM dispatch floor** on a tiny request, not a body cost. RSS at 1 MB × 50 VUs (fresh
proxy per route, `MALLOC_ARENA_MAX=4`): **~106 MB `/baseline` · ~187 MB `/body` · ~101 MB
`/body-headeronly`**
(`data/body_rss.csv`). The export-presence bypass keeps a header-only route near baseline. The buffer
stays bounded (16 MiB cap, fail-closed 413) for the filters that do read the body. The remaining
buffered-path copy is the target of a future `stream<u8>` increment (ADR 000020); a per-request
time-series / allocator-sweep decomposition lives in `bench/perf/mem_matrix.py`.

## Footprint

Idle resident set and the marginal cost of an open connection (`bench/harnesses/bench-server`):

| Metric | Value |
| --- | --- |
| idle RSS | ~46 MB |
| RSS holding keep-alive connections | ~52 MB (256 conns — see note) |
| marginal bytes / connection | ~24.9 KB |

*(Re-measured 2026-07-20. This phase's `plecto-loadgen hold --conns 1000` itself hit ADR 000092's
256-per-IP cap and logged `hold: 256 connections open` instead of the requested 1,000 — the same
admission-control interaction as the [sweep](#throughput--latency-vs-concurrency) and
[rate-limit](#enforcement--does-it-actually-hold-the-rate) findings above. The script's own
bytes/conn arithmetic divides by the requested 1,000 regardless of how many actually connected,
so it is not used here; recomputed over the actual 256 held connections, the marginal cost is
**~24.9 KB/conn** — consistent with the historical ~24.8 KB/conn figure. The absolute idle RSS
(~46 MB) is unaffected by the cap and matches the prior ~45 MB.)*

---

# 3. Realistic & protocol coverage

## Weighted request mix — with its own baseline

> M1 — open-loop 20k req/s, a weighted blend across routes on one gateway (k6): read-heavy, partly
> edge-checked (per-tenant rate-limit keys, 200 tenants, never-deny bucket), occasional writes,
> rare large payloads. Paired with a **read-only control at the same arrival rate** — 100 % plain
> reads — so the per-class deltas are attributable to the traffic *blend*, not the offered load.

| Profile | Class (share) | route | p50 | p99 | p99.9 |
| --- | --- | --- | --- | --- | --- |
| read-only (control) | read 100 % | GET `/baseline` (1 KB) | 0.12 ms | 2.74 ms | 10.61 ms |
| mix | read 60 % | GET `/baseline` (1 KB) | 0.11 ms | 5.48 ms | 16.96 ms |
| mix | auth read 25 % | GET `/ratelimit` (tenant key) | 0.14 ms | 5.57 ms | — |
| mix | write 10 % | POST `/body` (1 KB) | 0.16 ms | 5.89 ms | — |
| mix | large 5 % | POST `/body` (100 KB) | 0.30 ms | 5.88 ms | — |

Both profiles reach ~20k/s offered (zero 429s from the never-deny bucket; a small `dropped_iterations`
count — 1,245 read-only / 2,936 mix, well under 0.1 % of total iterations — see the caveat below).
The pairing is still the point: at the same rate, **the blend costs the plain reads +2.7 ms at p99**
(2.74 → 5.48 ms) this run — head-of-line pressure from the body classes — and the classes order
almost exactly as their work predicts (read ≤ auth read < write ≈ large, all p50s sub-millisecond).
A single-endpoint test hides all of this; the control run keeps it honest.

> **This pass's tails read markedly lower than 07-11's (p99 ~5.5–5.9 ms here vs ~16–20 ms
> previously) — plausible cause, not confirmed (measured 2026-07-20).** Unlike the enforcement /
> fairness / short-circuit scripts above, `bench/k6/weighted-mix.js`'s `record()` folds **every**
> response's `res.timings.duration` into its latency Trends regardless of status — it does not
> branch on `status === 200`. This scenario's `preAllocatedVUs` is `max(500, RATE*0.02)` = **500**
> for a 20,000 req/s offer, again above ADR 000092's 256-per-IP cap. A connection refused by the cap
> typically resolves near-instantly (no handshake, no proxy work), so if any such pseudo-responses
> are mixed into this run's percentile pool, they would drag the reported tail *down*, not up —
> opposite in direction from the sweep/enforce findings, but the same root interaction. This report
> cannot distinguish "the proxy genuinely got faster" from "some fast connection-refusals are mixed
> into the distribution" without re-instrumenting the script to separate them (a harness follow-up);
> treat this pass's absolute tail figures as directional, not a confirmed improvement, until then.
> The **pairing methodology and class ordering** (the section's actual point) are unaffected either
> way, since both profiles would be diluted equally.

## HTTP/3

The fast path terminates **HTTP/3 over QUIC** (ADR 000016; `tls-http` serves h1/h2/h3 on one port). A
functional check confirms it end-to-end:

```
curl --http3-only https://…/api/hello  ->  status=200 http_version=3
```

*(Re-confirmed 2026-07-20.)*

A **rigorous, coordinated-omission-safe H3 *load* benchmark is deferred**: oha and k6 have no native
HTTP/3, and a correct tail needs an H3-capable open-loop generator (**h2load** with
`--npn-list h3`, or an equivalent H3 load tool). Rather than publish process-spawn-bound `curl`-loop numbers, the H3 load figure
stays absent until that tooling is in place — server support is verified, not throughput.

## WebSocket Upgrade tunnel (ADR 000048)

Plecto Proxy's HTTP/1.1 Upgrade path (ADR 000048): a route declaring `[route.upgrade] protocols =
["websocket"]` forwards the client's handshake (controlled re-issue — hop-by-hop stripping stays
the default for every other route), and on the upstream's 101 the proxy splices the two connections
into an opaque bidirectional byte tunnel — the same post-upgrade relay shape used by typical
L7 `Upgrade` / TCP tunnel modes. This is a **different load shape**
than every other scenario in this report: a long-lived, stateful connection instead of a
short-lived request, so it exercises axes nothing else here does — connection-permit accounting
(the circuit breaker / least-request in-flight counters follow the tunnel for its whole lifetime,
not just the handshake) and an activity-based idle timeout, rather than throughput-per-request.

`bench-server`'s `/ws` route tunnels to a dedicated mock upstream that completes the RFC 6455
handshake and echoes every frame; `plecto-loadgen`'s `ws` subcommand drives three sub-scenarios
(`bench/perf/run-perf.sh`'s `ws` phase):

- **Handshake rate** — open-loop paced Upgrade attempts/sec (the 101 handshake runs through the full
  filter chain like any other request, so it pays the same per-request cost the rest of this report
  measures — only the post-101 tunnel is new).
- **Tunnel footprint** — RSS held by 1,000 concurrently open (idle) tunnels, the long-lived-connection
  analogue of [Footprint](#footprint)'s keep-alive connection measurement.
- **Echo throughput** — sustained request/response frames per held tunnel, at two payload sizes
  (1 KB / 64 KB), closed-loop per connection (the same concurrency model oha's `-c N` uses).

> Re-measured 2026-07-20 (v0.5.1/v0.5.2 patch confirmation): `bash bench/perf/run-perf.sh all` / `ws`.
> Handshake (paced 500/s, 64 workers) and echo (50 conns) both stay well under ADR 000092's per-IP
> cap and show no failure symptom; the tunnel-footprint `hold --conns 1000` also completed cleanly
> at the full 1,000 this pass (unlike the generic [Footprint](#footprint) phase's `hold`, which was
> capped to 256 — the two use the same loadgen subcommand shape but evidently not identical
> conditions; reported as measured, not reconciled further this pass).

| Scenario | Result |
| --- | --- |
| Handshake rate | 10,000/10,000 Upgrades succeeded at the paced 500/s target — **0 % failed** over 20 s |
| Tunnel footprint | idle RSS 77.3 MB → 88.9 MB with 1,000 held tunnels — **~11.9 KB/tunnel** |

![WebSocket echo throughput](img/ws_echo.webp)

| Payload | messages/s | throughput | p50 | p99 |
| --- | --- | --- | --- | --- |
| 1 KB  | 92,652 | 95 MB/s    | 0.50 ms | 1.86 ms |
| 64 KB | 84,752 | 5,554 MB/s | 0.54 ms | 1.42 ms |

The handshake rate holds at 100 % of target with zero rejections — the Upgrade path costs nothing
beyond the ordinary per-request floor. Tunnel footprint (~11.9 KB/tunnel) is in the same order as a
held keep-alive HTTP connection ([Footprint](#footprint): ~24.9 KB/conn) — a tunnel is not
meaningfully heavier to hold open than an ordinary idle connection, only longer-lived. Echo
throughput at 1 KB (92.7k msg/s) exceeds 64 KB (84.8k msg/s) as expected — the larger payload's
messages/s falls roughly in proportion to its size, while aggregate byte throughput rises (95 →
5,554 MB/s), consistent with a per-message dispatch floor that amortizes better over larger frames.
(Shape stable vs prior snapshots; absolute msg/s moves within host noise — this pass reads lower on
1 KB than 07-11, plausibly the accumulated load of this run's earlier handshake/footprint
sub-scenarios sharing one long-lived proxy session, `SHARE_PROXY=1`.)

*(A per-request small-frame delayed-ACK stall — the exact Nagle signature the
[connection-churn history](#plain-http11-ceiling) already found once — appeared during this
scenario's development on the mock upstream's accept-side socket; disabling Nagle there fixed it.
The numbers above are post-fix. The idle timeout (default 5 min, ADR 000048) and its interaction
with the breaker permit / least-request in-flight counters are exercised by the host's own test
suite, not this benchmark; a transfer-bytes / tunnel-duration metric is a documented observability
gap (ADR 000048's re-examine condition (d)) this report will pick up once it lands.)*

---

## Methodology — why the numbers look the way they do

(Builds on [Measurement setup](#measurement-setup) above — pinning, warm-up, open/closed-loop — with
what that setup buys.)

- **Track the invariant, not the headline.** The WASM tax and the rate-limit tax are ~µs/req (not a
  %), rate-limit enforcement converges to the configured refill rate, fairness is per-key isolation,
  resilience is ~time-constants, and round-robin is exact — these hold across hosts and generators,
  so a change in them is a real regression. A change in absolute peak throughput is usually just the
  host or the generator.
- **Benchmarks find bugs.** The body scenario surfaced a delayed-ACK stall from Nagle on the upstream
  sockets (no `TCP_NODELAY`); disabling Nagle there — standard for L7 proxies — removed a ~40 ms p99
  cliff on streamed bodies. Disclosing *how* a number was produced is the point.
- **Two layers that must agree.** In-process criterion micro-benchmarks isolate per-function cost
  deterministically; the open-loop macro scenarios measure it end-to-end. Micro-cost × calls-per-request
  should explain the macro delta — the WASM ladder is the worked example — so a divergence between the
  layers is a bug in one of them, not noise. The micro layer itself is split in two: criterion for
  wall-clock direction, and instruction counts (gungraun/callgrind, feature `instruction-bench`) as
  the frequency-invariant judge for "did the contract surface get more expensive?" — see
  [Reproducing](#reproducing) and `bench/methodology.md` § Measurement tiers.
- **The local per-change gate.** `bash bench/perf/run-perf.sh gate` re-measures exactly the
  invariants this report tracks (interleaved for a confidence half-width) and machine-checks them
  against `bench/perf/gate_tolerances.toml` — the bands are tracked in-repo, so a deliberate
  performance change is reviewed as a diff. `all` stays the human-read release snapshot.
- **CI regression gate.** Per-PR, two layers with different verdict policies (`bench.yml`): the
  criterion micro-benchmarks stay *informational* (hosted-runner wall-clock is noisy-neighbour
  bound, ~2–3 % CV, so a tight threshold would false-fail), while the gungraun instruction-count
  benches are *judged* — a soft limit (`ir=5%`) against the baseline saved from main pushes.
  Instruction counts don't inherit the runner's frequency/thermal noise, which is what makes a
  machine verdict meaningful on shared VMs. The heavy k6/oha macro suite never runs in CI — the
  local T1 `gate` covers per-change macro invariants.
- **Prior art.** Disclosing open- vs closed-loop and corrected latency is standard in tools such as
  `wrk2` and k6. This report follows that spirit using only its own measurements.

## Reproducing

The tracked, in-repo subjects and the runbook that produces every CSV here:

```bash
# Build the release examples first (the runbook does not build). bench-server/swap-bench live
# outside plecto/ (bench/harnesses/), so they need --features bench-harnesses.
cargo build --release -p plecto-server --features bench-harnesses \
  --example load-balancing --example bench-server --example tls-http --example swap-bench

# T1 — the per-change regression gate (~6-7 min): interleaved invariant deltas judged against
# bench/perf/gate_tolerances.toml, written to performance/data/gate.csv. Exit 0 = in band.
bash bench/perf/run-perf.sh gate          # or: just gate

# T2 — the full release-snapshot suite (~22 min at the report-tier windows). Phases:
#   quick gate ceiling sweep openloop rr ejection swap wasm v03 tls h3 ws footprint ratelimit body mix all
bash bench/perf/run-perf.sh all           # or: just report

# T3 — opt-in deep phases (not part of `all`): v0.3.0 response-context / replace + compression:
bash bench/perf/run-perf.sh v03           # or: just deep v03

# T0 — a fast local sanity check (~1 min, oha only, no k6/Docker, no tracked CSV):
bash bench/perf/run-perf.sh quick

# In-process micro-benchmarks, two layers (see bench/methodology.md § Measurement tiers):
# 1) criterion (wall-clock; drifts with the unpinned governor — read direction, not absolutes).
#    For ADR-sized contract changes, prefer a named baseline on the pre-change commit:
#      git checkout <pre-adr73> && cargo bench -p plecto-host -- --save-baseline pre-adr73
#      git checkout <post>      && cargo bench -p plecto-host -- --baseline pre-adr73
cargo bench -p plecto-control -p plecto-host -- --save-baseline main   # on the base branch
cargo bench -p plecto-control -p plecto-host -- --baseline main        # on a change, to read the deltas
# 2) instruction counts (gungraun/callgrind; frequency/thermal-invariant — the deterministic
#    judge for "did the contract surface get more expensive?"). Needs valgrind + a
#    version-matched `cargo install gungraun-runner`; feature-gated so plain `cargo bench` skips it:
#    NB: gungraun needs `=`-attached values — a space-separated `--save-baseline main` is parsed
#    as a positional benchmark filter and silently runs nothing — and baseline names allow only
#    [A-Za-z0-9_] (use `pre_adr73`, not `pre-adr73`).
cargo bench -p plecto-host    --features instruction-bench --bench wasm_inst     -- --save-baseline=main
cargo bench -p plecto-control --features instruction-bench --bench fastpath_inst -- --save-baseline=main
#    ...then on a change: the same commands with `-- --baseline=main` (soft limits are also
#    available, e.g. `-- --callgrind-limits=ir=5%`).

# Optional live dashboard (images are a one-time setup pull; the load stays on loopback):
INFLUX=1 bash bench/perf/run-perf.sh all     # http://localhost:3000/d/plecto-lb-k6

# The underlying examples (default ports overridable with PLECTO_PROXY_ADDR):
cargo run --release -p plecto-server --example load-balancing   # LB fast path
BACKEND_LATENCY_MS=0 cargo run --release -p plecto-server --features bench-harnesses --example bench-server   # WASM plane + rate-limit + body hook + WS
cargo run --release -p plecto-server --example tls-http          # TLS termination
cargo run --release -p plecto-server --features bench-harnesses --example swap-bench          # endpoint-set swap under load

# TLS full-vs-resumed handshake rungs (ADR 000052; the `tls` phase doesn't drive this yet — the
# cert.pem lives in tls-http's temp dir, printed nowhere, so find it under /tmp and pass it as --ca):
plecto-loadgen tls --mode full    --target https://localhost:PORT/api/hello --ca CERT.pem --out performance/data/tls_full.csv
plecto-loadgen tls --mode resumed --target https://localhost:PORT/api/hello --ca CERT.pem --out performance/data/tls_resumed.csv
```

The k6 scenarios live in `bench/k6/` and `bench/k6-wasm/`; the round-robin counter, the open-loop
fault/swap timelines, and the WebSocket handshake/hold/echo scenarios are `plecto-loadgen`
subcommands (`bench/loadgen/`, built lazily by the runbook). The open-loop driver records
schedule-latency into an HDR histogram (fixed footprint at any rate/window) and dumps the FULL
distribution alongside the summary (`--hist-out`, written to `performance/data/openloop_hist.csv`
by the runbook) — so a p99 move can be attributed to a second mode appearing vs one mode's tail
stretching, without a re-run. Charts are regenerated from the measured CSVs:

```bash
python3 performance/plot.py     # reads performance/data/*.csv -> performance/img/*.webp
```

(`matplotlib` brings `numpy` + `Pillow`; Pillow supplies the WebP encoder. The benchmark *method* —
the runbook, scenarios, the Rust loadgen, plotting — is tracked, as are the rendered charts and this
report; the measured CSVs are regenerable working data and stay untracked, like `bench/`'s raw run
artifacts. See `bench/plan.md`.)

## Non-goals

- Not a sizing or capacity guide.
- Not a comparison against other proxies, gateways, or Wasm runtimes.
- Not representative of production hardware, real networks, or non-trivial upstream work.

## References

- Gil Tene, *coordinated omission* — summarized in ScyllaDB's [On Coordinated Omission](https://www.scylladb.com/2021/04/22/on-coordinated-omission/).
- [k6 executors](https://grafana.com/docs/k6/latest/using-k6/scenarios/executors/) — closed-loop (`constant-vus`) vs open-loop (`constant-arrival-rate`) models.
- [oha](https://github.com/hatoo/oha) — the single-connection-pool HTTP load generator used for the ceiling, WASM overhead, and TLS runs.
- [criterion.rs](https://bheisler.github.io/criterion.rs/book/) — the in-process micro-benchmark harness (LB pick, route match, WASM per-request cost) and its baseline-comparison regression gate.
- Open-loop HTTP/1–2–3 load generators suitable for an HTTP/3 *load* benchmark (deferred here).
- [h2load](https://nghttp2.org/documentation/h2load-howto.html) — nghttp2's load generator; supports HTTP/3 (`--npn-list h3`) with qlog output, and a candidate for the deferred H3 load run.
- [wrk2](https://github.com/giltene/wrk2) — constant throughput with corrected latency recording.
- [Wasmtime](https://docs.wasmtime.dev/) — the pooling allocator and epoch interruption behind pooled vs on-demand filter instances.
- [WebAssembly Component Model](https://component-model.bytecodealliance.org/) — the `plecto:filter` contract is a Component Model world.
- [RFC 6455](https://www.rfc-editor.org/rfc/rfc6455) — the WebSocket protocol `bench-server`'s `/ws` mock upstream and `plecto-loadgen`'s `ws` subcommand implement (handshake + frame codec) to drive the Upgrade tunnel scenario.
