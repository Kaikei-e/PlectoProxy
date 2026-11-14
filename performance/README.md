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

- **Core isolation by pinning (and co-resident in-process upstream in T1/T2).** The proxy is pinned at exec time so every thread inherits the mask
  and the per-thread affinity is verified after start; the default proxy/generator split is by whole
  physical cores (`bench/perf/cpu_split.py`). On the reference host (`Intel Core i7-13700K`: 8 Raptor Cove P-cores as adjacent SMT pairs on CPUs `0-15`, 8 Gracemont E-cores on CPUs `16-23`), the proxy runs on `PROXY_CPUS=0-11` (6 P-cores / 12 threads) and the load generator runs on `GEN_CPUS=12-23` (2 P-cores / 4 threads + 8 E-cores / 8 threads). The generator therefore never steals a core from the proxy, though generator-heavy phases can be bounded by the hybrid P+E generator core set. **Important co-residency note:** the four standard benchmark harnesses (`bench-server`, `load-balancing`, `tls-http`, `swap-bench`) spawn their mock HTTP upstream **in-process** on the same OS process and Tokio runtime (`PROXY_CPUS=0-11`). Consequently, standard T1/T2 absolute throughput, CPU/req, and RSS figures (`body_rss.csv`, `footprint.txt`, `ws_footprint.csv`) measure **`proxy + in-process mock upstream` together** (adjacent route deltas cancel the shared upstream work, whereas absolute numbers and percentages include it). By contrast, `bench/perf/arena_sweep.py` (T3) isolates proxy, upstream, and generator into three separate OS processes on disjoint physical cores (`PROXY_CPUS=0-11`, `UP_CPUS=16-19`, `GEN_CPUS=12-15,20-23`).
- **Allocator configuration.** Every proxy launch runs with the shipped arena cap (default **32**, revised from the historical cap of 4; all 4 standard harnesses call `cap_malloc_arenas` before runtime initialization). Legacy behavior is reproducible in production with `PLECTO_MALLOC_ARENA_MAX=4` (or in benchmarks via `BENCH_MALLOC_ARENA_MAX=4`, as `run-perf.sh` scrubs inherited `PLECTO_*` variables). Note that the historical `MALLOC_ARENA_MAX=4` body RSS table (`~114 / ~196 / ~122 MB`, `2026-09-25`), the `2026-10-24` (`v0.15.2`) default-32 `body_rss.csv` measurement (`~122 / ~296 / ~117 MB`, `+51 %` on `/body` — traded for `+32 %` auth and `+18 %` rate-limit throughput per [ADR 000118](../docs/ADR/000118.md)), the `2026-11-14` (`v0.16.1`) measurement (`~136 / ~293 / ~132 MB`), and the current `2026-11-14` (`v0.16.2`) default-32 `body_rss.csv` table (`~138 / ~290 / ~121 MB`, `+48 %` on `/body` vs the cap-4 `~196 MB`) are all **co-resident** mid-load samples (`proxy + in-process upstream` under `phase_body`), whereas isolated **proxy-only** RSS (`~256 MiB` peak / `~186–199 MiB` post-load 15 s settled at cap 32, vs `~167 / ~140 MiB` at cap 4) is measured separately by `arena_sweep.py` (T3); see [`bench/methodology.md`](../bench/methodology.md#2026-09-27--アリーナ既定値を判断するための分離計測t3), [`performance/HISTORY.md`](HISTORY.md#2026-09-28-allocator-arena-default-transition-4--32), and [ADR 000118](../docs/ADR/000118.md).
- **Run data retention & host fingerprinting.** Raw per-round JSON, proxy logs, CSV copies and a host
  fingerprint (`host.txt`, allowlisted for publication: no hostname, kernel build string or per-CVE
  mitigation lines) are kept under `performance/data/runs/<run-id>/` and `just perf-archive` tars
  one for attaching to a GitHub Release.
- **Historical comparability note.** Numbers measured before this change (the 2026-09-25 snapshot and
  earlier) were taken with partial pinning and uncapped arenas outside the body phase, so footprint
  KB/conn and gate spreads are not directly comparable.
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

> **Latest snapshot (`2026-11-14` on `v0.16.2` / `HEAD` `0d7bb08`, implementing [ADR 000124](../docs/ADR/000124.md); Rust `1.100.0`, Wasmtime `50.0.1` + dual WASI `0.2`/`0.3` `@0.3.0`; macro tables & charts from `RUN_DIR: 20261114T132246Z-0d7bb08`, pre-run `contended=0` with `loadavg1=3.05, proxy_busy=0.1028`, inline header hooks [ADR 000119](../docs/ADR/000119.md), default arena cap 32 [ADR 000118](../docs/ADR/000118.md)):**
> - **In-process micro-benchmarks (`0d7bb08`, `taskset -c 0-11`, re-measured `2026-11-14`):** gungraun 0.20 / callgrind teardown-excluded instruction counts ([ADR 000113](../docs/ADR/000113.md), [ADR 000122](../docs/ADR/000122.md)) drop sharply on the extension plane after ADR 000124's synchronous zero-WASI call, borrowed `HttpRequestV02V03Ref<'a>` lowering, conditional wall clock, and lazy telemetry: `on_request/trusted_pooled` **19,201** instructions (**−31.3 %** vs `v0.16.1`'s `27,955`; est. cycles `32,094`, LL hits `513` down **−62.3 %** from `1,362`), `on_request/untrusted_fresh` **73,776** instructions (**−27.2 %** vs `v0.16.1`'s `101,377`; `3.84×` pooled, est. cycles `123,740`, LL hits `3,028` down **−50.6 %** from `6,130`), and unchanged `lb_pick` **227 / 339 / 278** instructions on round-robin / P2C / Maglev (est. cycles `423 / 566 / 487`). Criterion wall-clock (`minimal` default features matching the release binary and `bench-server`): `on_request/trusted_pooled` **1.03 µs** mean (`1,033.2 ns`, slope `1.037 µs`), `on_request/untrusted_fresh` **15.66 µs** mean (`15,660.7 ns`, `15.16×` pooled), `filter_load/trusted_cold` **9.93 ms** mean (`7.98 ms` median), `find_route` **36.0 → 43.4 → 149.4 ns** (`1 → 16 → 64` routes), `normalize_path` **20.5 ns** clean / **78.4 ns** dot-segments / **11.7 ns** query; with `outbound-http,outbound-tcp,fat-guest,streaming-body` enabled (comparable to `v0.15.2`), `on_request/trusted_pooled` drops to **1.31 µs** mean (**−37.5 %** vs `v0.15.2`'s `2.09 µs`) and `untrusted_fresh` to **18.59 µs** (**−34.8 %** vs `28.53 µs`; `filter_load/trusted_cold` `8.60 ms`). All five `bench/perf/ipc_check.py` checks pass (`IPC 3.44 / 0.87 / 1.63 / 1.75 / 2.41 <= 5.0`).
> - **Full suite & gate (`2026-11-14`, `v0.16.2` / `HEAD` `0d7bb08`, `PROXY_CPUS=0-11`, `GEN_CPUS=12-23`, T2 `RUN_DIR: 20261114T132246Z-0d7bb08`, T1 pooled gate `20261114T135754Z-0d7bb08`):** All 14 T2 `all` phases returned `ok`, and every functional and load invariant held: `rr_spread_req` **0** (`40,000 × 3`), `ejection` `300,000` responses over 75 s with `60,160` failed (**20.05 %**) during total ejection (`1.0 s` transition, `0` stray failed), `swap` **0 % failed** (`240,000` reqs over 60 s, 1 s transition `c → d` at `t = 15 s`), `enforce_allowed_ratio` **0.9999** (`1,100.0/s`, `78.0 %` shed over 10 s in T1; `1,033.3/s`, `79.3 %` shed, `0` status-less over 30 s in T2), `fairness` light key **500.0/s (`0 %` shed, `0` status-less)**, `wasm_mixed` **90.0 % / 10.0 %** (`108,047` / `11,987`, `0` status-less), `footprint` **25.9 KB/conn** (`25,887 B`), `ws_footprint` **21.4 KB/tunnel** (`21,365 B`, 250 tunnels), `ws_handshake` `10,000 / 10,000` (`500.0/s`, `0` failed over 20 s), HTTP/3 `200`, and T1 fixed-rate 2,000 rps tails `pooled` **+0.0077 ms** / `apikey` **+0.0048 ms** / `respctx` **+0.0009 ms** (all `pass`). On the ADR 000124-recalibrated `bench/perf/gate_tolerances.toml` bands (where fast-side `lo` bounds moved to `0.0 / -0.2 / -0.6` while keeping `hi` unchanged at `2.1 / 1.7 / 2.4`), Run 1 (`20261114T135107Z-0d7bb08`, `loadavg1=1.58`, `proxy_busy=0.1139`, `contended=0`, `n = 8`) returned `8 pass, 0 fail, 2 inconclusive` (`dispatch_floor_us` `0.3279 ± 0.3348`, `ratelimit_tax_us` `−0.3495 ± 0.8076`, straddling fast edges `0.0` / `−0.6`), and pooling Run 2 (`20261114T135754Z-0d7bb08`, `loadavg1=3.18`, `proxy_busy=0.0991`, `contended=0`) via `GATE_POOL_FROM` (**`n = 16`**, `performance/data/gate.csv`) returned **PASS — `10 pass / 0 fail / 0 inconclusive` (`exit 0`)**: `dispatch_floor_us` **`0.5909 ± 0.2398 µs`**, `apikey_cost_us` **`0.4349 ± 0.2712 µs`**, `ratelimit_tax_us` **`−0.1069 ± 0.4492 µs`**.
> - **Prior generation judged against (`2026-11-14`, `v0.16.1` / `HEAD` `b8f042e`, `RUN_DIR: 20261114T032413Z-b8f042e` & `20261114T030016Z-b8f042e` under external host load `load average ≈ 4–13`):** Rust `1.100.0` toolchain A/B left `on_request/trusted_pooled` at **27,955** Ir (`+0.28 %` vs `1.99.1`), `untrusted_fresh` at **101,377** Ir (`−0.06 %`), and `lb_pick` at `227 / 339 / 278` Ir. All 14 T2 phases returned `ok` (plain keep-alive `210,479`, `/noop-pooled` `174,197`, `/trusted` `143,763`, `/noop-fresh` `13,638`, `/ondemand` `13,548` rps; sweep peak `134,327`; openloop `94,028/s` p50 `0.051 ms` p99 `4.783 ms`), and the `n = 24` pooled T1 gate returned `9 pass / 0 fail / 1 inconclusive` (`dispatch_floor_us` **`1.0944 ± 0.1898 µs`**, `apikey_cost_us` **`0.8428 ± 0.2117 µs`**, `ratelimit_tax_us` `0.5913 ± 0.2086 µs` on fast edge `0.5`). Across `v0.16.1 → v0.16.2` ([ADR 000124](../docs/ADR/000124.md)), `dispatch_floor_us` dropped `1.09 → 0.59 µs`, `apikey_cost_us` dropped `0.84 → 0.43 µs`, `ratelimit_tax_us` moved `0.59 → −0.11 µs` (near k6 generator ceiling noise floor), plain keep-alive ceiling rose `+11.4 %` (`210.5k → 234.4k`), `/trusted` rose `+19.6 %` (`143.8k → 171.9k`), and fresh-instance throughput rose `+44.6–50.7 %` (`13.5–13.6k → 19.6–20.6k`).
> - **Older snapshots & full per-run verdict logs (`v0.16.1`, `v0.15.5`, `2026-10-25` ADR 000122, `v0.15.2`, `2026-09-29` inline hooks … `2026-07-02`):** see [`performance/HISTORY.md`](HISTORY.md).
>
> **µs/req deltas are what to track across snapshots**, not raw throughput — and the tracked
> invariant set is machine-checked by the T1 gate (`bash bench/perf/run-perf.sh gate`, bands in
> `bench/perf/gate_tolerances.toml`). The gate produces a three-valued verdict: `pass` (exit code 0,
> every invariant's interval inside its band), `fail` (exit code 1, interval outside band; dominates inconclusive), or
> `inconclusive` (exit code 2, interval straddles a band edge => re-run with `GATE_POOL_FROM=<that RUN_DIR>`, which **pools** the earlier rounds with the new ones instead of replacing them). In `gate.csv`, `ci_half` is the **two-sided 95 % Student-t half-width** of the mean of the n per-round paired deltas, $t_{0.975,n-1}\cdot s/\sqrt{n}$, with `n` in the last column. Gate runs before ADR 000122 recorded the 2–3-round sample half-range `(max - min) / 2` in that column instead — at $n=2$ exactly $1\,\text{SE}$ (50 % coverage), not a 95 % interval — so `±` figures quoted from those runs are not comparable to new ones.
>
> **Metric definition — µs/req**: µs/req = 1e6 / rps, i.e. inverse throughput. In a closed loop with C connections it equals mean latency / C (Little's law); it is neither latency nor CPU time. Note that criterion's per-call µs (single-thread, in-process) is a different quantity.

**Load-balancing fast path** (plaintext HTTP/1.1, 3 upstreams, trivial 0 ms backend; k6 / loadgen / oha):

- Closed-loop throughput peaks at **~125.4k req/s** (`125,442`, VU 100 this run; vs `~134.3k` on `v0.16.1` and `~147.2k` on idle `2026-10-24`, governed by the k6 generator ceiling and run-to-run host variance) with **p99 ≈ 1.58–4.94 ms** and
  zero failures through VU 200. **VU 400/800 show 29.0 % / 48.9 % "failed"** — this is **not** proxy
  overload: it is the sweep's own concurrency (400/800 simultaneous connections, all from the
  generator's single loopback IP) crossing ADR 000092's **256-connections-per-source-IP**
  admission cap, which those two rungs cross deliberately — see
  [the sweep section](#throughput--latency-vs-concurrency). Under the
  cap (VU ≤ 200) the curve still declines gracefully with no cliff.
- Open-loop at the auto **87,808/s** (70 % of closed-loop peak `125,441` req/s) **achieves 87,808/s exactly** (`7,902,720` reqs over 90 s) with
  **p50 0.047 ms, p95 1.633 ms, p99 24.447 ms, p99.9 73.791 ms, 0 dropped, 0 % failed** — schedule-latency
  (`plecto-loadgen openloop`, 64 workers — well under the per-IP cap, unaffected). While p50 improves to **0.047 ms** (`0.051 ms` on `v0.16.1`, `0.050 ms` idle on `2026-10-25`), the upper tail percentiles are stretched relative to `v0.16.1` (`p95 1.151 ms / p99 4.783 ms / p99.9 17.599 ms` at `94,028/s`) and the idle `2026-10-25` run (`p95 0.508 ms / p99 2.09 ms / p99.9 4.57 ms` at `103,026/s`); `loadavg1` rose during the back-to-back T2 run (`3.05` pre-run → `5.48` at end of `mix`), and the auto target differs (`87,808/s` vs `94,028/s` vs `103,026/s`), so this is not a fixed-rate comparison — cross-snapshot tail comparisons should pin the rate with `OPENLOOP_RATE`. The historical `2026-10-24` run read p50 `0.84 ms` because the generator's 1 ms timer granularity delayed sends — see [open-loop tail](#tail-latency-under-open-loop-load).
- Round-robin across three upstreams is **even to within zero requests** (33.3 % each, 40,000 / 40,000 / 40,000, 120,000 reqs).
- **Resilience is as designed**: ejecting one upstream drops its share to zero in ~1 s and the
  survivors absorb the load with **no client-visible errors**; a *total* outage **fails closed
  with HTTP 503** and the pool **recovers within ~1 s** of health returning.
- TLS termination (**aws-lc-rs**, ADR 000051): within-TLS, keep-alive **~188.7k** (**80.5 %** of the
  plaintext ceiling `234.4k`, vs `165.4k` in `v0.16.1`) vs handshake/req **~24.8k** (**13.1 %** of keep-alive, `24,765/s`) and h2 **~153.7k** (**81.5 %** of
  keep-alive, vs `138.4k` in `v0.16.1`) — the path is **crypto-/TLS-I/O-bound**, ordering clean. A resumption-isolated
  measurement (carried from 07-05, not re-run this pass) puts a **true full handshake at ~22.1k/s**
  vs **~29.8k/s resumed (93 %)** — see [TLS](#tls-termination).
- A **kept-alive** connection (**RR**) serves **~234.4k req/s** this run (`+11.4 %` over `v0.16.1`'s `210.5k`, reflecting ADR 000124's unfiltered zero-copy path); forcing a **TCP
  handshake per request** (**CRR**, 76.4k req/s vs `85.7k` in `v0.16.1`) costs **~67.4 % throughput and +1.45 ms p99** — connection
  reuse is still load-bearing (see [the plain HTTP/1.1 ceiling](#plain-http11-ceiling)).

**WASM extension plane** (the cost of running a decision as a sandboxed component; oha / k6):

- A **cost ladder** isolates each cost by adjacent delta (oha, `-c 50` — well under the per-IP cap,
  clean). With ADR 000124 on top of inline header hooks ([ADR 000119](../docs/ADR/000119.md)) and Wasmtime 50.0.1 ([ADR 000120](../docs/ADR/000120.md)),
  this run's full-throttle ceiling is clean (`baseline` **>** every WASM rung): **baseline → noop-pooled costs ~15.5 % throughput** full-throttle (`234.4k → 198.1k`,
  **≈ 0.78 µs/req** inverse-throughput delta vs `0.99 µs/req` in `v0.16.1`; T1 pooled gate `dispatch_floor_us` **`0.5909 ± 0.2398 µs`**, `n = 16`, pass); the **fixed-rate tail** (11,757 req/s, 60 % of `/ondemand`)
  puts the monotone p50 floor at **+0.014 ms p50** (**+0.046 ms p50** for the real filter: `0.0941 < 0.1082 < 0.1404 ms` across `/baseline < /noop-pooled < /trusted`, with monotone p99s `0.6744 < 0.8088 < 1.1044 ms`; `+0.0077 ms p50` in the T1 gate's 2,000 req/s tail, informational p99 `+0.0118 ms`). A **real filter's own work**
  (`filter-apikey` on top of the pooled no-op) is **≈ 0.77 µs** (`−13.2 %`, `198.1k → 171.9k` vs `1.22 µs` in `v0.16.1`; T1 pooled gate `apikey_cost_us` **`0.4349 ± 0.2712 µs`**, `n = 16`, pass); running that filter **fresh-per-request** instead of pooled
  costs **~8.8–9.6×** throughput (while fresh ceilings themselves jumped `+44.6–50.7 %` from `13.5–13.6k` to `19.6–20.6k`) — the price of re-paying `init` every request.
- These macro deltas **reconcile with the re-measured Criterion & gungraun [micro-benchmarks](#0-micro-benchmarks-in-process-criterion)** (`0d7bb08`, both layers dropping in lockstep under ADR 000124)
  in direction and order of magnitude, and with the T1 pooled gate (`bash bench/perf/run-perf.sh gate`, bands in
  `bench/perf/gate_tolerances.toml` — all 10 judged invariants **PASS** at `n = 16`; see the [TL;DR](#tldr)).
- **v0.3.0 response / compression (opt-in `v03` phase, measured 2026-08-15 on v0.9.0, pre-ADR-000119/000124, not re-run this pass):** reading the
  as-forwarded request snapshot on `on-response` costs **≈ +0.22 µs/req** over the same-session v0.9.0 `/noop-pooled` baseline (`112,955 rps`, `8.85 µs/req`; on `v0.16.2`, the T1 gate tracks the 2,000 rps fixed-rate tail delta `respctx_tail_p50_ms` directly at **`+0.0009 ms`**, `pass` inside `-0.05..0.25 ms`, matching the `v0.15.2` idle spread — see
  [the section itself](#v030-response-ladder--compression)); gzip on a 4 KiB compressible body costs
  **≈ −31 % ceiling / +2.3 µs/req** vs the same body uncompressed, within ~0.25 µs across snapshots.
- A rejected request (**HTTP 401 short-circuit**) is decided in **~0.087 ms (p50) / ~0.352 ms (p95) and never reaches the
  backend** — bad traffic is shed **~185× faster at p50 (~48× at p95)** than good traffic is forwarded (`16.101 ms` p50 / `16.97 ms` p95) through a 15 ms
  backend. The split matches the design mix — **90.0 % accepted /
  10.0 % rejected (`108,047` / `11,987`), zero status-less responses** — see
  [the section](#short-circuit-rejecting-bad-traffic-at-the-edge).

**Host-enforced rate limiting** (token bucket, spec host-configured in the manifest; k6):

- With ADR 000124 on top of inline header hooks ([ADR 000119](../docs/ADR/000119.md)), the T1 pooled gate (`20261114T135754Z-0d7bb08`, `n = 16`, **PASS**) measures `ratelimit_tax_us` at
  **`−0.1069 ± 0.4492 µs/req`** over a no-filter
  baseline when the bucket never denies (`VUS=50`, well under the per-IP cap, clean) — well inside the ADR 000124-recalibrated `−0.6..2.4 µs` band (`enforce_allowed_ratio` passed at `0.9999`), where k6's `/baseline` arm runs near its own generator ceiling (`111.7k` rps mean, `87.2k–123.1k` across the 16 rounds vs `112.2k` mean on `/ratelimit`, while oha reaches `234.4k` on `/baseline`), so the closed-loop filter cost is compressed into between-arm generator noise (`0 ± 0.45 µs`; deltas from different generators cannot be added or subtracted). The gate also records proxy CPU-µs/req at a fixed 20k/s (`ratelimit_cpu_us_per_req`, info row; **+15.3 ± 1.0 µs** over `32.3 µs/req` on the idle `2026-10-25` pooled runs, and **−6.28 ± 18.69 µs** over a high-variance `75.27 ± 25.24 µs` `/baseline` on `v0.16.2`, `n = 8`, where two windows in Run 1 dropped 149–197/s below 20k/s and host/clock variance dominated the CPU delta) — see [rate-limit overhead](#overhead--the-cost-of-consulting-the-bucket).
- Offered **5× over the configured rate**, the **allowed throughput converges to the
  bucket's discrete refill schedule** (**1,033.3/s** over 30 s for a `capacity = 2,000`, `1,000 tok / 1,000 ms` bucket: initial `2,000` burst + `29 × 1,000` discrete 1 s refills = **31,000** allowed) and the run accounts for
  **every** offered request — 31,000 allowed + 119,005 shed = 150,005 accounted for over 30 s, **0
  status-less** — putting the shed fraction at **79.3 %**, the same figure every post-repair
  snapshot has measured.
- Buckets are **per key**: a hot key offered 4× its limit is throttled to its own refill rate
  (1,033.3/s, 74.2 % shed) while a **light key on the same filter passes completely untouched —
  500.0/s offered, 500.0/s allowed, zero 429s** — no cross-key starvation, now measured rather than
  inferred.

**Request-body hook** (buffer-then-decide, ADR 000025; export-presence zero-copy bypass, ADR 000038; k6):

- A filter that **reads** the body (`/body`, filter-hello) costs **~8.1 % throughput at 1 KB** (down from `22.2 %` in `v0.16.1`, as the lower fixed dispatch/filter cost narrows the gap on small payloads) and
  scales with payload: **~53.2 % at 100 KB**, **~64.8 % at 1 MB**, versus the streaming passthrough
  (`VUS=50`, well under the per-IP cap, clean).
  A **header-only filter** (`/body-headeronly`) **streams the body through**: at 1 MB (`+0.6 %`) and 100 KB (`−0.3 %`) it sits **within ±1 % of `/baseline`** (ADR 000038, within noise),
  and at 1 KB (`+2.1 %`, `123.2k` vs `120.7k`, single-window variance vs `−10.9 %` in `v0.16.1`) the lowered dispatch floor no longer exposes a wide 1 KB gap in un-interleaved k6 runs.
- Co-resident `proxy + in-process upstream` RSS at 1 MB × 50 VUs (`body_rss.csv`, one mid-load sample under `phase_body`, default arena cap 32 per [ADR 000118](../docs/ADR/000118.md)): **~138 MB `/baseline` (`137,564 kB`) · ~290 MB `/body` (`290,264 kB`) · ~121 MB
  `/body-headeronly` (`120,908 kB`)** (up on `/body` from `~114 / ~196 / ~122 MB` under legacy arena cap 4; isolated proxy-only RSS via `arena_sweep.py` is **`~256 MiB` peak / `~186–199 MiB` settled**). The header-only bypass stays near baseline; the buffer stays bounded (16 MiB
  cap, fail-closed 413).

## Scope & honesty notes

- **Reference host disclosed; treat absolute figures as host-specific.** All runs execute on a single commodity reference host (`Intel Core i7-13700K`, 8 Raptor Cove P-cores / 16 threads on CPUs `0-15` + 8 Gracemont E-cores / 8 threads on CPUs `16-23`, 24 logical CPUs, default `powersave` governor, loopback, co-resident). While absolute figures depend on this reference host and its unpinned clock/governor (and `GEN_CPUS=12-23` mixes 2 P-cores + 8 E-cores), the benchmark structure isolates relative deltas, ratios, and time-constants on the same run.
- **Single host and pinning method.** Measured on the same single commodity host over loopback with
  the core-pinning setup described above (`PROXY_CPUS=0-11`, `GEN_CPUS=12-23`). At `1061a79` (`v0.15.2`), two pre-T2 T1 `gate` runs returned `INCONCLUSIVE (exit 2)` solely on `ratelimit_tax_us` (`1.30 ± 0.52` and `1.12 ± 0.38 µs` as old half-ranges, straddling the edge `0.9` of `0.9..2.4`), while the post-T2 confirmation run (`20261024T042039Z`) and the pre-tag run (`20261023T182045Z-57e6f8e-dirty`) reported `10 pass` under the half-range rule then in force. Re-judged from their archived rounds with the 95 % t interval, all four (and the 2026-10-21 run) are inconclusive on `ratelimit_tax_us` against that `0.9..2.4` band (three of the five against today's `0.5..2.4`): two k6 rounds cannot bound a ~1.2 µs delta tightly.
  Ratios, shapes and time-constants — what this report actually tracks — remain the durable signal.
- **Generator-bound where noted.** The closed-loop sweep tops out near the *generator's* ceiling on
  its cores, not the proxy's: absolute peaks move with host/generator noise (this run ~125.4k k6
  peak vs ~234.4k oha ceiling keep-alive — different generators, different ceilings). The sweep
  curve's *shape* is the signal, not
  its absolute peak (below the per-IP admission cap — see [the sweep section](#throughput--latency-vs-concurrency) for VU ≥ 400).
  Open-loop tails use `plecto-loadgen` so they are no longer k6-VU-bound.
- **Trivial upstreams** (tiny static responses, 0 ms latency by default, running in-process inside the four standard T1/T2 harnesses and isolated in a separate OS process in T3 `arena_sweep.py`) deliberately isolate
  **proxy + LB + filter overhead** rather than backend work. A 15 ms synthetic backend is used
  where realistic proportions matter (WASM short-circuit); a sized-body backend for the body sweep.
- The LB figures are **plaintext HTTP/1.1**, except the dedicated [TLS run](#tls-termination).
- **No comparative claims.** Mature proxies are referenced only for shared methodology, never ranking.
- Charts compiled with Flint (flint-chart) from the CSVs in performance/data; an optional InfluxDB + Grafana stack (`INFLUX=1`) provides
  live dashboards during k6 runs (its images are a one-time setup pull; the load stays on loopback).

---

# 0. Micro-benchmarks (in-process, criterion)

A deterministic, network-free layer (`cargo bench`, criterion, `taskset -c 0-11` on `HEAD` `0d7bb08` / tag **v0.16.2**) that isolates the **per-function** cost
of the hot path with low noise — complementary to the end-to-end macro scenarios below, and the basis
for the CI regression gate (`--save-baseline` / `--baseline`). Micro-cost × calls-per-request should
roughly explain the macro deltas, and it does (the WASM ladder is the worked example).

**Fast path** (`crates/control/benches/fastpath.rs`, Criterion mean point estimates):

| bench | cost | note |
| --- | --- | --- |
| LB pick — round-robin | 25.8 → 26.5 → 32.4 ns (3 → 8 → 32 instances) | ~O(1) over the eligible set (slope `25.8 / 26.5 / 32.6 ns`) |
| LB pick — P2C weighted-least-request | 35.9 → 38.7 → 60.1 ns (3 → 8 → 32 instances) | two eligibility passes + the sampled compare (slope `35.8 / 38.4 / 60.3 ns`) |
| LB pick — weighted Maglev | 21.3 / 21.6 / 21.5 ns (3 / 8 / 32 instances) | + one table lookup |
| LB pick under swap churn (`pick_under_swap_churn`) | 100.5 → 134.2 → 55.8 ns (3 → 8 → 32 instances) | round-robin pick while a background thread continuously calls `update_endpoints` (ADR 000044) — the per-pick `ArcSwap<Endpoints>` load cost under worst-case concurrent churn |
| route match (`find_route`) | 36.0 → 43.4 → 149.4 ns (1 → 16 → 64 routes) | pre-sorted `route_order` + early break ([ADR 000124](../docs/ADR/000124.md); down from `47 → 242 ns` on `v0.9.0`) |
| ingress path normalization | 20.5 ns clean / 78.4 ns dot-segments / 11.7 ns with-query | ADR 000027 / single-pass scan ([ADR 000124](../docs/ADR/000124.md)); a clean path is borrowed, no allocation |

All three LB algorithms are covered here; the macro suite only load-tests round-robin. On this run `n=3` (~100 ns) and `n=8` (~134 ns) `pick_under_swap_churn` cells read slower than `n=32` (~56 ns) — under continuous churn over a small instance slice, the background thread's frequent `update_endpoints` writes contend with reader loads for the same cache lines; reported as measured, not smoothed.

**Extension plane** (`crates/host/benches/wasm.rs`, Criterion mean point estimates):

| bench | `minimal` default features (matches `bench-server`) | `outbound-http,outbound-tcp,fat-guest,streaming-body` | isolates |
| --- | --- | --- | --- |
| `on_request` — pooled instance | **1.03 µs** (`1,033.2 ns`, slope `1.037 µs`) | **1.31 µs** (`1,306.5 ns`, slope `1.312 µs`; `v0.15.2` `2.09 µs`) | dispatch + call (init amortized) |
| `on_request` — fresh instance / request | **15.66 µs** (`15,660.7 ns`, slope `15.71 µs`) | **18.59 µs** (`18,591.3 ns`, slope `18.74 µs`; `v0.15.2` `28.53 µs`) | + per-request instantiation (the pool's value) |
| cold `load` (verify + instantiate + init) | **9.93 ms** mean (`7.98 ms` median) | **8.60 ms** mean (`8.52 ms` median; `v0.15.2` `7.89 ms`) | cosign signature + SBOM verification + compile |

*(Note: Criterion clocks wall-clock duration per call in a single-thread, in-process benchmark. This per-call µs is a different quantity from the macro inverse-throughput µs/req = 1e6 / rps; see [metric definition](#1-load-balancing-fast-path).)*

With [ADR 000124](../docs/ADR/000124.md) (`HEAD` `0d7bb08`, tag **v0.16.2**, Wasmtime `50.0.1`), zero-WASI filters run via synchronous `InstancePre::instantiate` + `TypedFunc::call` with borrowed `HttpRequestV02V03Ref<'a>` lowering and lazy telemetry under both builds; disabling `Config::concurrency_support(false)` on the default build (which omits async WASI features) cuts `on_request/trusted_pooled` further to **1.03 µs** (vs **1.31 µs** with async WASI features compiled in — still **−37.5 %** below `v0.15.2`'s `2.09 µs` under that same feature set). The **`15.16×`** (`minimal`) / **`14.23×`** (`capabilities`) pooled→fresh gap here is the same gap the [macro ladder](#the-wasm-cost-ladder--isolating-each-cost)
shows end-to-end (~8.8–9.6× in this T2 snapshot, with the HTTP layer and allocator contention around it) —
the two layers agree in direction and order of magnitude, so a divergence between them would be a
real bug. *(All Criterion figures above re-measured 2026-11-14 at `HEAD` `0d7bb08`, tag **v0.16.2**, `taskset -c 0-11`. Day-to-day drift of ±10–20 % is expected per [`bench/methodology.md`](../bench/methodology.md) § Measurement tiers.)*

**The frequency-invariant twin — instruction counts** (gungraun 0.20 / callgrind, feature
`instruction-bench`; `crates/host/benches/wasm_inst.rs`, `crates/control/benches/fastpath_inst.rs`).
Instruction counts don't move with clock, thermals or a noisy neighbour, which is what makes them
the *judged* layer in CI (`ir=5%` soft limit against main's baseline) while criterion stays
informational:

| bench (`HEAD` `0d7bb08` / `v0.16.2`, Rust 1.100.0, teardown-excluded) | instructions | L1 hits | LL hits | RAM hits | total read/write | est. cycles |
| --- | --- | --- | --- | --- | --- | --- |
| `on_request/trusted_pooled` (`wasm_inst`) | **19,201** | 28,864 | 513 | 19 | 29,396 | 32,094 |
| `on_request/untrusted_fresh` (`wasm_inst`) | **73,776** | 107,130 | 3,028 | 42 | 110,200 | 123,740 |
| LB pick — round-robin (`fastpath_inst`) | **227** | 318 | 0 | 3 | 321 | 423 |
| LB pick — P2C weighted-least-request (`fastpath_inst`) | **339** | 461 | 0 | 3 | 464 | 566 |
| LB pick — weighted Maglev, source IP (`fastpath_inst`) | **278** | 382 | 0 | 3 | 385 | 487 |

- **Evolution of the instruction floor across `v0.9.0 → v0.16.2`:** In v0.9.0, `on_request(fx: Fx)` and `lb_pick(group: Arc<UpstreamGroup>)` dropped their fixtures inside the callgrind-measured block (`~508k / ~564k` in `wasm_inst`, `2,114 / 2,377 / 2,277` in `fastpath_inst`, implying an impossible IPC > 30 against Criterion). [ADR 000113](../docs/ADR/000113.md) and [ADR 000122](../docs/ADR/000122.md) moved teardown out of the measured region; dropping unused `wasmtime` `cache` reduced `wasm_inst` in `v0.15.5` (`28,290 / 101,983`), and the `v0.16.1` / Rust `1.100.0` pass stood at `27,955 / 101,377` and `227 / 339 / 278`. On **`v0.16.2` (`0d7bb08`, [ADR 000124](../docs/ADR/000124.md))**, removing fiber/async-store orchestration, `HttpRequest`/`Vec<Header>` cloning, eager `SystemTime::now()` / span building, and extra pool wakeup cut **`on_request/trusted_pooled` by −31.3 % (`27,955 → 19,201` Ir; LL hits `1,362 → 513`, −62.3 %)** and **`on_request/untrusted_fresh` by −27.2 % (`101,377 → 73,776` Ir; LL hits `6,130 → 3,028`, −50.6 %)**, while `lb_pick` remained bitwise unchanged at `227 / 339 / 278` Ir.
- **Reconciling `wasm_inst` and `fastpath_inst` with Criterion (`0d7bb08`, `minimal` build, [`bench/perf/ipc_check.py`](../bench/perf/ipc_check.py) `ok`):** On `v0.16.2`, running both layers on the same `0d7bb08` tree gives **IPC 3.44** on `on_request/trusted_pooled` (`19,201 Ir / (1,033.2 ns × 5.4 GHz)`), **IPC 0.87** on `on_request/untrusted_fresh` (`73,776 Ir / (15,660.7 ns × 5.4 GHz)`), and **IPC 1.63 / 1.75 / 2.41** on `lb_pick` round-robin / P2C / Maglev (`227 / 339 / 278` Ir against `25.8 / 35.9 / 21.3 ns`) — all comfortably within the `<= 5.0` physical sanity bound. (The table's **est. cycles** column is callgrind's cache-weighted cost model — `L1 hits + 5 × LL hits + 35 × RAM hits`, e.g. `28,864 + 5 × 513 + 35 × 19 = 32,094` — not hardware cycles, so it is not the denominator of the IPC above.) Across the two WASM modes on `v0.16.2`, `on_request/untrusted_fresh` executes **`3.84×` more user-space instructions** (`73,776` vs `19,201`) and **`3.86×` more model cost** (`123,740` vs `32,094`), while Criterion wall-clock is **`15.16×` slower** (`15.66 µs` vs `1.03 µs` minimal; `14.23×` at `18.59 µs` vs `1.31 µs` capabilities). The remaining gap between `~3.8×` user-space instructions and `~14–15×` wall-clock is work callgrind does not count as user-space instructions — kernel time (page faults, `madvise`, `mmap`/`munmap`) and allocator behaviour around per-request store/linear-memory setup — plus **`5.90×` more LL cache hits** (`3,028` vs `513`) and **`2.21×` more RAM hits** (`42` vs `19`). Instruction-count invariance is not wall-clock invariance; both layers are kept for that reason.

---

# 1. Load-balancing fast path

> **Metric definition — µs/req**: `µs/req = 1e6 / rps`, i.e. inverse throughput. In a closed loop with *C* connections it equals mean latency / *C* (Little's law); it is neither latency nor CPU time. Criterion's per-call µs in [§0 Micro-benchmarks](#0-micro-benchmarks-in-process-criterion) is a single-thread, in-process quantity and dimensionally distinct.

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

![Plain HTTP/1.1 ceiling](img/ceiling.png)
![Plain HTTP/1.1 ceiling, tail latency](img/ceiling_tail.png)

| Variant | KPI | req/s | p50 | p99 |
| --- | --- | --- | --- | --- |
| keep-alive       | RR  | 234,433 | 0.19 ms | 0.86 ms |
| cold (TCP/req)   | CRR | 76,434  | 0.53 ms | 2.31 ms |

*(Re-measured 2026-11-14 (tag **v0.16.2**, `HEAD` `0d7bb08`, Rust `1.100.0`, pre-run `contended=0`, `loadavg1=3.05`) — `bash bench/perf/run-perf.sh all` / `ceiling`.
Cold/keep-alive **ratio** and the RR/CRR split are the durable signal.)*

A TCP handshake per request costs **~67 % (`67.4 %`) throughput and +1.45 ms p99** even on loopback (where the
handshake is nearly free; keep-alive RR climbed **+11.4 %** from `210.5k` on `v0.16.1` to `234.4k` under [ADR 000124](../docs/ADR/000124.md)'s unfiltered zero-copy fast path, whereas cold CRR read `76.4k` vs `85.7k` on `v0.16.1` and `106.2k` on idle `2026-10-24`) — over a real network the gap widens with RTT. Connection reuse is
load-bearing; this is the plaintext analogue of the [TLS handshake-per-request row](#tls-termination) below.

> **A note on a latency bug this scenario caught.** An early body run showed a ~40 ms p99 cliff on
> medium streamed bodies — the signature of a delayed-ACK stall. The upstream client had Nagle's
> algorithm on (no `TCP_NODELAY`), so a streamed request body sent in several writes stalled on the
> peer's delayed-ACK timer. Disabling Nagle on the upstream sockets — standard practice for L7
> proxies — removed it (100 KB streamed p99 42.9 ms → 4.2 ms). The numbers here are post-fix.

## Throughput & latency vs concurrency

Closed-loop sweep (k6 `constant-vus`) — a fixed number of virtual users, each issuing its next
request only after the previous response. Rising concurrency walks the load curve.

![Throughput vs concurrency](img/throughput_vs_concurrency.png)
![Latency percentiles vs concurrency](img/latency_vs_concurrency.png)

| VUs | req/s | p50 | p95 | p99 | p99.9 | failed |
| --- | --- | --- | --- | --- | --- | --- |
| 50  | 120,011 | 0.25 ms | 1.05 ms | 1.58 ms | 3.07 ms | 0% |
| 100 | **125,442** | 0.50 ms | 1.95 ms | 2.88 ms | 5.14 ms | 0% |
| 200 | 122,755 | 1.00 ms | 3.28 ms | 4.94 ms | 9.04 ms | 0% |
| 400 | 101,176 | 1.12 ms | 5.03 ms | 8.35 ms | 14.83 ms | **29.0%** |
| 800 | 84,374  | 1.36 ms | 6.29 ms | 10.83 ms | 21.11 ms | **48.9%** |

Throughput peaks at **~125.4k at VU 100 this run** (vs ~134.3k on `v0.16.1` and ~147.2k on idle `2026-10-24` — in general this reflects the k6 generator's own ceiling on `GEN_CPUS=12-23` and run-to-run host variance, so
which VU count wins the peak is host/generator noise, not a proxy regression) and, **through VU 200**,
declines gracefully with latency rising in proportion and zero failures — the shape this section has
always shown.

> **A newly-added admission-control cap now surfaces at VU >= 400 (measured 2026-07-20).** This pass
> is the first since ADR 000092 landed (2026-07-15, commit `36006c7`): Plecto Proxy now refuses a new
> connection outright once a single source IP holds `MAX_CONNECTIONS_PER_IP` = **256** concurrent
> connections (`crates/server/src/conn_limit.rs`, amending [ADR 000027](../docs/ADR/000027.md)) - a CWE-770/CWE-400 hardening
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
> (over 256). This is a **benchmark-harness / feature interaction**, not a regression in the load
> balancer itself. **Reproduced on 2026-09-25, 2026-10-24 (27.8 % / 50.5 %), 2026-11-14 `v0.16.1` (29.1 % / 51.3 %), and `v0.16.2` (29.0 % / 48.9 %)**, and *kept* here rather
> than tuned away: unlike the open-loop scenarios — whose oversized VU pools were pure artifact and
> are now bounded below the cap (see the [short-circuit](#short-circuit-rejecting-bad-traffic-at-the-edge), [rate-limit](#enforcement--does-it-actually-hold-the-rate), and [weighted-mix](#weighted-request-mix--with-its-own-baseline) notes below) — these two rungs exist to walk the concurrency curve, so
> crossing the cap is the honest thing for them to do. Read VU 400/800 as "one source IP past its
> admission budget", not as proxy overload; every other phase in this report keeps its own
> concurrency under 256 and reads clean.

## Tail latency under open-loop load

Open-loop sends at a **constant arrival rate** regardless of how fast responses come back, so
queueing surfaces in the tail instead of being hidden — the *coordinated-omission-safe* model.

| Model | target | achieved | p50 | p95 | p99 | p99.9 | dropped | failed |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| open-loop, 0 ms backend (`plecto-loadgen`, `v0.16.2` `2026-11-14`, `yield_now()` pacing) | 87,808/s | **87,808/s** | **0.047 ms** | 1.633 ms | 24.447 ms | 73.791 ms | **0** | 0% |
| previous snapshot (`v0.16.1` `2026-11-14`, contended host, `yield_now()` pacing) | 94,028/s | 94,028/s | 0.051 ms | 1.151 ms | 4.783 ms | 17.599 ms | 0 | 0% |
| idle-host reference (`v0.15.2+`, `2026-10-25`, `yield_now()` pacing) | 103,026/s | 103,026/s | 0.050 ms | 0.508 ms | 2.09 ms | 4.57 ms | 0 | 0% |
| *(superseded: 2026-10-24, `sleep_until` pacing)* | 103,026/s | 103,026/s | 0.84 ms | 1.65 ms | 2.83 ms | 5.58 ms | 0 | 0% |

The auto target (70 % of each run's own closed-loop peak, **87,808/s** this run from peak `125,441` req/s, vs `94,028/s` on `v0.16.1` and `103,026/s` on idle `2026-10-25`) is **achieved exactly** (`7,902,720` requests over 90 s) with
**zero dropped slots** under schedule-latency measurement (`plecto-loadgen openloop`, 64 workers —
well under ADR 000092's per-IP cap, unaffected; wrk2 model — see
[`bench/methodology.md`](../bench/methodology.md)). A co-resident Rust generator sustains the
auto rate without dropping slots. With `yield_now()` pacing, **86.9 % of requests complete within 100 µs of their scheduled send time** (`6,865,893 / 7,902,720` in `openloop_hist.csv`) and p50 improves to **0.047 ms** (ahead of both `v0.16.1`'s `0.051 ms` and idle `2026-10-25`'s `0.050 ms`). Meanwhile the upper tail percentiles (`p95 1.633 ms / p99 24.447 ms / p99.9 73.791 ms`, histogram weighted mean `0.778 ms` above the `64 / 87,808 = 0.729 ms` single-in-flight wire bound) reflect transient scheduling pauses queuing scheduled tokens behind those ms-scale bursts; `loadavg1` climbed during the back-to-back T2 suite (`3.05` pre-run → `5.48` at end of `mix`), and the offered rates also differ (`87,808/s` vs `94,028/s` vs `103,026/s`), so this is not a fixed-rate comparison — cross-snapshot tail comparisons should pin the rate with `OPENLOOP_RATE` (supported by `bench/perf/run-perf.sh`).

> **Why the 2026-10-24 p50 was 0.84 ms — generator-side send lag, confirmed by re-measurement.**
> With `C = 64` single-in-flight worker connections sustaining $\lambda = 103{,}026\text{ req/s}$ (`0 dropped`), Little's law bounds the mean wire response time (`write_all` + `read_response`) at $W_{\text{wire}} \le 64 / 103{,}026 = 0.621\text{ ms}$, and a right-skewed distribution has its median below its mean — yet schedule-latency (`done − scheduled`) reads p50 `0.84 ms` / mean `0.895 ms`. At least ~0.2–0.3 ms of it is therefore time spent *before* the request was written. Binned at 100 µs, `openloop_hist.csv` shows a flat plateau from ~0.2 to ~1.4 ms (≈ 6–9 % per bin) rather than a geometric tail — the shape a uniform 0–1 ms delay convolved with the wire latency would give. That points at the pacing: the run used `tokio::time::sleep_until(scheduled)`, and Tokio's timer wheel has 1 ms granularity, so a wake-up can land up to 1 ms late. The alternative explanation — per-connection queueing at 1,610 req/s per worker — would produce a geometric tail, which the histogram does not show. `bench/loadgen/src/openloop.rs` now sleeps only until `scheduled − 1 ms` (and only when more than 2 ms remain) and covers the last stretch with `tokio::task::yield_now()`, keeping the coordinated-omission-safe `done − scheduled` accounting. Re-measured on 2026-10-25 at the same 103,026/s: p50 fell from 0.84 to **0.050 ms**, the mean from 0.895 to 0.132 ms, and the plateau is gone (89.6 % of samples in the first 100 µs bin, then a geometric decay) — the old p50 was the generator's timer, not the proxy. One confound is not yet bounded: the yield loop keeps the generator cores busy, which can shift the package power budget shared with the proxy's P-cores. That would bias the proxy *slower*, not faster, so it cannot explain the improvement, but the closed-loop ceiling and proxy CPU-µs/req under open-loop load have not been compared with and without it.

## Round-robin distribution

![Round-robin distribution](img/rr_distribution.png)

Over a steady window with all three upstreams healthy, **120,000** requests split **40,000 /
40,000 / 40,000** — exact across all three instances (33.3 % each, 0 spread). Round-robin holds under load.
(Re-measured 2026-11-14 (tag **v0.16.2**, `HEAD` `0d7bb08`, Rust `1.100.0`, pre-run `contended=0`); `plecto-loadgen rr`, 48 workers — well under the per-IP cap, unaffected.)

## Resilience: ejection & fail-closed

A steady open-loop rate (~4k req/s, `plecto-loadgen ejection` with a 5 s unrecorded warm-up so
t=0 is already steady state) while a controller drives a fault timeline (`eject b` → `rejoin b` →
`eject all` → `restore all`) and the driver buckets each upstream's served-count and the 503/s
every second:

![Load balancing under fault injection](img/ejection_timeline.png)
![Fail-closed 503s during total ejection](img/ejection_failed.png)

- **Even baseline.** ~4k req/s split three ways while healthy (1,334/1,333/1,333 this run).
- **Graceful ejection.** When **b** is driven unhealthy its share falls to zero within ~1 s (a
  one-second mixed transition bucket — `a=1554, b=893, c=1553` at t=15, then clean) and the
  survivors (a + c) absorb the full load **with zero failed requests** — this run they split it
  **evenly** (2,000/2,000), round-robin over two survivors landing on an even split (and `rejoin b` at t=30 transitions through `a=1800, b=400, c=1800` back to `1,333/1,334/1,333` at t=31).
- **Fail-closed, not fail-open.** With **every** instance unhealthy (t=45 transition `a=974, b=973, c=973, failed=1080`), Plecto Proxy returns **HTTP 503**
  promptly (no hang, no blind forward); the 503/s line jumps to the full offered rate (4,000/s here).
- **Fast recovery.** Restoring health returns instances to rotation within ~1 s (a one-second mixed
  bucket at t=60 — `a=306, b=307, c=307, failed=3080`, then clean 1,334/1,333/1,333 at t=61).

*(Re-measured 2026-11-14 (tag **v0.16.2**, `HEAD` `0d7bb08`, Rust `1.100.0`, pre-run `contended=0`). Over the 75 s T2 fault-timeline run: 300,000 responses, 60,160 failed (20.05 %) during total ejection, across events eject b @15 s / rejoin b @30 s / eject all @45 s / restore all @60 s; over the 40 s pooled T1 gate run: 160,000 responses, 24,040 failed (15.03 %), 0 stray failed.)*

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

![Endpoint-set swap under load](img/swap_timeline.png)

> Re-measured 2026-11-14 (tag **v0.16.2**, `HEAD` `0d7bb08`, Rust `1.100.0`, pre-run `contended=0`): a steady ~4k req/s open-loop while, at
> t=15 s (post-warmup), the manifest is rewritten `[a, b, c]` → `[a, b, d]` and SIGHUP-reloaded
> (64 workers — well under the per-IP cap, unaffected; same shape as every prior pass).

- **Zero client-visible failures.** All 240,000 responses over the 60 s run succeeded — **0 %
  failed** — even through the swap itself. Unlike a health-based ejection, nothing here ever needs
  to fail closed: `a` and `b` are unchanged addresses, so `reconcile` reuses their `Arc`s and
  health outright (ADR 000017's reuse rule), and only `d` starts pessimistic.
- **The swap completes within one second.** The transition second (t=15) shows a brief mixed
  bucket (`a=1608, b=1606, c=13, d=773`) as in-flight requests to `c` finish and the reconciled
  pool takes over mid-second; by t=16 the split is already clean — `c=0`, and `a` / `b` / `d` even
  at ~1,333 each (`1,333 / 1,334 / 1,333`) — the same ~1 s time constant [ejection](#resilience-ejection--fail-closed) shows,
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

![TLS vs plain](img/tls_vs_plain.png)
![TLS vs plain, tail latency](img/tls_tail.png)

| Variant | req/s | p50 | p99 | isolates |
| --- | --- | --- | --- | --- |
| plain (h1)               | 234,433 | 0.19 ms | 0.86 ms | [ceiling](#plain-http11-ceiling) keep-alive |
| TLS h1, keep-alive       | 188,716 | 0.25 ms | 0.58 ms | record layer + TLS I/O path |
| TLS h1, handshake/req    | 24,765  | 1.79 ms | 6.32 ms | oha, shared `ClientConfig` — see caveat below |
| TLS (h2)                 | 153,710 | 0.30 ms | 0.90 ms | h2 multiplexing over TLS |

The decomposition is the point. This run's ordering is clean — plain h1 keep-alive (234.4k) sits
above the TLS keep-alive rung (188.7k, ~80.5 % of plaintext): **within-TLS ratios** are the signal:
handshake/req (24.8k, slightly below the `26.2k` idle `2026-10-24` figure) is **~13.1 % of TLS keep-alive**, and **h2 is clean** (153.7k/s, ~81.5 % of TLS
keep-alive, p99 0.90 ms). The TLS-terminated path remains **crypto-/TLS-I/O-bound**, though ADR 000124's
HTTP/1 & HTTP/2 header/routing savings still lift TLS keep-alive (+14.1 %) and TLS h2 (+11.0 %) cleanly over `v0.16.1`. A client that funnels many VUs over a handful of
multiplexed connections can make h2 *look* far worse (head-of-line queueing, not server work);
measuring with a connection-per-concurrency client removes that artifact.

*(Re-measured 2026-11-14 (tag **v0.16.2**, `HEAD` `0d7bb08`, Rust 1.100.0) on **aws-lc-rs** (ADR 000051), `-c 50` —
well under the per-IP cap, unaffected. Qualitative story unchanged across every snapshot so far.)*

### Full vs resumed handshake (ADR 000052)

*(Not re-run this pass — `bench/perf/run-perf.sh`'s `tls` phase doesn't drive this
rung automatically; the numbers below are carried over unchanged from 2026-07-05 at commit `c635ed3`, tag **v0.5.0**.)*

The `handshake/req` row above no longer isolates a *true* full handshake: oha shares one rustls
`ClientConfig` across connections, and against a server issuing stateless TLS 1.3 session tickets
its "cold" connections silently resume once warm. `plecto-loadgen tls --mode full|resumed` gives
each rung explicit resumption control instead:

| Client resumption | req/s | p50 | p99 | resumed |
| --- | --- | --- | --- | --- |
| full (disabled)      | 22,099 | 2.06 ms | 4.36 ms | 0 % |
| resumed (enabled)     | 29,768 | 1.54 ms | 3.26 ms | 93.0 % |

Comparing the carried `2026-07-05` (`v0.5.0`) true full handshake (22.1k/s) against the same-session `2026-07-05` (`v0.5.0`, 26.5k/s) and `2026-10-24` (`v0.15.2`, 26.2k/s) `handshake/req` rows gave a **~16–17 %** gap —
confirming oha's row really was partly resumed — whereas against this `2026-11-14` (`v0.16.2`) snapshot's `handshake/req` row (`24.8k`, `24,765/s`) the cross-version gap is **~10.8 %** (`22.1 / 24.8`), pending a re-run of the full/resumed rungs. Enabling client resumption recovers **~35 % throughput**
over a true full handshake — **≈11.7 µs/connection** (`45.3 → 33.6 µs` in inverse throughput `1e6 / rps`; note that per the [metric definition](#1-load-balancing-fast-path) this is an inverse-throughput delta across concurrent connections rather than single-core server CPU time, and includes both the proxy's server-side `CertificateVerify` signing on `PROXY_CPUS=0-11` and the load generator's client-side certificate-chain verification on `GEN_CPUS=12-23`, which mixes 2 P-cores and 8 E-cores). That saving comes from skipping the
certificate chain + asymmetric signature generation/verification, **not** the ephemeral key exchange: rustls's client
hardcodes `psk_dhe_ke` and never offers plain `psk_ke` ([RFC 9846 §4.3.9](https://www.rfc-editor.org/rfc/rfc9846#section-4.3.9), obsoleting RFC 8446 where this lived at §4.2.9; for forward secrecy of PSK + (EC)DHE/KEM see [RFC 9846 §2.2](https://www.rfc-editor.org/rfc/rfc9846#section-2.2), [RFC 9257 §4.2](https://www.rfc-editor.org/rfc/rfc9257#section-4.2) and [§6](https://www.rfc-editor.org/rfc/rfc9257#section-6), and rustls `client/hs.rs`'s source comment — *"such connections don't have forward secrecy"*), so every resumed handshake here still runs a fresh ephemeral key exchange for forward secrecy. Because both `plecto` (`crates/control/src/tls.rs`) and `plecto-loadgen` build `rustls` with `aws-lc-rs`'s default `prefer-post-quantum` provider, that key exchange negotiates the hybrid post-quantum group **`X25519MLKEM768`** (ML-KEM-768 + X25519; emitted as `kx_group` by `plecto-loadgen tls`) rather than classical ECDHE alone. The ~11.7 µs inverse-throughput delta is consistent with that: skipping only the asymmetric
sign/verify + cert bytes is a much smaller saving than skipping the hybrid `X25519MLKEM768` key exchange too would be. The residual
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

![Throughput by decision path](img/wasm_throughput.png)
![Per-request latency by decision path](img/wasm_latency.png)

> W1 — fixed 50 connections, 0 ms backend, valid key (oha, warm-up burned in a discarded 5 s
> pre-run). Full-throttle: read these rows as **throughput ceilings**; the honest latencies are in
> the fixed-rate tail table below.

Five routes forward to the **same** backend, so each **adjacent delta isolates exactly one cost**. A
pure **no-op** WASM filter (no host-API calls) is the key addition — it separates "the WASM tax" from
"a real filter's work", which older reports conflated.

| Route | Decision path | req/s | p50 | p99 |
| --- | --- | --- | --- | --- |
| `/baseline` | native fast path (no filter) | 234,433 | 0.19 ms | 0.86 ms |
| `/noop-pooled` | a **pure no-op** WASM filter, pooled | 198,053 | 0.24 ms | 0.50 ms |
| `/noop-fresh` | the same no-op, **fresh instance / request** | 20,553  | 2.40 ms | 4.87 ms |
| `/trusted` | the real `filter-apikey`, pooled | 171,887 | 0.28 ms | 0.60 ms |
| `/ondemand` | `filter-apikey`, fresh instance / request | 19,595  | 2.52 ms | 4.97 ms |

*(Re-measured 2026-11-14 (tag **v0.16.2**, `HEAD` `0d7bb08`, Rust `1.100.0`, pre-run `contended=0`, `loadavg1=3.05`), `-c 50` — well under the per-IP cap,
clean. `/baseline` is sourced from [ceiling.csv](#plain-http11-ceiling); the other four rungs are
measured together in the same session. With [ADR 000124](../docs/ADR/000124.md)'s synchronous zero-WASI execution and borrowed request lowering on top of inline header hooks
([ADR 000119](../docs/ADR/000119.md)), default arena cap 32 ([ADR 000118](../docs/ADR/000118.md)), and Wasmtime 50.0.1: `baseline` > every WASM rung, pooled ceilings jumped `+13.7–19.6 %` over `v0.16.1`, and fresh ceilings jumped `+44.6–50.7 %`; the fixed-rate tails
below remain the honest queueing-free read.)*

- **baseline → noop-pooled** = the **irreducible extension-plane dispatch cost**. Full-throttle,
  this run shows a **~15.5 % throughput** cost (234.4k → 198.1k, **≈ 0.78 µs/req** inverse-throughput
  delta [see metric definition](#1-load-balancing-fast-path), down from `0.99 µs/req` in `v0.16.1`; T1 pooled gate `dispatch_floor_us` **`0.5909 ± 0.2398 µs`**, `n = 16`, pass, vs `1.09 ± 0.19 µs` in `v0.16.1` and `1.24 ± 0.05 µs` on idle `2026-10-25`); the fixed-rate tails put the monotone queueing-free floor at
  **+0.014 ms p50** (real filter **+0.046 ms p50**, `0.0941 < 0.1082 < 0.1404 ms`, and monotone p99s `0.6744 < 0.8088 < 1.1044 ms` across `/baseline < /noop-pooled < /trusted` at 11,757 req/s; the T1 gate at 2,000 req/s reads `+0.0077 ms` p50, `+0.0118 ms` p99). Every WASM filter pays this floor.
- **noop-pooled → noop-fresh** = the **per-request instantiation cost**, cleanly isolated from any
  host work: throughput drops **~9.6×** (`9.64×`, 198.1k → 20.6k; `8.77×` on `/trusted` → `/ondemand`, narrowed from `12.77×` / `10.61×` in `v0.16.1` because fresh instantiation sped up `+44.6–50.7 %` after ADR 000124). This is still what pooling buys.
- **noop-pooled → trusted** = a **real filter's own work** on top of the no-op (header parse +
  host-KV lookup + counter): **−13.2 % (≈ 0.77 µs/req this run)** — with T1 pooled gate `apikey_cost_us` **`0.4349 ± 0.2712 µs`** (`n = 16`, pass, inside ADR 000124's recalibrated `−0.2..1.7 µs` band; down from `0.84 ± 0.21 µs` in `v0.16.1` and `1.06–1.30 µs` on idle `2026-10-24/25`). With inline
  header hooks ([ADR 000119](../docs/ADR/000119.md)) and ADR 000124, the dispatch floor (`~0.59–0.78 µs`) and the apikey
  filter's own work (`~0.43–0.77 µs`) stay roughly equal terms. As a historical illustration of the pooling rule, the first 8-round gate (2026-10-25) left
  `apikey_cost_us` *inconclusive* at `1.74 ± 1.13 µs`: six of its eight paired rounds read 0.95–1.34 µs, but in
  rounds 3 and 4 the `/trusted` window ran at 124.8k and 80.6k rps (round 4's `/baseline` also dropped to 169k), giving
  2.24 and 4.93 µs. A disturbance that hits one arm of a pair does not cancel, and the t interval correctly refuses a
  verdict. Per the gate's rule the re-run (`20261025T053124Z`, whose own 8 rounds read `1.06 ± 0.14 µs`) was **pooled** with these
  rounds rather than replacing them: n = 16 gives `1.40 ± 0.53 µs`, still inconclusive against the `0.3..1.7` band; a third pooled run (n = 24, reaching the gate's 3-run pooling cap) resolves it to
  **`1.30 ± 0.35 µs`, pass** — with the two disturbed rounds still in the estimate. (Because round-level pooling treats the 24 rounds as independent and does not model between-run cluster variance — at the run-mean level $n = 3$ gives `1.30 ± 0.94 µs`, which still straddles `1.7` — the independent non-pooled holdout run `20261025T061734Z` at **`1.06 ± 0.09 µs`**, $n = 8$, is the decisive check.)
- **noop-fresh and ondemand are the same order of magnitude** (20.6k vs 19.6k req/s — within **~4.7 %**),
  confirming instantiation dominates the fresh path — the filter's per-request work is
  small next to re-paying `init` (`~15.66 µs` minimal / `~18.59 µs` capabilities in `v0.16.2` Criterion) every request.

### The same ladder at one fixed rate — honest tails

> W1b — every rung offered the **same** fixed **11,757 req/s** this run (60 % of the slowest rung's
> ceiling, `/ondemand` at 19,595/s — `+44.6 %` higher offered load than `v0.16.1`'s `8,128/s`), 50 connections, oha `-q` + `--latency-correction`
> (coordinated-omission-safe). Identical offered load, so the latency columns are directly
> comparable.

| Route | achieved | p50 | p90 | p99 |
| --- | --- | --- | --- | --- |
| `/baseline` | 11,755/s | 0.09 ms | 0.38 ms | 0.67 ms |
| `/noop-pooled` | 11,755/s | 0.11 ms | 0.31 ms | 0.81 ms |
| `/trusted` | 11,756/s | 0.14 ms | 0.64 ms | 1.10 ms |
| `/noop-fresh` | 11,685/s | 0.95 ms | 53.35 ms | 259.28 ms |
| `/ondemand` | 11,756/s | 0.84 ms | 24.69 ms | 160.24 ms |

At a rate every rung sustains (11,757 req/s), both p50 (`0.0941 < 0.1082 < 0.1404 ms`) and p99 (`0.6744 < 0.8088 < 1.1044 ms`) increase monotonically across native, pooled noop, and pooled real filter: the pooled dispatch floor costs **+0.014 ms p50** over native (`0.0941 → 0.1082 ms`) and the real pooled filter **+0.046 ms p50** (`0.0941 → 0.1404 ms`, with p99 staying `≤ 1.10 ms`).
The fresh rungs live at **p99 ~160–259 ms** at 11,757 req/s (`/noop-fresh` / `/noop-pooled` = `259.28 / 0.8088 ≈ 321×`, `/ondemand` / `/trusted` = `160.24 / 1.1044 ≈ 145×`; elevated relative to both `v0.16.1`'s `69.05–156.99 ms` at `8,128/s` and the idle `2026-10-24` run's `2.64–4.19 ms` at `7,565/s`, largely because the auto-scaled 60 % offer jumped to `11,757/s` — `+55.4 %` above `7,565/s` — pushing concurrent fresh instantiation past the kernel/allocator knee), reflecting the per-request instantiation and kernel/allocator cost described below. (And `/noop-fresh` vs `/ondemand` shows the same single-window jitter pattern — `0.95 vs 0.84 ms` p50, `259.28 vs 160.24 ms` p99, and `/noop-fresh` slightly short of target at `11,685/s`: on the fresh path, per-request instantiation at `~15.66 µs` plus kernel/allocator contention is `>20×` larger than `filter-apikey`'s `~0.77 µs` guest work, so run-to-run jitter in concurrent instantiation swamps the tiny guest-work difference — a single fixed-rate window per rung cannot separate the two. Adjacent deltas between the fresh rungs are not a reliable isolation of one cost at this rate.)

> **Why per-request instantiation scales poorly under concurrency — and what is (and is not) known about the knee since `v0.15.2`:**
> In the original `2026-07-06` investigation (under legacy `MALLOC_ARENA_MAX=4` and blocking-pool `spawn_blocking` dispatch), a fresh instance (`Allocation::OnDemand`) triggered heavy `mmap`/`munmap` + `mmap_lock` serialization and cross-core TLB shootdowns (`~31–35` TLB shootdowns/req in `/proc/interrupts` vs `~0.3` pooled), causing p99 to blow up from `1.4 ms` at `1k/s` and `4.7 ms` at `2k/s` to `~650 ms` at `4.2k/s` and `~1.2 s` at `6k/s`. In the idle `2026-10-24` (`v0.15.2`) snapshot, **7,565 req/s** completed at **p99 = 2.64–4.19 ms** — more than 150× below the old `4.2k–6k/s` cliff, so the mechanism behind the 07-06 knee no longer describes the current fresh tail (while at higher auto-scaled 60 % offers — **8,128 req/s** in `v0.16.1` and **11,757 req/s** in `v0.16.2` — `p50` stays `0.84–0.95 ms` while `p99` stretches to `160.24–259.28 ms`). Four changes landed after 07-06 and are candidate causes for the ceiling/knee shift — inline header hooks ([ADR 000119](../docs/ADR/000119.md), which also cut proxy threads from 80 to 35 per its scheduling measurement), the default allocator arena cap `4 → 32` ([ADR 000118](../docs/ADR/000118.md)), Wasmtime 50.0.x ([ADR 000120](../docs/ADR/000120.md)), and synchronous zero-WASI fresh instantiation ([ADR 000124](../docs/ADR/000124.md)) — but **none of them has been isolated against `/proc/interrupts` at matched fixed rates**, and where the knee now sits has not been swept. Even with that ceiling gain, fresh instantiation remains **~145–321× slower at p99** on like-for-like pairs at 11,757/s (`/noop-fresh` / `/noop-pooled` = `259.28 / 0.8088 ≈ 321×`, `/ondemand` / `/trusted` = `160.24 / 1.1044 ≈ 145×` this run; `~12–19×` idle at `7,565/s` in `v0.15.2`) and **~8.8–9.6× lower in ceiling throughput** than Wasmtime's pooling allocator (which pre-maps slots and avoids per-request virtual-memory churn). *(Re-measuring TLB shootdowns/req from `/proc/interrupts` deltas — unprivileged — at 1k / 2k / 4k / 6k / 7.5k / 10k req/s is the prerequisite for any causal statement here.)*

**The µs/req deltas are the invariants to track for regressions, not the percentages** (which widen or
shrink whenever the *baseline* moves). These macro deltas **reconcile with the re-measured `v0.16.2` in-process
[micro-benchmarks](#0-micro-benchmarks-in-process-criterion)** (`0d7bb08`) in direction and general scale, while
remembering that the `~0.78 µs/req` (`0.59 µs/req` in the T1 pooled gate) is an inverse-throughput delta under 50-connection saturation (~+39 µs mean latency across 50 connections), whereas a single-thread criterion call (`~1.03 µs` minimal, `1.31 µs` capabilities) is in-process wall-clock time on one core. The fresh `~15.66 µs` (`minimal`) / `~18.59 µs` (`capabilities`), by contrast, is the *uncontended* single-thread cost — criterion
instantiates sequentially, so it never pays the `mmap_lock` contention or cross-core shootdowns the
concurrent macro run exposes (the knee above). The layers agree once that kernel-side term is named.

> **Decomposed 2026-09-29 (inline header hooks, [ADR 000119](../docs/ADR/000119.md)) & confirmed across T2/T1 (`v0.15.2` idle `2026-10-24/25`, `v0.16.1` `2026-11-14`, and **`v0.16.2`** `2026-11-14` `HEAD` `0d7bb08` implementing [ADR 000124](../docs/ADR/000124.md), wasmtime 50.0.1 + dual WASI 0.2/0.3 `@0.3.0`, [ADR 000120](../docs/ADR/000120.md) / [ADR 000121](../docs/ADR/000121.md)):**
> Prior to ADR 000119, the ~4.5–4.9 µs/req dispatch floor was mostly the blocking-pool round trips (two per filtered request: on-request
> and on-response) rather than guest execution. Trusted, non-blocking filters now run inline on the
> Tokio worker. On the reference host (i7-13700K, 6 P-cores, 50 conns), dispatch floor first dropped from
> 4.06–4.21 to 1.41–1.44 µs/req at ADR 000119 landing, and per-request scheduling collapsed: voluntary context switches 5.09 → 0.79/req (baseline 0.87–0.92),
> CPU 103 → 76 µs/req, runqueue wait 76 → 0.7 µs/req, and proxy thread count 80 → 35; in `v0.15.2` (`2026-10-24/25`), inline dispatch held at **1.17–1.30 µs/req** (`dispatch_floor_us` **1.24 ± 0.05 µs** pooled / **1.17 ± 0.12 µs** holdout, `apikey_cost_us` **1.30 ± 0.35 µs** pooled / **1.06 ± 0.09 µs** holdout), and on `v0.16.1` at **1.0944 ± 0.1898 µs** / **0.8428 ± 0.2117 µs**. On **`v0.16.2`** (`0d7bb08`, [ADR 000124](../docs/ADR/000124.md)), removing fiber/async-store orchestration for sync zero-WASI filters, borrowing `HttpRequestV02V03Ref<'a>`, and gating telemetry/wall-clock pushes the dispatch floor and real filter work roughly in half again: T2 cost ladder reads **~0.78 µs/req** dispatch floor / **~0.77 µs/req** apikey cost, and the pooled T1 gate (`20261114T135754Z-0d7bb08`, `n = 16`, PASS) records `dispatch_floor_us` **`0.5909 ± 0.2398 µs`** and `apikey_cost_us` **`0.4349 ± 0.2712 µs`** alongside a **−31.3 %** gungraun instruction drop (`27,955 → 19,201` Ir).

## Short-circuit: rejecting bad traffic at the edge

![Accept vs reject latency](img/wasm_shortcircuit.png)

> W2 — fixed 2000 req/s, 15 ms backend, ~90 % valid / ~10 % bad keys (k6). 108,047 accepted, 11,987
> rejected, **0 status-less** — a **90.0 % / 10.0 %** split, matching the script's own key roll.

| Path | p50 | p95 | p99 |
| --- | --- | --- | --- |
| accept (200, forwarded) | 16.101 ms | 16.97 ms | 17.31 ms |
| reject (401, short-circuited) | 0.087 ms | 0.352 ms | 0.458 ms |

Accepted requests cost the 15 ms backend plus the small pooled-filter + proxy overhead. Rejected
requests are decided **at the edge in 0.087 ms (p50) / 0.352 ms (p95)** (`wasm_mixed.csv`; vs `0.178 ms` p95 on the idle `2026-10-24` host and `0.337 ms` on `v0.16.1`, while p99 is `0.458 ms`) and never reach the upstream: bad traffic is shed
**~185× faster at p50 (`16.101 / 0.087 = 185.1×`) and ~48× faster at p95 (`16.97 / 0.352 = 48.2×`)** than good traffic is forwarded, and is harmless to the backend it would otherwise
hit. (Filter faults or deadline overruns **fail closed** - 502/504 - exercised by the test suite,
not this benchmark.)

> **The 07-20 split (76 %/24 %) was a harness artifact, fixed and re-measured on 08-15 and reconfirmed 2026-09-25 / 2026-10-24 / 2026-11-14 (`v0.16.1` & `v0.16.2`).**
> That pass's `constant-arrival-rate` executor pre-allocated **300 VUs** — above ADR 000092's
> 256-connections-per-source-IP cap — and `bench/k6-wasm/mixed.js` counted *anything* not `200` as
> a rejection, so refused connections (`res.status === 0`, no HTTP status at all) were tallied
> alongside genuine 401s. The script now bounds its pool below the cap and counts a status-less
> response in its own `no_status` bucket, which the CSV carries; this pass reports **`no_status` =
> 0** and the split returns to the designed ~90/10. The 07-20 *latency* figures were always valid —
> only the count split was contaminated.

<a id="v030-response-ladder--compression"></a>
## Response hooks & compression (ADR 000073–000075)

The `all` pass measures every route with ADR 000073/074/075 **present but
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
| `/noop-pooled` | on-response unused params → continue | 112,955 | 0.42 ms | 0.83 ms | 8.85 |
| `/resp-ctx` | read as-forwarded snapshot → continue | 110,183 | 0.44 ms | 0.83 ms | 9.08 |
| `/resp-replace` | read + `replace` (418, 23 B body) | 105,742 | 0.45 ms | 0.84 ms | 9.46 |

*(Not re-run this pass — measured 2026-08-15 at commit `b2a89be` (tag **v0.9.0**) via `v03` prior to [ADR 000119](../docs/ADR/000119.md) inline header hooks, [ADR 000118](../docs/ADR/000118.md) default arena cap 32, and [ADR 000124](../docs/ADR/000124.md); `/noop-pooled` in this table is therefore `112,955 rps` (`8.85 µs/req`) from that v0.9.0 session rather than the `2026-11-14` (`v0.16.2`) snapshot's `198,053 rps` (`5.05 µs/req`). Same-process adjacent deltas — do not splice onto an older or newer `wasm` CSV's noop row. On current `HEAD` (`v0.16.2` / `0d7bb08`), the T1 pooled gate (`20261114T135754Z-0d7bb08`, `n = 16`, PASS) tracks the 2,000 rps fixed-rate tail delta `respctx_tail_p50_ms` directly and reads **`+0.0009 ms`** (`pass`, well inside `-0.05..0.25 ms` and matching the `v0.15.2` (`2026-10-24`) idle-host spread `−0.0008 … +0.0011 ms` / confirmation run `+0.0010 ms`, vs contention noise `−0.0081 ms` on `v0.16.1`). µs/req = 1e6 / rps inverse throughput; see [definition](#1-load-balancing-fast-path).)*

- **noop-pooled → resp-ctx ≈ +0.22 µs/req** in that v0.9.0 `v03` run — the cost of *using* the ADR 000073 request
  snapshot on `on-response` (path length + header scan), with the same continue/forward path. The
  three snapshots that have measured this rung read **+0.90 / +0.10 / +0.22 µs**, all small enough
  that the run-to-run movement of the ceiling itself is a comparable term; track the fixed-rate p50
  below alongside this figure, not this number alone. The T1 gate's `respctx_tail_p50_ms`
  invariant (**`+0.0009 ms`** over pooled in `v0.16.2` `20261114T135754Z-0d7bb08`, **`+0.0010 ms`** in the `2026-10-24` `v0.15.2` confirmation run's `gate.csv`, `+0.016 ms` in v0.9.0; vs `−0.0081 ms` contention noise on `2026-11-14` `v0.16.1`) is the tighter read on an idle/uncontended host.
- **noop-pooled → resp-replace ≈ +0.60 µs/req** net at full throttle in that v0.9.0 (`2026-08-15`) `v03` session — unchanged from the preceding
  `v03` run to two decimals. `replace` synthesises a tiny body and **drops** the upstream payload on the
  wire (verified: `Content-Encoding` N/A, 23-byte 418). That wire-shape change can *under-* or
  *over-read* replace's guest/host work relative to resp-ctx run to run; do **not** read either
  run's number as a pure CPU claim. The regression signal for replace is "still within ~1 µs of
  the pooled no-op on this host," which held in that v0.9.0 `v03` session.
- Fixed-rate tails at **63,444/s** (60 % of this ladder's slowest ceiling; oha `-q`
  `--latency-correction`): all three rungs hold the offered rate; **p50 stays ~0.88–0.91 ms**. p99
  at this rate is host-noise-dominated on this session (8–18 ms) — same caveat as other
  high-rate fixed runs near a host knee; prefer µs/req + p50 for this row.

### Native response compression (ADR 000074 / 000075)

> R2 — same oha shape; backend **4 096 B** repeating `text/plain` (above the 1024-byte
> `min_length` default). Both routes send `Accept-Encoding: gzip`. `/baseline` has no
> `[route.compression]` → identity; `/compress` pins `algorithms = ["gzip"]`. Wire check:
> identity 4096 B vs gzip **45 B** + `Content-Encoding: gzip` + `Vary: accept-encoding`.

| Route | Transform | req/s | p50 | p99 | µs/req |
| --- | --- | --- | --- | --- | --- |
| `/baseline` | identity (AE advertised, opt-in off) | 192,892 | 0.25 ms | 0.46 ms | 5.18 |
| `/compress` | gzip (level 5, ADR 000075 defaults) | 133,135 | 0.36 ms | 0.63 ms | 7.51 |

- **baseline → compress ≈ −31.0 % ceiling / +2.33 µs/req** (inverse throughput; see [definition](#1-load-balancing-fast-path)) for this highly compressible 4 KiB
  filler — the third pass in a row within ~0.25 µs of the same figure (+2.11 / +2.09 / +2.33). Real
  HTML/JSON ratios and CPU will differ; this row is a **regression floor** for the opt-in path, not
  a capacity guide for production payloads. RFC 9411 §7.3-style: one object size, sustainable
  throughput, method disclosed.
- Fixed-rate at **79,881/s** (60 % of compress ceiling): both hold the rate; p50 ≈ 0.94 ms;
  p99 again host-noise-band at this offered load (87 ms identity / 116 ms gzip — this session's
  neighbour load shows up hardest in the highest-rate fixed run in the report) — µs/req from the
  ceiling table is the durable signal.

**Criterion note (not re-run here).** Day-to-day criterion absolute drift (±10–20 %) is expected
without a locked governor. To attribute ADR 000073 contract-surface cost in-process, use a
same-host baseline pair — `cargo bench -p plecto-host -- --save-baseline pre-adr73` on the
pre-landing commit, then `--baseline pre-adr73` after — per
[criterion baselines](https://bheisler.github.io/criterion.rs/book/user_guide/command_line_options.html)
([`bench/methodology.md`](../bench/methodology.md)).

## Outbound ext_authz (ADR 000036)

*(Not measured this pass — load numbers deferred; capability verified functional by host test suite.)*

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

| Route | Interleaved T1 gate (`20261114T135754Z-0d7bb08`, 16 pooled rounds) | Single-round T2 (`ratelimit_overhead.csv`) | T2 p50 | T2 p99 |
| --- | --- | --- | --- | --- |
| `/baseline` (no filter) | **111,661 req/s mean** (`87.2k–123.1k`, `9.03 µs/req`) | 125,905 req/s | 0.25 ms | 1.65 ms |
| `/ratelimit` (bucket) | **112,180 req/s mean** (`106.5k–116.7k`, `8.92 µs/req`, **`−0.11 ± 0.45 µs/req`** 95 % t, `n = 16`) | 122,776 req/s | 0.29 ms | 1.58 ms |

*Note: In the interleaved gate column, `9.03` and `8.92 µs/req` are the means of the 16 per-round `1e6 / rps` values (their difference `8.92 − 9.03 = −0.11 µs/req` matches `ratelimit_tax_us = −0.1069`), rather than `1e6` divided by the mean `req/s` (`8.96` / `8.91 µs/req`).*

![Rate-limit overhead](img/ratelimit_overhead.png)
![Rate-limit overhead, tail latency](img/ratelimit_overhead_tail.png)

*(Re-measured 2026-11-14 (tag **v0.16.2**, `HEAD` `0d7bb08`, Rust `1.100.0`, pre-run `contended=0`), `VUS=50` — well under ADR 000092's per-IP cap, unaffected.)*

With [ADR 000124](../docs/ADR/000124.md) on top of inline header hooks ([ADR 000119](../docs/ADR/000119.md)), the rate-limit tax has compressed far below its historical range (**`1.06–1.49 µs/req`** on the idle `2026-10-25` 8-round gates; **`1.12–1.30 µs/req`** across the five archived `v0.15.2` runs; `0.5913 ± 0.2086 µs/req` on `v0.16.1`), so an un-interleaved single-round k6 closed-loop run (`ratelimit_overhead.csv`, plotted above: `125.9k` vs `122.8k req/s`, p50 `0.25 ms → 0.29 ms`) can swing from between-window host variance. That is why the paired gate rounds are the authoritative read for this invariant. Under ADR 000124's recalibrated `bench/perf/gate_tolerances.toml` band (`−0.6..2.4 µs`, where `hi = 2.4 µs` stays unchanged and safety against a non-limiting filter is verified separately by `enforce_allowed_ratio = 0.9999`), Run 1 (`20261114T135107Z-0d7bb08`, `n = 8`) read `−0.3495 ± 0.8076 µs` (`inconclusive`, straddling `lo = −0.6`), Run 2 alone (`n = 8`) read `+0.1358 ± 0.5645 µs`, and the **16-round pooled gate (`20261114T135754Z-0d7bb08`)** resolves `ratelimit_tax_us` to **`−0.1069 ± 0.4492 µs/req` (`n = 16`, PASS)** — its upper bound `+0.34 µs` sits more than `2 µs` below the `2.4 µs` regression limit while `/baseline` varied across `87.2k–123.1k` rps (`106.5k–116.7k` on `/ratelimit`). *(Historical context on the band calibration: at `n = 2` on `v0.15.2`, e.g. `20261024T042039Z-1061a79` at `1.20 ± 0.80 µs`, intervals were too wide; moving to 8 paired rounds and setting `lo = 0.5` in [ADR 000122](../docs/ADR/000122.md) passed on idle `v0.15.2` (`1.06 ± 0.14` / `1.49 ± 0.54 µs`), before ADR 000124's data-plane/host speedups shifted `lo` to `−0.6` so zero-bound k6 ceiling noise does not false-flag a faster proxy.)*

**This tax is not "dispatch floor + bucket consult".** Additively it could never be smaller than the floor, yet the k6
`ratelimit_tax_us` reads below the same run's oha `dispatch_floor_us` in almost every gate run since ADR 000119 (`−0.11` vs `0.59 µs` in `20261114T135754Z-0d7bb08`; `0.59` vs `1.09 µs` in `v0.16.1`; `1.20` vs
`1.29 µs` in the 10-24 confirmation run). The two are measured by different generators: oha (`-c 50`) drives `/baseline` to ~234.4k rps (~212.7k idle `v0.15.2`), while k6 (`VUS=50`
over 1,000 tenant keys) reaches ~87.2k–123.1k rps this run (mean `111.7k`; ~117k–155k idle) on `GEN_CPUS=12-23` — near its own ceiling. In a closed loop
whose generator is near saturation, extra proxy work is partly absorbed by the generator waiting less, which compresses
$\Delta(1/\text{RPS})$; that is the most plausible reading, not a measured decomposition. Either way, deltas from different
generators cannot be added or subtracted. A candidate generator-independent read is proxy **CPU-µs/req at a fixed rate**,
which the gate records as `ratelimit_cpu_us_per_req` (20k/s, `bench/perf/proc_cpu.py`; `info` until calibrated). Each snapshot records both per-thread `/proc/<pid>/task/*/schedstat` (ns resolution) and process-wide `/proc/<pid>/stat` (`utime + stime`), falling back to `stat` whenever a thread present at window start has exited by window end; across all 32 windows of the four 2026-10-25 runs, `schedstat` and `stat` agreed to within `~0.1 µs/req`.
On the idle `2026-10-25` runs, three pooled runs (12 paired rounds, no pre-window idle sleep) read **+15.3 ± 1.0 µs CPU per request** over a `/baseline` of `32.3 ± 0.4 µs`, and the standalone holdout (`20261025T061734Z`, 4 paired rounds with `GATE_CPU_SETTLE_S=11` — now recorded in `gate_params.txt` / `build_id.txt` so runs with different settle times cannot be pooled) read **+18.3 ± 0.8 µs** over a `/baseline` of `34.5 ± 0.7 µs`
(absolute values include the in-process mock upstream; the delta cancels it). On this `v0.16.2` (`0d7bb08`) pooled gate (`n = 8`, `GATE_CPU_SETTLE_S=11`), `/baseline` reads **`75.27 ± 25.24 µs/req`** and `ratelimit_cpu_us_per_req` **`−6.28 ± 18.69 µs/req`** — two 20k/s windows in Run 1 dropped 444 and 588 of 60,000 iterations with rate-hold warnings, so host scheduling/turbo variance dominated the uncalibrated CPU delta. Note also from the idle `2026-10-25` holdout why the 11 s settle reads ~7 % higher on `/baseline` (`32.3 → 34.5 µs`) and why `/ratelimit` spawns a few blocking-pool threads (`26 → 29 → 32` across the first two windows, then flat at `32`): the 11 s idle settle after the ~128k req/s closed-loop phase lets the cores clock down under the `powersave` governor before entering the light 20k/s 3 s windows (lower clock frequency means more nanoseconds on-CPU per instruction), while `filter-hello` (`Isolation::Trusted`, `MemoryBackend`) runs its header hooks inline on the Tokio worker (`runs_inline = true`) and only the once-per-65,536-calls instance recycle (`max_requests_per_instance = 65,536`, [ADR 000119](../docs/ADR/000119.md) Decision 2) hands off replacement instance creation (`IdleMiss::PoolFilling`) to `spawn_blocking`. Its additivity is not yet verified: at 20k/s the
proxy is far from saturated, so the figure can include per-request wake-up and recycle costs that amortize differently under load, and ~15–18 CPU-µs spread over 12 cores (≈ 1.27–1.53 µs) only matches the inverse-throughput
deltas in order of magnitude — a consistency hint, not an equivalence. What the tax *contains* is still the filter dispatch plus the host-native bucket consult,
including the per-call host-state quota check (ADR 000027) that keeps a multi-tenant filter's bucket count bounded —
only the arithmetic across generators is invalid.

### Enforcement — does it actually hold the rate?

![Rate-limit enforcement](img/ratelimit_enforce.png)

> R2 — a **tight** bucket (refill 1000 tok/s, burst 2000), offered **5000 req/s** open-loop at one
> key for 30 s (k6).

| offered | allowed (200) | shed (429) | status-less | accept p99 | 429 p99 |
| --- | --- | --- | --- | --- | --- |
| 5,000/s | **1,033/s** | 79.3% | **0** | 0.52 ms | 0.31 ms |

Offered 5× over the limit, the **allowed throughput converges to the bucket's discrete refill
schedule**: in `crates/host/src/backend.rs` (`apply_bucket`), the first request on a fresh key initializes `(tokens = capacity, last_refill = t_0)` and refills occur only at discrete integer multiples of `refill_interval_ms = 1,000 ms` (`intervals = now_ms.saturating_sub(last) / refill_interval_ms`). Over a window of $W$ seconds under continuous overload, the bucket therefore grants `capacity + (W - 1) × refill_tokens` requests:
- **30 s T2 report window**: `2,000 + 29 × 1,000 = 31,000` allowed requests (`31,000 / 30 s = 1,033.33 req/s`), **the same 1,033/s as every prior snapshot**.
- **10 s T1 gate window**: `2,000 + 9 × 1,000 = 11,000` allowed requests (`11,000 / 10 s = 1,100.0 req/s`). Historically, `gate_verdict.py` divided by a continuous approximation `(capacity + W × refill) / W = 1,200.0 req/s`, which is why every historical `gate.csv` recorded `enforce_allowed_ratio = 1,100 / 1,200 = 0.9166`; `gate_verdict.py` now uses the exact discrete-tick formula `(capacity + (W - 1) × refill) / W = 1,100.0 req/s` (`ratio = 1.0000`; `0.9999` in this snapshot's `gate.csv`).
- **Operator guidance on refill granularity:** Because `refill_interval_ms = 1,000` grants all 1,000 refill tokens at once on each 1-second boundary under continuous overload, operators who want smoother sub-second pacing to upstream backends can configure a finer `refill_interval_ms` with the same rate (for example, `refill_tokens = 10` and `refill_interval_ms = 10` for 1,000 tokens/s).

The accounting is complete this pass: 31,000 allowed + 119,005 shed = **150,005**,
accounting for the 5,000/s × 30 s offered load, with **zero status-less responses** and **79.3 % shed**
(accept p50 `0.10 ms` / `0.099 ms`, accept p99 `0.52 ms` / `0.517 ms`, 429 p99 `0.31 ms` / `0.310 ms` — tighter accept tails than `v0.16.1`'s `0.72 ms` p99).

> **This row was contaminated on 2026-07-20 and is repaired here (2026-08-15).** That pass reported
> 59.6 % shed because `bench/k6-wasm/ratelimit-enforce.js` counted only `status === 200` and
> `status === 429`, dropping everything else — and its `preAllocatedVUs` (`max(200, RATE/10)` =
> **500** for a 5,000 req/s offer) was almost double ADR 000092's 256-per-IP cap, so **48.8 % of
> attempts produced no HTTP status at all** and vanished from both counters. The pool is now bounded
> below the cap and status-less responses have their own tracked `no_status` bucket. The repaired
> figure — **79.3 %** — reproduces the pre-cap 07-11 snapshot's 79.3 % exactly, which is the
> strongest available evidence that the cap interaction, not the proxy, was the whole story.

### Fairness — one key cannot starve another

![Rate-limit fairness](img/ratelimit_fairness.png)

> R3 — same tight bucket; two keys concurrently: a **hot** key offered 4000/s and a **light** key
> offered 500/s (k6).

| key | offered | allowed (200) | shed | status-less |
| --- | --- | --- | --- | --- |
| hot | 4,000/s | 1,033/s | 74.2% | **0** |
| light | 500/s | **500/s** | 0% | **0** |

State is **per key**, and this pass measures it end to end rather than inferring it: the hot key is
throttled to **its own** refill rate (1,033/s — the same figure the single-key
[enforcement](#enforcement--does-it-actually-hold-the-rate) run produces, so the light key's traffic
costs it nothing), while the light key **receives every request it offers** — 500/s offered, 500/s
allowed, zero 429s, zero status-less. A hot neighbour cannot starve a light one.

> **The 07-20 pass could not show this** (light read 145/s against a 500/s offer, with 0 % shed):
> the two scenarios share one k6 process and one loopback source IP, and `hot`'s pool alone
> (`max(200, 4000/10)` = **400**) exceeded ADR 000092's 256-per-IP cap, so the light key's
> connections were refused at accept — starvation by the harness's own connection budget, dressed
> up as a fairness result. With both pools bounded below the cap (150 hot / 50 light) the intended
> claim is now the measured one.

## Request body handling

The request-side **body hook** (`on-request-body`, ADR 000025) follows a *buffer-then-decide* model:
for a filtered route carrying a body, the host buffers it (bounded — 16 MiB cap, fail-closed 413),
runs the filter's `on-request-body`, and forwards the possibly-transformed body — or short-circuits
before upstream. `filter-hello` uppercases the body (a real transform) or 403s on a `deny-body`
marker. A bodyless request, a filter-less route, and — since ADR 000038 — a route whose filters are
**all header-only** (none exports `on-request-body`) keep the zero-copy streaming path: the host
decides from the component's exports whether any filter reads the body, and buffers only then.

![Request body hook](img/body.png)
![Request body hook, tail latency](img/body_tail.png)

> B — 50 VUs, POST a `SIZE`-byte body at 1 KB / 100 KB / 1 MB (k6), to `/body` (filter-hello buffers +
> transforms), `/body-headeronly` (a header-only filter — body streams through, ADR 000038), and
> `/baseline` (no filter). Default allocator arena cap of **32** ([ADR 000118](../docs/ADR/000118.md), amending ADR 000038's earlier cap 4).

| size | route | req/s | throughput | p99 |
| --- | --- | --- | --- | --- |
| 1 KB   | /baseline        | 120,681 | 124 MB/s  | 1.73 ms |
| 1 KB   | /body            | 110,866 | 114 MB/s  | 1.35 ms |
| 1 KB   | /body-headeronly | 123,165 | 126 MB/s  | 1.65 ms |
| 100 KB | /baseline        | 38,918  | 3985 MB/s | 4.40 ms |
| 100 KB | /body            | 18,200  | 1864 MB/s | 6.02 ms |
| 100 KB | /body-headeronly | 38,795  | 3973 MB/s | 4.34 ms |
| 1 MB   | /baseline        | 5,685   | 5961 MB/s | 31.0 ms |
| 1 MB   | /body            | 2,002   | 2099 MB/s | 45.6 ms |
| 1 MB   | /body-headeronly | 5,719   | 5997 MB/s | 31.3 ms |

*(Re-measured 2026-11-14 (tag **v0.16.2**, `HEAD` `0d7bb08`, Rust `1.100.0`, pre-run `contended=0`).)*

A filter that **reads** the body pays for it, growing with payload: **~8.1 % throughput at 1 KB** (down from `22.2 %` in `v0.16.1`, as [ADR 000124](../docs/ADR/000124.md) narrows the fixed dispatch and filter tax on small payloads), **~53.2 % at 100 KB**, **~64.8 % at 1 MB** (a
full-body copy + uppercase per request). A **header-only filter takes the zero-copy bypass** — the
body never enters guest memory: at 1 MB it lands **within `+0.6 %` of `/baseline`** (ADR
000038 — the two paths are within run-to-run noise at this size); at 100 KB
it lands within **`−0.3 %`** of `/baseline` (`38.8k` vs `38.9k`; `VUS=50`, well under the per-IP cap); and at 1 KB (`+2.1 %`, `123.2k` vs `120.7k`, single-window k6 variance vs `−10.9 %` in `v0.16.1`) the lowered dispatch floor puts `/body-headeronly` within single-window k6 noise of `/baseline`. RSS at 1 MB × 50 VUs (fresh
proxy per route, default arena cap 32 per [ADR 000118](../docs/ADR/000118.md); proxy + in-process upstream, one mid-load sample): **~138 MB `/baseline` (`137,564 kB`) · ~290 MB `/body` (`290,264 kB`) · ~121 MB
`/body-headeronly` (`120,908 kB`)**
(`data/body_rss.csv`). The export-presence bypass keeps a header-only route near baseline. The buffer
stays bounded (16 MiB cap, fail-closed 413) for the filters that do read the body. The remaining
buffered-path copy is addressed by the opt-in `stream<u8>` streaming-body capability (ADR 000020 / ADR 000121); a per-request
time-series / allocator-sweep decomposition lives in `bench/perf/mem_matrix.py`. For choosing an
arena default, use `bench/perf/arena_sweep.py`: it isolates the upstream in a separate process and
records proxy-only loaded peaks and post-load retention for every candidate. The snapshot above
is neither a proxy-only peak nor a post-load retention measurement; see the
[measurement procedure](../bench/methodology.md#2026-09-27--アリーナ既定値を判断するための分離計測t3).

## Footprint

Idle resident set and the marginal cost of an open connection (`bench/harnesses/bench-server`):

| Metric | Value |
| --- | --- |
| idle RSS | ~43 MB |
| RSS holding keep-alive connections | ~49 MB (250 conns) |
| marginal bytes / connection | ~25.9 KB |

*(Re-measured 2026-11-14 (tag **v0.16.2**, `HEAD` `0d7bb08`, Rust `1.100.0`, pre-run `contended=0`). The phase asks for **250** connections (under
ADR 000092's 256-per-IP cap, so every connection it counts is one the proxy actually holds) and
divides by the count the generator reports as open. At 25.9 KB/conn (25,887 bytes/conn) the
figure stays in the expected ~24.8–25.9 KB/conn band, and idle RSS (~43 MB, 42,868 kB; 49,188 kB with 250 connections) tracks expected baseline memory.)*

---

# 3. Realistic & protocol coverage

## Weighted request mix — with its own baseline

> M1 — open-loop 20k req/s, a weighted blend across routes on one gateway (k6): read-heavy, partly
> edge-checked (per-tenant rate-limit keys, 200 tenants, never-deny bucket), occasional writes,
> rare large payloads. Paired with a **read-only control at the same arrival rate** — 100 % plain
> reads — so the per-class deltas are attributable to the traffic *blend*, not the offered load.

| Profile | Class (share) | route | p50 | p99 | p99.9 |
| --- | --- | --- | --- | --- | --- |
| read-only (control) | read 100 % | GET `/baseline` (1 KB) | 0.061 ms | 0.998 ms | 8.596 ms |
| mix | read 60 % | GET `/baseline` (1 KB) | 0.067 ms | 5.429 ms | 13.852 ms |
| mix | auth read 25 % | GET `/ratelimit` (tenant key) | 0.080 ms | 5.505 ms | 14.095 ms |
| mix | write 10 % | POST `/body` (1 KB) | 0.099 ms | 6.140 ms | 15.424 ms |
| mix | large 5 % | POST `/body` (100 KB) | 0.387 ms | 8.238 ms | 17.366 ms |

*(Re-measured 2026-11-14 (tag **v0.16.2**, `HEAD` `0d7bb08`, Rust `1.100.0`, pre-run `contended=0`; `loadavg1` rose to `5.48` by end of `mix`).)*

Both profiles target ~20k/s offered (from the run log: `19,989` rps read-only with `1,492` dropped iterations (`≈0.25 %`) out of ~600,000; `19,965` rps mix with `2,695` dropped (`≈0.45 %`); `0` 429s from the never-deny bucket, **zero status-less responses** — the honest
open-loop shed signal under the cap, see the note below). In k6's `constant-arrival-rate`, a dropped iteration means the 240-VU pool was momentarily exhausted; compared to the idle `2026-10-24` host (`0.191–0.683 ms` p99, `433` / `0` dropped), the elevated ms-scale upper tails (`0.998–8.238 ms` p99, `8.596–17.366 ms` p99.9) and dropped counts track host scheduling pressure accumulated late in the back-to-back T2 run (`loadavg1` `3.05 → 5.48`), while p50 improves over `v0.16.1` across read/auth/write (`0.061–0.099 ms` vs `0.064–0.107 ms`) and stays sub-millisecond across all classes (`0.061–0.387 ms`, vs `0.057–0.352 ms` idle). Because k6 only counts dropped iterations and does not record their latency, percentiles above roughly p99.5 are biased low.
Comparing the two single-window runs gives a reference difference of `+4.431 ms` at p99 (`0.998 → 5.429 ms`) and `+5.256 ms` at p99.9 (`8.596 → 13.852 ms`) on the plain reads — reflecting head-of-line pressure
from the body classes alongside window-to-window load variance — while within the mix window itself, the classes order strictly as their work predicts (read < auth read < write <
large, monotone in p50, p99, and p99.9: `0.067 < 0.080 < 0.099 < 0.387 ms` p50, `5.429 < 5.505 < 6.140 < 8.238 ms` p99, `13.852 < 14.095 < 15.424 < 17.366 ms` p99.9). A single-endpoint test hides all of this; the control run
keeps it honest.

> **The 07-20 figures for this section were diluted; these are not (2026-08-15, reconfirmed 2026-09-25 / 2026-10-24 / 2026-11-14 (`v0.16.1` & `v0.16.2`)).** That pass's
> `record()` folded **every** response's duration into the latency Trends regardless of status,
> while its VU pool (**500**, for a 20,000 req/s offer) sat above ADR 000092's 256-per-IP cap — so
> near-instant connection refusals were pulling the published percentiles *down*. `weighted-mix.js` now drops status-less responses into their own counter
> before the trend, and holds its pool at **240** — just under the cap. Running this phase materially above 20k req/s from a single source IP is not
> possible on this harness without crossing the cap.

## HTTP/3

The fast path terminates **HTTP/3 over QUIC** (ADR 000016; `tls-http` serves h1/h2/h3 on one port). A
functional check confirms it end-to-end:

```
curl --http3-only https://…/api/hello  ->  status=200 http_version=3
```

*(Re-confirmed 2026-11-14 on tag **v0.16.2**, `HEAD` `0d7bb08`.)*

A **rigorous, coordinated-omission-safe H3 *load* benchmark is deferred**: default/distro `oha` builds and `k6` omit native
HTTP/3 (`oha` supports `--http-version 3` experimentally only when compiled from source with `--features http3`, and `h2load --alpn-list=h3` is closed-loop concurrency-based where `--rate` paces connection creation rather than open-loop request arrivals; adding an H3 subcommand to `plecto-loadgen` or using a custom `oha --features http3` build is tracked for T3). Rather than publish process-spawn-bound `curl`-loop numbers, the H3 load figure
stays absent until that tooling is wired into the harness — server support is verified, not throughput.

## WebSocket Upgrade tunnel (ADR 000048)

Plecto Proxy's HTTP/1.1 Upgrade path (ADR 000048): a route declaring `[route.upgrade] protocols =
["websocket"]` forwards the client's handshake (controlled re-issue — hop-by-hop stripping stays
the default for every other route), and on the upstream's 101 the proxy splices the two connections
into an opaque bidirectional byte tunnel — the same post-upgrade relay shape used by typical
L7 `Upgrade` / TCP tunnel modes. This is a **different load shape**
than every other scenario in this report: a long-lived, stateful connection instead of a
short-lived request, so it exercises axes nothing else here does — connection-permit accounting
(the connection admission guards, circuit breaker, and least-request in-flight counters follow the tunnel for its whole lifetime,
not just the handshake) and an activity-based idle timeout, rather than throughput-per-request.

`bench-server`'s `/ws` route tunnels to a dedicated mock upstream that completes the RFC 6455
handshake and echoes every frame; `plecto-loadgen`'s `ws` subcommand drives three sub-scenarios
(`bench/perf/run-perf.sh`'s `ws` phase):

- **Handshake rate** — open-loop paced Upgrade attempts/sec (the 101 handshake runs through the full
  filter chain like any other request, so it pays the same per-request cost the rest of this report
  measures — only the post-101 tunnel is new).
- **Tunnel footprint** — RSS held by concurrently open (idle) tunnels, the long-lived-connection
  analogue of [Footprint](#footprint)'s keep-alive connection measurement.
- **Echo throughput** — sustained request/response frames per held tunnel, at two payload sizes
  (1 KB / 64 KB), closed-loop per connection (the same concurrency model oha's `-c N` uses).

> **Re-measured 2026-11-14 (tag `v0.16.2`, `HEAD` `0d7bb08`, Rust `1.100.0`, pre-run `contended=0`); historical context from the 2026-10-25 fixes ([ADR 000122](../docs/ADR/000122.md)):**
> 1. **Why `hold --conns 1000` succeeded despite `MAX_CONNECTIONS_PER_IP = 256` ([ADR 000092](../docs/ADR/000092.md)):** In `crates/server/src/listener.rs`, `_permit` (`MAX_CONNECTIONS`) and `_ip_guard` (`PerIpConnGuard`) lived on `serve_conn`'s stack. Because `hyper`'s `serve_connection(...).with_upgrades()` returns as soon as the `101 Switching Protocols` response headers are written and hands the upgraded I/O to `OnUpgrade` (which runs in the detached `tokio::spawn` task inside `upgrade_switch`), `serve_conn` returned immediately after each 101 handshake and **dropped `_ip_guard` and `_permit` while the WebSocket tunnel stayed open**. Both guards are now bundled into `Arc<ConnGuards>` (`listener.rs` → `dispatch.rs` → `proxy.rs` `upgrade_switch`) and moved into the spawned tunnel task for the full tunnel lifetime, and `run-perf.sh` `phase_ws` now uses `--conns 250` (matching [Footprint](#footprint)) and divides by the actual held count.
> 2. **Why `idle_rss` in `ws_footprint.csv` was `76.5 MB` (vs `42.0 MB` in `footprint.txt`):** `phase_ws` previously reused one `bench-server` process across `10.1 handshake` (10,000 upgrades at 500/s) and `10.2 tunnel footprint`, so `idle_rss = 76,452 kB` was read on a heap that had just served the 10,000-upgrade burst. The most likely reading of the 34.5 MB excess is freed-but-retained allocator memory (more likely with arena cap 32), which new tunnel allocations then reuse — making the marginal `~8.8 KB/tunnel` an underestimate. That is an inference, not an allocator measurement. `phase_ws` now starts with `stop_proxy` and runs `10.2 tunnel footprint` **first** on a freshly launched `bench-server`.

| Scenario | Result |
| --- | --- |
| Handshake rate (2026-11-14, `v0.16.2`) | 10,000/10,000 Upgrades succeeded at the paced 500.0/s target — **0 % failed** over 20 s |
| Tunnel footprint (2026-11-14, `v0.16.2`, fresh process, measured first) | idle RSS 42.8 MB → 48.0 MB with **250 held tunnels** (`ws hold: 250 tunnels`, `42,812 → 48,028 kB`) — **~21.4 KB/tunnel** (`21,365 B`) |
| *(superseded: 2026-10-24)* | 76.5 MB → 85.0 MB across 1,000 tunnels (`8,778 B/tunnel`) — measured while upgraded tunnels escaped the per-IP cap, on a post-burst heap; not reproducible on the current tree |

![WebSocket echo throughput](img/ws_echo.png)
![WebSocket echo tail latency](img/ws_echo_tail.png)

| Payload | messages/s | throughput | p50 | p99 |
| --- | --- | --- | --- | --- |
| 1 KB  | 109,739 | 112 MB/s   | 0.32 ms | 1.72 ms |
| 64 KB | 48,236  | 3,161 MB/s | 0.98 ms | 2.15 ms |

The handshake rate holds at 100 % of the paced 500.0/s target (`10,000/10,000`, `0 % failed` over 20 s; note this validates zero-drop stability at 500 handshakes/s rather than measuring a maximum handshake ceiling). On a fresh process a held tunnel costs **~21.4 KB** of RSS — the same order as a held keep-alive connection in [Footprint](#footprint) (`~25.9 KB/conn`), and 2.4× the superseded post-burst `~8.8 KB`, which confirms that figure was an underestimate. For scale: each tunnel's relay (`tokio::io::copy_bidirectional`) eagerly allocates one 8 KiB buffer per direction (16 KiB), plus the two `Upgraded` I/O handles and the task itself. Echo throughput at 1 KB
(`109.7k msg/s`) is **2.28×** the 64 KB rate (`48.2k msg/s`) while aggregate byte throughput rises **28.1×**
(`112 → 3,161 MB/s`; charts regenerated from `ws_echo.csv`), consistent with a per-message dispatch floor that amortizes better over larger
frames (consistent with `v0.16.1`'s `109.7k / 48.7k msg/s`; relative to the idle `2026-10-25` run's `219.6k / 60.1k msg/s`, the `~20 %` drop at 64 KB is plausibly load/clock variance, whereas the `~2.0×` difference at 1 KB remains unattributed — consistent with the `2026-08-15` snapshot, where the 1 KB rung moved `~2.4×` without attribution). The *shape* — small frames dispatch-bound, large frames bandwidth-bound — is the
part that has held across every snapshot.

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
  invariants this report tracks (8 short paired rounds, rung order reversed on even rounds, so a common per-round level shift cancels inside each round and a linear host drift cancels in the sample mean across counterbalanced odd/even round pairs while remaining in $s$ as alternating-sign spread; `ci_half` is the 95 % Student-t half-width of the mean per-round delta, with `n` reported) and machine-checks them
  against `bench/perf/gate_tolerances.toml` — the bands are tracked in-repo, so a deliberate
  performance change is reviewed as a diff. The gate returns a **three-valued verdict**:
  - `pass` (exit code 0): every invariant's uncertainty interval `[value - ci_half, value + ci_half]` falls within its band `[band_lo, band_hi]`.
  - `fail` (exit code 1): at least one invariant's uncertainty interval falls completely outside its band (`value + ci_half < band_lo` or `value - ci_half > band_hi`). Exit code 1 dominates inconclusive.
  - `inconclusive` (exit code 2): an invariant straddles a band edge (neither fully inside nor fully outside). Re-run with `GATE_POOL_FROM=<that RUN_DIR>`: the new rounds are **pooled** with the earlier ones into one estimate (refused with exit 64 if `build_id.txt` differs or if more than 3 runs are pooled), so a re-run adds evidence instead of replacing an unwelcome result. Because round-level pooling treats rounds as independent and sequential stop-on-pass slightly inflates nominal coverage, pooling is capped at **at most 3 runs** (`n = 24`) and any band recalibration requires an independent non-pooled holdout run.
  (Invariants without `ci_half` keep binary pass/fail judgment; `info` rows such as `ratelimit_cpu_us_per_req` are reported, never judged, until they get a band.)
  `all` stays the human-read release snapshot.
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
# bench/perf/gate_tolerances.toml, written to performance/data/gate.csv.
# Exit code: 0 = pass (in band), 1 = fail (out of band), 2 = inconclusive (straddles edge => re-run).
# 75 = refused before launching anything: 1-min load average > GATE_MAX_LOAD (default 4.0) or
# PROXY_CPUS busy > GATE_MAX_PROXY_BUSY (default 0.20), sampled into $RUN_DIR/host_load.txt.
# Retry on an idle host, or GATE_ALLOW_CONTENDED=1 runs anyway and tags the verdict "(contended host)".
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
stretching, without a re-run. Charts are compiled with Flint (flint-chart) from the CSVs in performance/data:

```bash
cd performance/flint && npm ci && npm run plot
```

(The benchmark *method* — the runbook, scenarios, the Rust loadgen, plotting — is tracked, as are the rendered charts and this
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
- HTTP/3 load-testing references for the deferred H3 *load* benchmark (see [HTTP/3](#http3)):
- [h2load](https://nghttp2.org/documentation/h2load-howto.html) — nghttp2's closed-loop concurrency-based load generator; supports HTTP/3 (`--alpn-list=h3`) with qlog output (for open-loop H3 arrival pacing, see `oha --features http3` or a future `plecto-loadgen` H3 subcommand in [HTTP/3](#http3)).
- [wrk2](https://github.com/giltene/wrk2) — constant throughput with corrected latency recording.
- [Wasmtime](https://docs.wasmtime.dev/) — the pooling allocator and epoch interruption behind pooled vs on-demand filter instances.
- [WebAssembly Component Model](https://component-model.bytecodealliance.org/) — the `plecto:filter` contract is a Component Model world.
- [RFC 6455](https://www.rfc-editor.org/rfc/rfc6455) — the WebSocket protocol `bench-server`'s `/ws` mock upstream and `plecto-loadgen`'s `ws` subcommand implement (handshake + frame codec) to drive the Upgrade tunnel scenario.
