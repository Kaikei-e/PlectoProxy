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
| v0.3.0 response / compression | `v03` (opt-in, not in `all`) | oha | ADR 000073/074/075 in use |

## Measurement tiers — 「いつ・何のために回すか」

スイートは「何を測るか」ではなく「いつ・何のために回すか」で 4 層に分ける。窓長は
`run-perf.sh` 冒頭の TIER 表 1 箇所に集約されている（`TIER=gate|report`）。

| tier | phase | 時間 | 目的 | 判定 |
| --- | --- | --- | --- | --- |
| **T0 quick** | `quick` | ~1 分 | 起動 smoke。CSV 非出力 | 目視のみ |
| **T1 gate** | `gate` | ~6–7 分 | **変更ごとの回帰ゲート**。invariant の差分のみ | **機械判定**（`gate.csv` + exit code） |
| **T2 report** | `all` | ~22 分 | リリース snapshot の全網羅レポート | 人間が読む（`performance/README.md`） |
| **T3 deep** | `v03` / `tls --mode full\|resumed` / `mem_matrix.py` / PMU | opt-in | 原因究明・一次特性測定 | 仮説があるときだけ |

**決定表 — どの変更でどの tier を回すか**:

| 状況 | 回す tier |
| --- | --- |
| ホットパスに触るコード変更 | T1 `gate`（+ contract 変更なら instruction ベンチの baseline 比較） |
| リリース前 | T2 `all` + T1 `gate`（README snapshot 更新） |
| gate が band を外れた / 性能異常の調査 | T3 の該当 phase（v03 / tls 分解 / mem_matrix / PMU） |
| 新機能の一次特性（未計測の軸） | T3 に phase を足してから、invariant 化できるものを T1 へ昇格 |

### T1 gate の統計設計 — interleave が単発長窓に勝る理由

単発 30–60 s 窓は分散推定を持たない**点推定**で、run-to-run 変動（clock / thermal / 隣接負荷）
をそのまま食らう。warm-up 除外済みの loopback 定常状態では percentile のサンプリング誤差は
~10 s で既に無視でき、残る変動はホスト状態由来——これは窓を伸ばしても消えない（同じホスト状態を
長く見るだけ）。同じ総時間なら **短い窓 × 複数 round の A/B/C interleave** に振り替えるほうが、
遅いドリフトが round 内でペア化されて隣接差分から相殺され、round 間の広がり（mean ± half-range）
という信頼幅まで手に入る。反復をどのレベルに配分すべきかの一般論は Kalibera & Jones,
*Rigorous Benchmarking in Reasonable Time* (ISMM 2013) に従う。

gate の測定項目は performance/README.md が invariant と宣言しているものに 1:1 対応する:
dispatch floor / apikey cost（µs/req, interleave ×3）、固定レート tail p50（+ `resp-ctx`）、
rate-limit tax（interleave ×2）、enforcement 収束（バケット数学は 2–3 s で収束するので 10 s）、
RR 正確性、圧縮 ejection タイムライン（eject@10 / rejoin@18 / eject-all@26 / restore@32、40 s）。
判定帯は [`perf/gate_tolerances.toml`](perf/gate_tolerances.toml)（リポジトリ追跡——**性能の期待値
変更は PR でレビューされる**）、照合は [`perf/gate_verdict.py`](perf/gate_verdict.py)。

固定レート tail は report tier の「slowest rung の 60 % 自動導出」ではなく **2,000 rps 定数**。
自動導出は rung 構成が変わるたびに offered rate が変わり snapshot 間比較を壊す；gate は fresh
rung を測らないので knee の心配がなく、定数で全 snapshot が同一条件になる。

### micro 層の二本立て — wall-clock と命令数

criterion（wall-clock）は governor 非固定方針の下で日跨ぎ ±10–20 % ドリフトし得るため、
「ADR 表面コストが増えたか」の一次判定は **gungraun**（旧名 iai-callgrind、callgrind ベース）の
**命令数**で行う（周波数・温度・隣接負荷に不変。`--save-baseline` / `--baseline` の named
baseline と `--callgrind-limits 'ir=5%'` のソフトリミットを持つ）。wall-clock criterion は
「実時間でどうか」の参考として並設を維持する——IPC 劣化は命令数に出ないため、両方要る。
命令数層に**見えないもの**も明記しておく: (a) `spawn_blocking` ハンドオフ等のスケジューリング項
（命令ではなく待ち時間）、(b) mmap_lock 競合 / TLB shootdown のような並行時 knee（callgrind 下は
逐次実行）。criterion が既に開示している逐次実行の非対称性をそのまま継承するので、命令数の
不変＝実時間の不変ではない。
CI（`bench.yml`）はこの二本立てをそのまま反映する: criterion ジョブは informational のまま、
gungraun の instruction ジョブが **main push で baseline を保存し、PR を `ir=5%` ソフトリミットで
機械判定**する（命令数は shared runner のノイズを受けないため、hosted CI でも判定が成立する）。
criterion を CI の閾値判定に使わない方針は criterion 公式 FAQ の推奨どおり。
（[gungraun](https://github.com/gungraun/gungraun), Tier S;
[criterion FAQ](https://bheisler.github.io/criterion.rs/book/faq.html), Tier S）

### open-loop の分布記録

`plecto-loadgen openloop` は latencies を [HdrHistogram](https://github.com/HdrHistogram/HdrHistogram)
（固定フットプリント・記録数 ns）で保持し、percentile 数点に加えて **分布全体**を `--hist-out` で
ダンプする。p99 の動きが「二峰性（特定経路の出現）」か「裾の伸び（確率的競合）」かを追加測定なしで
切り分けるための一次データ。

### Sources（tiers）

| # | Title | URL | Tier | Note |
|---|-------|-----|------|------|
| 1 | Kalibera & Jones, Rigorous Benchmarking in Reasonable Time (ISMM 2013) | https://dl.acm.org/doi/10.1145/2464157.2464160 | A | 反復配分・効果量 CI |
| 2 | criterion.rs FAQ | https://bheisler.github.io/criterion.rs/book/faq.html | S | CI 閾値判定を避ける根拠 |
| 3 | gungraun (formerly iai-callgrind) | https://github.com/gungraun/gungraun | S | 命令数ベース決定的 micro・baseline・limits |
| 4 | HdrHistogram | https://github.com/HdrHistogram/HdrHistogram | S | 固定コスト分布記録 |

## v0.3.0 response / compression — measurement method

### 調査目的
- ADR 000073（response-context / `replace`）と ADR 000074/075（native compression）を
  **行使したときのコスト**を、既存 WASM ladder と同じ業界手法で測る方法を確定する。

### 要約
マクロは **adjacent-delta ladder（同一 backend・隣接差分で一コスト隔離）** + **closed-loop 天井（oha）** +
**固定レートの CO 補正 tail（oha `-q` + `--latency-correction`）**。µs/req を回帰信号とし、
baseline 移動で膨らむ % は副次。マイクロは criterion の **named baseline 比較**（同一ホスト・
コミット前後）で ADR 表面コストを切り分ける。compression は RFC 9411 §7.3 の「オブジェクトサイズ固定の
HTTP throughput」形を 1 サイズで採る。

### 公式ドキュメントからの発見
- **Adjacent isolation / CO-safe tails**: 既存 Plecto mapping（oha 天井 + `-q`/`--latency-correction`）
  がそのまま適用できる。固定レートは「slowest rung の 60 %」自動導出（WASM ladder と同型）。
  ([oha README](https://github.com/hatoo/oha), Tier S; [k6 open vs closed](https://grafana.com/docs/k6/latest/using-k6/scenarios/concepts/open-vs-closed/), Tier S)
- **wrk2 schedule-latency**: open-loop 権威は引き続き `plecto-loadgen openloop`。本 `v03` フェーズは
  単一路線天井比較なので oha で足りる（generator を増やさない）。
  ([giltene/wrk2](https://github.com/giltene/wrk2), Tier S)
- **criterion baselines**: `--save-baseline <name>` / `--baseline <name>` で静的参照点を保持。
  日跨ぎ絶対値比較ではなく、同一セッション相対比較で contract コストを切り分ける。
  noise threshold 既定 ±2 % — CPU governor 未固定ではそれ以上のドリフトがあり得る。
  ([criterion CLI](https://bheisler.github.io/criterion.rs/book/user_guide/command_line_options.html), Tier S;
   [analysis / T-test](https://bheisler.github.io/criterion.rs/book/analysis.html), Tier S)
- **RFC 9411 §7.3 / §7.4**: HTTP throughput はオブジェクトサイズを変えて持続可能な inspected
  throughput；latency は sustainable TPS 下で TTFB/TTLB。Plecto の `v03` compression 行は
  **1 サイズ（4 KiB text/plain）・gzip 固定**の throughput 天井 + 同レート tail（簡略形）。
  多サイズ sweep は未実施（フルベンチ回避）。([RFC 9411](https://www.rfc-editor.org/rfc/rfc9411), Tier S)

### 注意事項・落とし穴
- `replace` は合成ボディで upstream ペイロードを落とすため、full-throttle の µs/req は
  「guest の replace コスト」と「転送バイト削減」が混ざる。制御列は同じ forward 形状の
  `resp-ctx`（continue）対 `noop-pooled` を主に読む。
- 高レート固定 tail（slowest の 60 % が ~67k–92k のとき）の p99 はホスト膝付近でノイズ化し得る。
  この行では **µs/req + p50** を主信号、p99 は参考。
- oha は `Accept-Encoding` を `-H` で明示（自動圧縮クライアント挙動に依存しない）。
- criterion の日跨ぎ悪化は、atomic pick のような contract 無関係ベンチが同方向に動いていれば
  ホストノイズ仮説が強い（governor 未ロック方針の帰結）。

### 推奨アクション（実装済み）
1. `phase_v03` — `/noop-pooled` → `/resp-ctx` → `/resp-replace` + `/baseline` vs `/compress`（gzip）。
2. README に µs/req 列を併記；`v03` は `all` に入れない。
3. criterion ADR 切り分け手順を Reproducing に文書化（コミット前後の `--save-baseline`）。

### 情報の鮮度
- 調査日: 2026-07-11
- **確定事実**: CO 回避に open-loop または `-q`+latency-correction；criterion named baseline；
  RFC 9411 のオブジェクトサイズ付き HTTP throughput KPI。
- **projected**: compression の多サイズ / 多 codec（br/zstd）sweep；criterion pre-adr73 実測差分
  （コミットをまたぐ同一ホスト再計測はオペレータ作業）。

### Sources
| # | Title | URL | Tier | Note |
|---|-------|-----|------|------|
| 1 | RFC 9411 §7.3–7.4 | https://www.rfc-editor.org/rfc/rfc9411 | S | HTTP throughput / latency KPI |
| 2 | criterion CLI baselines | https://bheisler.github.io/criterion.rs/book/user_guide/command_line_options.html | S | save/compare baseline |
| 3 | criterion analysis | https://bheisler.github.io/criterion.rs/book/analysis.html | S | bootstrap T-test, noise threshold |
| 4 | wrk2 schedule-latency | https://github.com/giltene/wrk2 | S | CO 回避の古典形 |
| 5 | oha README | https://github.com/hatoo/oha | S | `-q` + `--latency-correction`, `-H` |
| 6 | k6 open vs closed | https://grafana.com/docs/k6/latest/using-k6/scenarios/concepts/open-vs-closed/ | S | CO と arrival-rate |

## Offline policy

- **During a load run**: loopback only. `K6_NO_USAGE_REPORT=true`. No registry / CDN / phone-home.
- **`REQUIRE_OFFLINE=1`**: refuse if `ip -4 route show default` is non-empty.
- **`INFLUX=1`**: optional local dashboard only.

## Commands

```bash
bash bench/perf/run-perf.sh gate  # T1: per-change invariant gate (~6-7 min, machine verdict)
bash bench/perf/run-perf.sh all   # T2: release-snapshot report (~22 min, report-tier windows)
bash bench/perf/run-perf.sh industry
bash bench/perf/run-perf.sh v03   # T3: ADR 000073/074/075 in-use costs only (~6 min)
OPENLOOP_RATE=60000 bash bench/perf/run-perf.sh openloop
OPENLOOP_GEN=k6 OPENLOOP_RATE=60000 bash bench/perf/run-perf.sh openloop
sudo unshare -n -- bash -c 'ip link set lo up; REQUIRE_OFFLINE=1 bash bench/perf/run-perf.sh industry'
```
