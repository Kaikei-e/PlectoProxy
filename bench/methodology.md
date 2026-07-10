# Benchmark methodology — industry alignment

This document is the **method source of truth** for how Plecto Proxy measures performance.
Numeric snapshots live in [`../performance/README.md`](../performance/README.md); the runner is
[`perf/run-perf.sh`](perf/run-perf.sh).

## Web Research Report: L7 proxy / gateway load testing

### 調査目的
- 業界で「効いている」計測（coordinated omission 回避・RR/CRR・traffic mix・報告の透明性）を特定し、Plecto の既存ハーネスをそれに揃える／統廃合する。

### 要約
業界の権威ある形は **(1) 持続接続上のスループット (RR)** と **(2) 接続確立込み (CRR / CPS)** を分け、**(3) 固定到着レートの open-loop で tail を測る**（closed-loop 飽和時のレイテンシはサービス時間ではない）、**(4) アプリケーショントラフィック mix** を併記し、**(5) 方法を公開する**こと。Plecto は既に大半を持っていたが、open-loop の権威が k6 にあり **generator 天井で rate をピン留めする**状態だったため、**schedule-latency の `plecto-loadgen openloop` を権威に昇格**した。

### 公式ドキュメントからの発見
- **RFC 9411**（BMWG）: HTTP throughput、TCP/HTTP connections per second、transaction latency、application traffic mix。TLS 最悪ケースでは session reuse/resumption を切る（Plecto は `plecto-loadgen tls --mode full|resumed` で明示分解）。([RFC 9411](https://www.rfc-editor.org/rfc/rfc9411))
- **RFC 3511**（旧手法、RFC 9411 が obsolete）: HTTP/1.1 persistent vs non-persistent。([RFC 3511](https://www.rfc-editor.org/rfc/rfc3511))
- **k6 open vs closed models**: closed-loop は coordinated omission を起こし得る。open-loop は `constant-arrival-rate`。([k6 docs](https://grafana.com/docs/k6/latest/using-k6/scenarios/concepts/open-vs-closed/))
- **oha `--latency-correction`**: `-q` と併用したときだけ CO 補正が効く。([oha README](https://github.com/hatoo/oha))

### コミュニティ情報からの発見
- **wrk2**（Gil Tene）: 定数スループット＋**intended send time からのレイテンシ**が CO 回避の古典形。`plecto-loadgen openloop` が採用。([giltene/wrk2](https://github.com/giltene/wrk2), Tier S)

### 注意事項・落とし穴
- 飽和時の p99 をサービスレイテンシとして読まない。
- generator が先に溶けると tail は proxy ではなく generator の待ち行列になる。
- k6 latency は iteration 時間であり wrk2 schedule-latency とは定義が違う（`OPENLOOP_GEN=k6` は A/B 用）。
- HTTP/3 *load* は oha/k6 に native H3 が無く deferred（機能確認のみ）。
- loopback は latency を過小評価する。絶対値は回帰用下界。

### 推奨アクション（実装済み）
1. 権威 open-loop = `plecto-loadgen openloop`（schedule-latency）。`OPENLOOP_GEN=k6` で旧経路。
2. ceiling CSV に KPI 列 `RR` / `CRR`。
3. `industry` phase: ceiling + sweep + openloop + mix。
4. `REQUIRE_OFFLINE=1`: デフォルト IPv4 ルートがあると拒否。

### 情報の鮮度
- 調査日: 2026-07-11
- **確定事実**: CO 回避には open-loop＋（schedule 補正 or 十分な pre-alloc）が必要。RFC 9411 の KPI 分割。
- **projected**: H3 *load* KPI（ツール未同梱のため未計測）。

### Sources
| # | Title | URL | Tier | Note |
|---|-------|-----|------|------|
| 1 | RFC 9411 | https://www.rfc-editor.org/rfc/rfc9411 | S | L7 inline DUT KPI 形 |
| 2 | k6 open vs closed | https://grafana.com/docs/k6/latest/using-k6/scenarios/concepts/open-vs-closed/ | S | CO と arrival-rate |
| 3 | k6 constant-arrival-rate | https://grafana.com/docs/k6/latest/using-k6/scenarios/executors/constant-arrival-rate/ | S | open-loop executor |
| 4 | wrk2 README | https://github.com/giltene/wrk2 | S | schedule-latency |
| 5 | oha README | https://github.com/hatoo/oha | S | `-q` + `--latency-correction` |

## Plecto mapping

| Industry KPI | Plecto phase | Generator | Notes |
| --- | --- | --- | --- |
| HTTP throughput (persistent) | `ceiling` keep-alive / **RR** | oha | Canonical plain-h1 ceiling |
| Connections/s (CRR) | `ceiling` cold / **CRR** | oha `--disable-keepalive` | TCP/req |
| Transaction latency @ fixed RPS | `openloop` | **`plecto-loadgen openloop`** | Schedule-latency; authoritative |
| Closed-loop concurrency curve | `sweep` | k6 `constant-vus` | Ceiling shape, not tail authority |
| Application mix | `mix` | k6 CAR | RFC 9411 §7.1 shape |
| TLS full vs resumed | `tls` | oha + `plecto-loadgen tls` | Resumption isolated |
| Resilience time-constants | `ejection` / `swap` | loadgen | Plecto-specific |
| Extension-plane tax | `wasm` / `ratelimit` / `body` | oha + k6 | Adjacent-delta ladder |

## Offline policy

- **During a load run**: loopback only. `K6_NO_USAGE_REPORT=true`. No registry / CDN / phone-home.
- **`REQUIRE_OFFLINE=1`**: refuse if `ip -4 route show default` is non-empty.
- **`INFLUX=1`**: optional local dashboard only.

## Commands

```bash
bash bench/perf/run-perf.sh industry
OPENLOOP_RATE=60000 bash bench/perf/run-perf.sh openloop
OPENLOOP_GEN=k6 OPENLOOP_RATE=60000 bash bench/perf/run-perf.sh openloop
sudo unshare -n -- bash -c 'ip link set lo up; REQUIRE_OFFLINE=1 bash bench/perf/run-perf.sh industry'
```
