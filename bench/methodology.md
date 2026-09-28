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

### 依存 bump の再ベースライン手順

依存の bump が codegen を動かす場合（例: wasmtime の major——Cranelift の最適化パスが変われば
guest コードの命令数はホストの変更なしに動く）、命令数の差は「Plecto の回帰」ではなく上流の
差分なので、bump ブランチ上で **pre/post の named baseline を対で取る**。名前は `pre_<dep><major>`
（例: `pre_wt48`。gungraun の baseline 名は英数字とアンダースコアのみ）とし、得られた命令数 delta は
bump の ADR に記録する。T1 `gate` も同じブランチで回し、帯を外れた項目は
`perf/gate_tolerances.toml` の規約どおり**同一 PR で再センタリング**する（期待値変更をレビューに
乗せるのがこのファイルの目的）。注意すべきは CI の非対称性で、`bench.yml` の instruction ジョブは
main push の baseline を無審査に保存する——bump がマージされた時点で post 側が新しい基準になり、
pre 側は残らない。delta の記録を PR / ADR に残すことが、唯一のレビュー痕跡になる。

### 測定前の binary 鮮度ガード — 「その verdict はどの build の証拠か」

ベンチの結論は **測った binary についての証拠にしかならない**。`gate` の PASS を「今のコード」の
根拠として引用できるのは、測った `target/release/examples/*` が今のソースから build されている
場合だけである。実際に、旧リリース時点の examples が `target/` に残ったまま `gate` が走って
PASS し、それが新コードの根拠として引用された事故と、examples が丸ごと存在しないまま `all` が
走って全 phase が起動に失敗しながら **exit 0** で終わり（`h3.txt` には `status=000` という偽の
結果行まで書かれた）、存在しない HTTP/3 回帰を追いかけた事故が起きている。存在チェック
（`-x`）だけでは前者を、phase の戻り値を捨てる driver では後者を防げない。

`run-perf.sh` は phase が proxy を起動する前に、必要な release example が **(a) 存在し
(b) stale でない** ことを要求する（`require_fresh`。phase dispatch 直後の preflight と
`launch()` の両方で強制されるので、phase 側にコピーを置く必要はない）。違反は phase ではなく
**run 全体の hard-fail**（exit 3）で、その場で rebuild コマンドを表示する。

- **鮮度の基準時刻** = `plecto/` 配下の「その binary の中身になり得る」ソースの最新 mtime
  （`*.rs` / `*.toml` / `*.wit` / `*.lock`、tracked + untracked-not-ignored）。未コミットの編集も
  そのまま効く。`plecto/` 配下の doc 更新は基準に入れない（rebuild を要求する理由がない）。
  `tests/` / `benches/` / `spike/` も除外する——`cargo build --example` の build graph に無いので、
  要求した rebuild を cargo が no-op で終え、binary の mtime が動かず**ガードが永久に満たせなく
  なる**ため。
- **HEAD の commit 時刻は基準に使わない**。edit → build → commit → 測定は普通の流れで、commit を
  基準にすると毎回 false positive になる。
- **escape hatch**: `PLECTO_BENCH_ALLOW_STALE=1` を付けると鮮度判定だけを警告に落とす（存在
  チェックは落とさない）。用途は 2 つ——古い build を**意図的に**測る cross-build 比較と、
  build graph 外の変更で rebuild が no-op に終わる場合。常用は禁止で、この変数を付けて得た数値は
  「どの build か」を明記しない限り snapshot の根拠にできない。

あわせて phase の失敗は握り潰さない: 各 phase は `run_phase` 経由で走り、末尾に per-phase の
サマリ行を出し、**一つでも失敗した run は非ゼロで終了する**。`h3` phase は他 phase と同じ
health wait を使い、curl が到達できない（`status=000`）ときは tracked の
`performance/data/h3.txt` を**書き換えずに**失敗する。

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

## 生成器のサイズと DUT 自身の admission control

負荷生成器の同時接続数は「測りたい負荷」ではなく**生成器側の実装都合**で決まりがちで、これが DUT の
admission control と衝突すると、測定値は静かに壊れる。Plecto の fast path は一つの source IP が保持できる
同時接続を `MAX_CONNECTIONS_PER_IP` = 256 に制限する（ADR 000092、CWE-770/400）。ループバック実験では
生成器・proxy・upstream が同じ 127.0.0.1 を共有するため、**VU プールがこの上限を超えると超過分は accept
で拒否される** —— 拒否された接続は HTTP ステータスを返さないので、`status === 200` / `=== 429` で分岐する
スクリプトからは**存在ごと消える**。

規則は二つ:

1. **プールは上限未満に固定する。** 目的が admission control そのものでない限り、`preAllocatedVUs` /
   `maxVUs` は 256 未満に置く（本リポは 200–240）。Little の法則より、ループバックの sub-ms サービス時間
   では 20,000 req/s でも in-flight は数十のオーダーなので、大きなプールは元々測定的価値がない。プールが
   足りなければ k6 は `dropped_iterations` として報告する —— open-loop の正直な shed signal であり、
   静かな歪みより常に良い。例外は closed-loop `sweep` の VU 400/800 rung で、これは**上限を踏むことが目的**。
2. **ステータスのない応答は独立バケットに数える。** 「200 でない = 短絡された」と解釈すると、拒否された
   接続が filter の判断として計上される。各シナリオは `no_status` を別に数え、CSV に出す。**期待値は 0**
   であり、0 でない run はその行を無効とみなす。

この二点を欠いたまま 2026-07-20 に測った結果は、短絡 mix が 90/10 ではなく 76/24 に見え、rate-limit の
enforcement は offered の 48.8 % を失い、fairness の light key は 500/s のうち 140/s しか通っていないように
見えた（= starvation の誤検出）。2026-08-15 に両方を直したところ、いずれも設計値に戻り、shed 率は上限導入前
（2026-07-11）の 79.3 % と小数点まで一致した。**負荷生成器の設定は、それ自体が測定対象に対する仮定である。**

同じ理由で `footprint` phase も接続数を 250（上限未満）に落とし、bytes/conn の除数を「要求数」ではなく
**生成器が実際に開いたと報告した数**に変えた。要求数で割ると、拒否された分だけ小さい値を publish してしまう。

### 情報の鮮度

- 再検証日: 2026-08-15。手法本体は変更なし（下記の通りツール側の破壊的変更は本リポに当たらない）。
- **k6 v2.0**: 削除されたのは `externally-controlled` executor / `pause|resume|scale|status` /
  `k6 login` / `--no-summary` / `--summary-mode=legacy`、および REST API サーバの自動起動。本リポが使う
  `constant-arrival-rate` / `constant-vus` / `handleSummary` / `-q` / `--out influxdb` は現行のまま。
  ([Migrate to k6 v2](https://grafana.com/docs/k6/latest/get-started/migrating-to-v2/), Tier S)
- **RFC 9411**: Informational、obsoleted / updated ともに無し（現行）。
- **oha 1.14 / gungraun 0.19**: `-q` + `--latency-correction`、`--baseline=` / `--callgrind-limits`
  ともに現行仕様。ローカル runner と `Cargo.lock` の版一致を CI と同じ方式で確認。

## 2026-09-26 — Measurement hygiene update (pinning, arenas, retention, three-valued gate)

### 調査目的・改訂の背景
ローカル性能計測ハーネス（`bench/perf/run-perf.sh`）において、測定の衛生性（hygiene）と再現性を向上させるための改訂を実施した。

### 改訂内容
1. **Exec-time pinning とスレッド別アフィニティ検証**:
   - 従来は起動後の `taskset -cp` で親 PID のみリピンしていたため、tokio worker が全コアアフィニティを引き継ぐ競合（race）が生じていた。
   - `taskset -c "$PROXY_CPUS" env ... binary &` による exec-time pinning へ変更し、全ワーカースレッドが確実にマスクを継承するようにした。
   - 起動健全化後、`/proc/$PROXY_PID/task/*/status` の `Cpus_allowed_list` を読み取り、期待される `$PROXY_CPUS`（正規化集合で比較）と不一致のスレッドがあれば即座に exit 1 で失敗させる。
   - 各起動ごとにスレッド数・アフィニティ一覧を `$RUN_DIR/affinity.txt` にスナップショット記録する。
2. **物理コア境界での CPU 分割 (`cpu_split.py`)**:
   - 従来の論理 CPU 半分分割（SMT ペアの分断リスク）を廃止し、`/sys/devices/system/cpu/cpu*/topology/{physical_package_id,core_id}` から物理コアをグループ化して丸ごと割り当てる `cpu_split.py` を導入。
   - コア共有を `--check` で検知し警告を出力。ハイブリッド CPU（`/sys/devices/cpu_core` と `/sys/devices/cpu_atom`）の場合は各セットの内訳を note 出力する。
   - 開発ホスト（i7-13700K: 0-15 P-core 隣接 SMT ペア、16-23 E-cores）では、既定値 `PROXY_CPUS=0-11`, `GEN_CPUS=12-23`（GEN 側が P コアと E コアの混在）となり過去履歴と連続性を保つ。
3. **アロケータ arena 数の統一 (MALLOC_ARENA_MAX=4)**:
   - `launch()` において `MALLOC_ARENA_MAX="${BENCH_MALLOC_ARENA_MAX:-4}"` および `PLECTO_MALLOC_ARENA_MAX="${BENCH_MALLOC_ARENA_MAX:-4}"` を全起動に一貫して適用（`bench-server` も `plecto` バイナリ同様に `cap_malloc_arenas` を呼ぶ）。`phase_body` 限定指定を撤廃。
4. **生データ保持とホスト指紋 (`RUN_DIR` / `host.txt` / `just perf-archive`)**:
   - 揮発性 `mktemp -d` を廃止し、起動時に `RUN_DIR=performance/data/runs/<UTC yyyymmddTHHMMSSZ>-<git short sha>[-dirty]` を作成。
   - 各 phase の生 JSON、プロキシログ（`$RUN_DIR/logs/<example>-<seq>.log`）、出力 CSV コピー（`$RUN_DIR/csv/`）を保存。
   - ホスト指紋 `$RUN_DIR/host.txt`（UTC 時刻、git revision + dirty、OS / カーネル major.minor、メモリ量、lscpu の許可リスト項目とトポロジ、scaling_governor、boost/turbo、smt、アフィニティ分割、ツールバージョン）を記録。Release に添付される前提で許可リスト方式にしており、ホスト名・カーネルのビルド文字列・脆弱性ごとの緩和状態は載せない（署名を行うマシンのパッチ水準を公開しないため）。緩和策は性能に効く唯一の区別として、`/proc/cmdline` の `mitigations=` 上書きの有無だけを残す。
   - `just perf-archive` により run ディレクトリを `.tar.gz` に固めて GitHub Release 添付用にパッケージ可能にした。
5. **3 値判定ゲート (T1 gate)**:
   - `gate_verdict.py` は保守的不確実性幅 `[value - ci_half, value + ci_half]` を評価し、3 値を返す:
     - `pass` (exit code 0): 不確実性区間が許容帯 `[band_lo, band_hi]` に収まる。
     - `fail` (exit code 1): 区間が帯の外側に外れる（fail は inconclusive に優先）。
     - `inconclusive` (exit code 2): 帯の境界を跨ぐ。`gate INCONCLUSIVE — re-run` を出力し再計測を要求（自動再実行は行わない）。
   - `Report.dump` のサマリ行は stderr へ出力し、`gate.csv` の純粋な CSV フォーマットを維持する。

### 比較可能性についての注意
2026-09-26 以前の測定値は、部分的なアフィニティ割り当ておよび body 以外の非キャップ arena 下で取得されたため、**footprint KB/conn** および **gate の分散幅（spreads）** は本改訂以降の数値と直接比較できない。新世代の基準点として扱う。

## 2026-09-27 — アリーナ既定値を判断するための分離計測（T3）

`run-perf.sh body` の RSS は **proxy と同一プロセス内 upstream の合算を負荷中に一度採った値**であり、
proxy 単体の peak / 負荷後の保持量ではない。アロケータの既定値を変更する判断には
`bench/perf/arena_sweep.py` を使う。T1 gate の帯は既存条件の回帰検知に使い、既定値の選択基準にはしない。

- proxy / upstream / generator を別プロセス・互いに重ならない物理コアに固定する。基準ホストの既定は
  `PROXY_CPUS=0-11`, `UP_CPUS=16-19`, `GEN_CPUS=12-15,20-23`。異なるホストでは3組を明示指定する。
- proxy の `PLECTO_MALLOC_ARENA_MAX` だけを変更する。実バイナリと同じ起動時 `mallopt` を通し、
  exec 時の `MALLOC_ARENA_MAX` は追加しない。継承した malloc 環境変数と `glibc.malloc.*` tunable を除去し、
  upstream のアロケータ条件は全セルで同じにする。
- RSS は各 `(N, route, round)` で新しい proxy を起動し、`smaps_rollup` を200msごとに採る。
  1MiB × 50 VU、warm-up 5秒＋計測15秒の負荷中 peak と、その後15秒の無負荷期間の最後2秒平均を
  `settled` として別々に保存する。settled は15秒後の観測値であり、長期定常値の保証ではない。
- 負荷中のスレッド数、全セルの時系列、生JSON、各プロセスのログ、設定・glibc/ツール版・バイナリSHA256を保存する。
  生成器の失敗・HTTP失敗・RSS取得失敗は測定失敗とし、ゼロ値で補完しない。
- N=4 / 16 / 24 / 32 / 0（glibcに委ねる）を昇順・降順の2巡で比較する。スループットは同じ分離構成で
  `/baseline` / `/noop-pooled` / `/trusted` を3回インタリーブし、rate-limit はk6の1000 keyで2回比較する。
  各巡に upstream 直接負荷の対照を置き、生成器・upstream が律速していないかも読む。
- この分離構成の絶対値は従来の同居構成とは直接比較しない。p99は閉ループ飽和時の待ち時間であり、
  固定レートのサービス遅延とは区別する。

```bash
cd plecto
cargo build --release --locked -p plecto-server --features bench-harnesses \
  --example bench-server --example upstream
cd ..
python3 bench/perf/arena_sweep.py --phase memory \
  --out performance/data/runs/arena-isolated-memory
python3 bench/perf/arena_sweep.py --phase throughput \
  --out performance/data/runs/arena-isolated-throughput
```

出力先は未作成のディレクトリを指定する。`summary.csv` はセル完了ごとに保存し、全条件の成功時だけ
`complete` ファイルを作る。通常の計測器テストは `python3 -m unittest discover -s bench/perf -p 'test_*.py'`。

## 2026-09-28 — アリーナ既定値改定（4→32）に伴うハーネス起動と環境変数の統制

分離計測（2026-09-27）の結果を受け、glibc malloc アリーナ上限の出荷既定値を 4 から 32 へ移行する作業に伴い、ベンチマークハーネスおよび計測ランナーの統制を実施した。

1. **4 ハーネスでの起動前上限呼び出しの統一**:
   - `bench-server` だけでなく、通常 proxy 起動を行う 4 つのハーネス（`bench-server`, `load-balancing`, `tls-http`, `swap-bench`）すべてにおいて、Tokio ランタイム生成（`#[tokio::main]` 等）より前に `plecto_server::cap_malloc_arenas()` を呼ぶよう統一。起動経路の違いによる上限の漏れや数値の二重管理を解消した。
2. **`run-perf.sh` の環境変数注入と衛生化**:
   - `BENCH_MALLOC_ARENA_MAX` 未指定時は `PLECTO_MALLOC_ARENA_MAX` 環境変数を設定せず、バイナリ内の出荷既定（Rust default 32）を直接利用する。
   - 明示指定時（4, 16, 24, 32, 0）は `PLECTO_MALLOC_ARENA_MAX` のみを注入し、プロセス起動時の `MALLOC_ARENA_MAX` は直接設定しない（Plecto の起動時 `mallopt` 経路を通す）。
   - 親環境から継承したアロケータ環境変数（`MALLOC_*` および `GLIBC_TUNABLES` 内の `glibc.malloc.*`）を清掃・除去し、`LD_PRELOAD` を明示的に拒否する。
3. **出荷既定と明示指定の比較 (`arena_sweep.py`)**:
   - `arena_sweep.py` は `--arenas default,32` を受け付け、環境変数を設定しない出荷既定（`default`）と、環境変数で明示指定した `32` の実測比較ができる。
4. **実装・検証状況**:
   - **コード検証・ビルド**: ユニットテスト 4 件 GREEN、cargo test 67 suites 746 passed、clippy green、4 ハーネス＋upstream の release build、Python テスト 33 件 pass。ADR は [ADR 000118](../docs/ADR/000118.md) として記録済み。
   - **T3 分離計測（出荷既定 default 対 明示 32 の 2 巡）**: 両条件で `complete` を得て正常完了（HTTP 失敗なし）。2 巡平均の観測値は以下の通り（※ baseline は oha/k6 で負荷モデルが異なるため合算平均は算出しない。2 巡の差は実測比較のための記録であり、統計的同等性を検証・保証するものではない）:
     - `/trusted` 認証: 96,281.3 / 94,982.7 rps
     - `/ratelimit`: 82,305.3 / 81,917.9 rps
     - `/body`: 2,090.1 / 2,098.5 rps
     - body 経路 proxy 単体 peak RSS: 256.51 / 256.00 MiB
     - 負荷停止後 15 秒 RSS: 185.93 / 199.21 MiB
   - **T1 回帰ゲート測定と apikey 帯較正**:
     - 同一 release binary / 現行 runner による出荷既定（default32）の T1 gate 2 巡（生データ: `performance/data/runs/gate-default-run1-20260928`, `gate-default-run2-20260928`）: run1 は `dispatch_floor_us` 4.5397 ± 0.0540 µs (pass) / `apikey_cost_us` 1.4053 ± 0.0716 µs (旧帯 0.3..1.2 に対して fail)、run2 は 4.5860 ± 0.0600 µs (pass) / 1.4210 ± 0.0451 µs (fail)。両 run とも他 9 項目は pass（micro 層は criterion baseline main 不在で情報層 skip）。
     - 同一条件での glibc 既定対照（`BENCH_MALLOC_ARENA_MAX=0`、生データ: `performance/data/runs/gate-glibc-control-20260928`）: `apikey_cost_us` は 1.1052 ± 0.1744 µs となり、上限 1.2 をまたいだため PASS ではなく **INCONCLUSIVE (exit code 2)** を記録（他 9 項目は pass）。
     - これらの実測値は特定セットアップ下での観測結果であり、単一の cap 0 対照やホスト変動を考慮すると、数値変動の要因をアリーナ上限のみへ排他的に因果帰属できるものではない。既定値 32 の採用自体は前日 T3 のスループット／RSS トレードオフに基づき決定済みであり、ゲート帯は選定基準ではなく既存期待値の回帰検知用である。基準ホストでの運用上の再較正（統計的信頼区間の保証ではない）として、default32 実測の最大中心値 1.421 + 3 × 最大 half-range 0.0716 ≈ 1.636 を切り上げ、`bench/perf/gate_tolerances.toml` の `apikey_cost_us` 上限を 1.2 → 1.7 へ改定（lower 0.3 および他全帯・判定方式は維持）。
   - **Holdout 検証完了（T1 gate holdout）**: 改定後の `gate_tolerances.toml`（`apikey_cost_us` 上限 1.7）に対し、独立した `explicit32` の T1 gate holdout 実測（生データ: `performance/data/runs/gate-explicit32-validation-20260928`）が完了し、ゲート正常終了（exit code 0、10 pass, 0 fail, 0 inconclusive）を確認した。
     - 実測値: `dispatch_floor_us` 4.5796 ± 0.1892 µs (pass), `apikey_cost_us` 1.2994 ± 0.2035 µs (pass; 新帯 0.3..1.7 内), `ratelimit_tax_us` 4.3764 ± 0.0745 µs (pass)。固定レート tail p50（pooled 0.1262 ms, apikey 0.0146 ms, respctx -0.0288 ms）、`enforce_allowed_ratio` 0.9166、`rr_spread_req` 0、`ejection_transition_s` 1 s、`ejection_stray_failed` 0 も全項目 pass（informational: pooled p99 0.1517 ms, respctx p99 -0.0009 ms, enforce limited 0.7800; criterion micro は保存 baseline main 不在のため情報層 skip）。

## Offline policy

- **During a load run**: loopback only. `K6_NO_USAGE_REPORT=true`. No registry / CDN / phone-home.
- **`REQUIRE_OFFLINE=1`**: refuse if `ip -4 route show default` is non-empty.
- **`INFLUX=1`**: optional local dashboard only.

## Commands

どの phase も、必要な release example が存在し stale でないことを先に要求する（上の
「測定前の binary 鮮度ガード」）。build は別コマンド:

```bash
cd plecto && cargo build --release -p plecto-server --features bench-harnesses \
  --example load-balancing --example bench-server --example tls-http --example swap-bench
```

```bash
bash bench/perf/run-perf.sh cpus  # CPU split & hybrid topology check only
bash bench/perf/run-perf.sh gate  # T1: per-change invariant gate (~6-7 min, machine verdict)
bash bench/perf/run-perf.sh all   # T2: release-snapshot report (~22 min, report-tier windows)
bash bench/perf/run-perf.sh industry
bash bench/perf/run-perf.sh v03   # T3: ADR 000073/074/075 in-use costs only (~6 min)
OPENLOOP_RATE=60000 bash bench/perf/run-perf.sh openloop
OPENLOOP_GEN=k6 OPENLOOP_RATE=60000 bash bench/perf/run-perf.sh openloop
sudo unshare -n -- bash -c 'ip link set lo up; REQUIRE_OFFLINE=1 bash bench/perf/run-perf.sh industry'
PLECTO_BENCH_ALLOW_STALE=1 bash bench/perf/run-perf.sh gate  # 意図的に古い build を測るときだけ
just perf-archive                 # performance/data/runs/ 最新 run を tar.gz アーカイブ
```
