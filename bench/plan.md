# Plecto ベンチマーク計画 / runbook

Plecto の二つの半身——**native fast path**（接続・TLS・HTTP・routing・LB・upstream）と
**extension plane**（WASM フィルタ＋ host-API）——の性能を、**method を公開しつつ raw 出力は出さない**方針で
測る。結果（数値・グラフ・考察）は [`../performance/README.md`](../performance/README.md) が source of truth。
本ファイルは「何を・なぜ・どう測るか」の runbook。

## 方針（tenets）

- **canonical runner は1本**: [`perf/run-perf.sh`](perf/run-perf.sh)。core-pin（`taskset`）で proxy と
  generator を**互いに素な CPU 集合**に固定し、generator が proxy の core を奪わないようにする。
  出力は `performance/data/*.csv` → [`../performance/plot.py`](../performance/plot.py) → `performance/img/*.webp`。
- **負荷はこのマシン内で完結**: generator → proxy → in-process upstream はすべて loopback。telemetry は無効
  （`K6_NO_USAGE_REPORT=true`、Influx/Grafana の phone-home off）。**負荷実行時に外部へ通信しない**。
  Docker image / generator binary の**取得（setup）は外部 OK**——禁じるのは「負荷をかける最中の外部通信」。
- **絶対値ではなく invariant を読む**: host / clock 依存の絶対 throughput ではなく、**比・曲線形・時定数・
  µs/req・enforcement の収束**を回帰の signal とする。loopback は kernel short-circuit で latency を過小評価する
  ため、数値は下界・回帰用として扱う（`performance/README.md` の caveat 参照）。
- **open-loop を tail の権威に**: closed-loop（`constant-vus`）は throughput 天井、open-loop
  （`constant-arrival-rate`）は coordinated-omission-safe な tail。両者を併記し、tail は open-loop を採る。

## ツール

| 用途 | ツール | 備考 |
| --- | --- | --- |
| closed/open-loop・rate-limit・body | [k6](https://grafana.com/docs/k6/latest/) | PATH の k6 を使用（download しない） |
| single-route ceiling・TLS | [oha](https://github.com/hatoo/oha) | `~/.cargo/bin/oha`、軽い generator で proxy 天井を出す |
| round-robin・fault/swap timeline・WebSocket | `plecto-loadgen`（`bench/loadgen/`, Rust） | open-loop 自前ペーシング。GIL 律速だった旧 Python driver を代替（`rr` / `ejection` / `swap` / `hold` / `ws` サブコマンド） |
| 任意の live dashboard | InfluxDB + Grafana（`docker-compose.yml`） | `INFLUX=1` の時だけ起動（image 取得は setup） |

## シナリオカタログ

各シナリオの subject / generator / 出力 CSV / 読むべき invariant / 対応 ADR。

### Quick tier

| phase | subject | generator | 出力 | invariant |
| --- | --- | --- | --- | --- |
| `quick` | ceiling 1 点 + idle RSS の smoke check（~1 分、k6/Docker 不要） | oha | （非追跡・console のみ） | 動作確認のみ。回帰 baseline は `all` を使う |

### Fast path（native）

| phase | subject | generator | 出力 | invariant |
| --- | --- | --- | --- | --- |
| `ceiling` | plain HTTP/1.1 の canonical ceiling: keep-alive RPS + cold-connection CPS | oha（`--disable-keepalive`） | `ceiling.csv` | connection 再利用の価値。`wasm` / `tls` はこの数値を再利用し、自分では測り直さない |
| `sweep` | throughput/latency vs concurrency | k6 constant-vus（50–800 VU） | `sweep.csv` | 曲線形（plateau→graceful decline、cliff 無し） |
| `openloop` | coordinated-omission-safe tail（`sweep` の knee の 70% を自動導出） | k6 constant-arrival-rate | `openloop.json` | 飽和点での tail 発散 |
| `rr` | round-robin の均等性 | `plecto-loadgen rr`（X-Instance 集計） | `rr.csv` | 1 req 精度で 1/3 ずつ |
| `ejection` | health-eject + fail-closed（ADR 000017） | `plecto-loadgen ejection` | `ejection_*.csv` | ~1s 時定数・503 fail-closed・~1s 復帰 |
| `swap` | endpoint-set の swap under load（ADR 000044、reload が health ではなくアドレス集合自体を変える） | `plecto-loadgen swap`（`--exec-at` で manifest 書換 + SIGHUP を時刻指定実行） | `swap.csv`, `swap_events.csv` | ejection と同じ ~1s 時定数で新集合に追従（同じ `ArcSwap` 差し替え経路） |
| `tls` | TLS 終端の分解（h1/keepalive/handshake/h2） | oha | `tls.csv` | record-layer 安い・handshake 支配。`plain (h1)` 行は `ceiling.csv` を参照、測り直さない |
| `footprint` | idle RSS + bytes/conn | `plecto-loadgen hold` | `footprint.txt` | conn あたり限界バイト |

### Extension plane（WASM filter + host-API）

| phase | subject | generator | 出力 | invariant |
| --- | --- | --- | --- | --- |
| `wasm` | filter overhead（baseline/pooled/on-demand）+ short-circuit | oha + k6 | `wasm_overhead.csv`, `wasm_mixed.csv` | µs/req と pooling の価値（init 再払いの差）。`baseline` 行は `ceiling.csv` を参照 |
| `ratelimit` | host token-bucket（ADR 000026）: overhead / enforcement / per-key fairness | k6 | `ratelimit_{overhead,enforce,fairness}.csv` | 許可 rps が refill rate に収束・hot key が light key を starve しない |
| `body` | request-body hook（ADR 000025）の buffer-then-decide コスト + payload sweep | k6 | `body.csv` | hook の payload 比例コスト・streaming passthrough との対比 |
| `ws` | WebSocket Upgrade トンネル（ADR 000048）: handshake rate / tunnel footprint / echo throughput | `plecto-loadgen ws`（handshake/hold/echo モード） | `ws_handshake.csv`, `ws_footprint.csv`, `ws_echo.csv` | 双方向 splice が短命リクエストと根本的に異なる負荷特性を持つことを可視化。conn_limit / breaker permit / least-request in-flight のトンネル寿命分の会計 |

> `wasm` / `ratelimit` / `body` / `ws` はすべて単一の `bench/harnesses/bench-server`（旧 `wasm-bench` +
> `edge-bench` を統合、`filter-apikey` / `filter-hello` ベース）で駆動。bucket spec は manifest
> （`RL_CAPACITY` / `RL_REFILL_TOKENS` / `RL_REFILL_INTERVAL_MS`）で host 設定（ADR 000026）。enforcement/
> fairness は tight bucket、overhead は never-deny bucket を流す。`swap` は専用の `bench/harnesses/swap-bench`
> （4 instance、SIGHUP reload 配線）で駆動——`load-balancing` デモ自体は変更しない。

### Realistic & protocol coverage

| phase | subject | generator | 出力 | invariant |
| --- | --- | --- | --- | --- |
| `mix` | 重み付き request mix（60/25/10/5 read/auth/write/large）+ 同レート read-only 対照 | k6 constant-arrival-rate | `mix.csv` | blend のコストが offered load ではなく traffic 構成に帰属する |
| `h3` | HTTP/3 の機能確認のみ（負荷は deferred — oha/k6 に native H3 無し。h2load `--npn-list h3` か Nighthawk が候補） | curl `--http3-only` | `h3.txt` | status=200 http_version=3 |

## 実行

```bash
# 前提: release example を build（run-perf.sh は build しない）。bench-server/swap-bench は plecto/ 外
# （bench/harnesses/）を指すので --features bench-harnesses が要る。
cd plecto && cargo build --release -p plecto-server --features bench-harnesses \
  --example load-balancing --example bench-server --example tls-http --example swap-bench

# 1 phase か all。proxy を core 集合へ、generator を互いに素な集合へ pin して performance/data/*.csv を出力。
# phases: quick ceiling sweep openloop rr ejection swap wasm tls h3 ws footprint ratelimit body mix all
bash bench/perf/run-perf.sh all

# すぐ試したいだけなら quick（~1 分、oha のみ、k6/Docker 不要、CSV 非出力の smoke check）
bash bench/perf/run-perf.sh quick

# core 集合の override（自ホストの topology に合わせる）
PROXY_CPUS=0-7 GEN_CPUS=8-15 bash bench/perf/run-perf.sh sweep

# 任意: live dashboard（image 取得は setup、負荷は loopback のまま）
INFLUX=1 bash bench/perf/run-perf.sh all   # http://localhost:3000/d/plecto-lb-k6

# CSV からグラフ再生成
python3 performance/plot.py                # performance/data/*.csv -> performance/img/*.webp
```

## 統廃合の経緯

- canonical を `perf/run-perf.sh` に一本化。旧 `run-bench.sh` / `run-wasm-bench.sh`（k6+Grafana 専用 + arm64
  k6 download）と、それらだけが使っていた重複 k6 スクリプト（`k6/lb-load.js` / `lb-rate.js` / `lb-ejection.js`、
  `k6-wasm/route.js`）を撤去。後者は sweep-step / openloop / oha / `plecto-loadgen` が上位互換で代替する。
- Grafana/InfluxDB スタック（`docker-compose.yml` + `grafana/`）は**残置し `INFLUX=1` で opt-in**。run-perf の
  k6 phase が `--out influxdb` で流すので、回帰 baseline（CSV/charts）と live 観測の両方を1本の runner で賄う。
- **2026-07-04: `wasm-bench` + `edge-bench` を `bench-server` に統合。** 両者は同じ plain-HTTP/1.1 ルート
  （`/baseline`）を別プロセスで独立に測っており、`churn` phase を含めて実質同じものを3回測っていた
  （host noise の範囲でしか違わない ~228k–243k req/s の3値）。`ceiling` phase が一度だけ測り、`wasm` /
  `tls` はその数値を参照する（測り直さない）。あわせて ADR 000041–000048 で着地した新機能のうち計測空白
  だった2軸を追加: **`swap`**（ADR 000044、endpoint-set の swap under load。専用の `swap-bench` ハーネス
  — 4 instance + SIGHUP 配線、`load-balancing` デモ自体は変更しない）と **`ws`**（ADR 000048、WebSocket
  Upgrade トンネル。`bench-server` に `/ws` route + RFC 6455 mock upstream を追加、`plecto-loadgen` に
  `ws` / `swap` サブコマンドを追加）。`quick` phase（~1分・oha のみ）は「すぐ試せるシナリオ」として新設。
  criterion 側にも `pick_under_swap_churn`（`crates/control/benches/fastpath.rs`）を追加——ADR 000044 の
  per-pick `ArcSwap<Endpoints>` load を継続的な swap churn 下で単独計測する。

## 出力ファイル

- 追跡（コミット対象）: `performance/img/*.webp`・`performance/README.md`・`performance/data/*.txt` /
  `*.json`（`footprint.txt` / `h3.txt` / `openloop.json` — CSV ほど大きくない単発の結果）。
- 非追跡（regenerable working data、ルート `.gitignore` の `performance/data/*.csv`）: 各シナリオの CSV・
  生 k6/oha 出力・HTML・log・download binary・machine-spec 入りの write-up。
