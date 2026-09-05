---
name: flint-chart
description: >-
  Authors Flint ChartAssemblyInput specs and renders them via Flint MCP for Plecto
  observability and performance data (admin /metrics RED, access logs, bench CSVs).
  Writes the input spec (chart_spec + semantic_types), transforms data before Flint,
  and defaults to create_chart_view. Use when the user asks to chart, plot, visualize,
  graph, or dashboard Plecto metrics, traces, RED, /metrics, latency, throughput,
  filter decisions, upstream health, bench results, or says 「チャート」「グラフ」「可視化」
  「メトリクスを見て」「Flint」.
when_to_use: >-
  ユーザがチャート・グラフ・可視化・ダッシュボード・Flint を求めるとき。admin /metrics、
  RED、アクセスログ、performance/data の CSV、フィルタ decision の内訳、upstream 健全性、
  ベンチ（sweep / ceiling / wasm / openloop）を図示するとき。表を並べて終わる作業には使わない。
allowed-tools: Read, Write, Grep, Glob, Bash, FetchMcpResource, GetDynamicTools, CallDynamicTool
---

# Flint charts for Plecto

Plecto の観測データとベンチ結果を **Flint の入力スペック**として書き、MCP で描く。
出力は Vega-Lite / ECharts JSON ではなく `ChartAssemblyInput`（`chart_spec` + `semantic_types`）。
カタログ・チャネル・semantic type の正文は MCP リソース `flint://agent-skill`。
テーマを新しく作るときだけ `flint://theme-skill` を読む。

## 起動直後

1. 実データを見る（列名・値の分布。列名だけで描かない）。
2. Flint が要らない変形（rate、delta、join、pivot、累積ヒストグラムの差分化、wide→long）を先にやる。
3. `chartType` を登録名のまま選ぶ。迷ったら `flint://agent-skill` か `list_chart_types`。
4. 各エンコード列に **最も具体的な** semantic type を付ける。
5. `title` は所見の一文、`subtitle` は何を・誰の・いつ・何単位か。
6. 見せるときは **`create_chart_view` が既定**。静画（PNG/SVG）を明示されたときだけ `render_chart`。
   スペック検証だけなら `validate_chart`、バックエンド JSON が欲しいときだけ `compile_chart`。

MCP 呼び出しの前に `user-flint` 名前空間のスキーマを `GetDynamicTools` で取る。
`CallDynamicTool` では `mcpDetails.description` を付ける。変数名を `data` に渡さない
（MCP はホストのローカル変数を見ない）。小さい表は `data.values`、ファイルなら
`data.url`（ローカル `.json` / `.csv` / `.tsv`。リモート URL は不可）。

変形した表は `flint-data/` に書いて `data.url` で渡してよい（スクレイプや一時表はコミットしない）。
大きいデータセットを手で再シリアライズしない。

## Plecto のデータ源

| 源 | 場所 | そのまま Flint に渡してよいか |
|---|---|---|
| admin `/metrics` | Prometheus text exposition v0.0.4（`plecto/crates/server/src/metrics.rs`） | **否。** パース → ロング表へ。カウンタはプロセス起動以来の累積 |
| アクセスログ | `[observability] access_log = true` の JSON 行（契約は `docs/operations.md`） | 否。JSONL → 表。`path` のカーディナリティは落とす |
| ベンチ CSV | `performance/data/*.csv`（列は `performance/plot.py` が正） | 列がチャート形なら可。wide は先に畳む |
| OTLP | traces のみ（ADR 000040）。metrics の OTLP 化は未決 | スパン表に落としてから。生 protobuf は渡さない |

ライブスクレイプ例（admin はデータプレーンと別ポート、manifest の `admin_addr`）:

```bash
curl -s "http://${ADMIN}/metrics"
```

**いまの露出に無いもの**（描かない・推測で列を作らない）:

- `plecto_requests_total` の `route` ラベル — ADR 000112 は **proposed**。現行は `status_class` のみ。
- `plecto_filter_*` の filter-ID — host 集約（ADR 000099）。
- duration ヒストグラムの route 内訳。
- リクエスト側 `continue` / `modified` の内訳。見えるのは
  `plecto_filter_executions_total` / `_errors_total` / `_short_circuits_total` だけ。

## 変形ルール（Flint の前）

Flint はコンパイラであり、PromQL でも wrangler でもない。

1. **sanity-read。** カテゴリの distinct、量の min/max、単位（秒 vs ms vs µs、0–1 vs 0–100）。
2. **埋め込み合計を混ぜない。** `all` / `Total` 行を内訳と同じ stacked/grouped/color に載せない。
3. **Prometheus カウンタ。** 1 スクレイプなら subtitle に「プロセス起動以来の累積」と書く。
   時系列にするならスクレイプ間の delta / rate を先に計算する。
4. **Prometheus ヒストグラム。** `_bucket{le=}` は**累積**。Flint の `"Histogram"` は生観測のビンなので
   **使わない。** 隣接 `le` の差で非累積カウントにし、`"Bar Chart"`（`x` = `le` を Category、
   `sortOrder` はバケット順）。バケット上限は
   `1ms, 5ms, 10ms, 25ms, 50ms, 100ms, 250ms, 500ms, 1s, 2.5s, 5s, 10s, +Inf`。
5. **配列 fold だけが組み込み reshape。** `x` か `y` に量の列の配列（色は fold が占有）。
   それ以外の long/wide はホスト側で畳む。
6. **列名は実在するものだけ。** 発明しない。

## 意味の切り分け（RED を雑に畳まない）

| 系列 | 意味 | クライアントに見えるもの |
|---|---|---|
| `plecto_requests_total{status_class}` | 完了リクエスト | 1xx–5xx |
| `plecto_rate_limited_total` | ネイティブ route 上限（ADR 000033） | **429** |
| `plecto_circuit_open_total` | upstream breaker shed（ADR 000028） | **503** |
| `plecto_filter_short_circuits_total` | フィルタが chain を止めた | ゲストが決めた status（auth なら 401/403 が多い） |
| `plecto_filter_errors_total` | trap / deadline / instantiate / unavailable | fail-closed（素通りしない） |
| `plecto_upstream_retries_total` | 別インスタンスへの再試行 | 1 リクエストに複数回あり得る |
| `plecto_outlier_ejections_total` | rotation から外した回数 | カウンタ（現在 eject 中は `state="ejected"`） |
| `plecto_upstream_instances` | スクレイプ時の健全性。`ejected` > `unhealthy` > `healthy`（ADR 000099） | ゲージ。持続カウンタなし |

4xx 全体を「エラー」にしない。429 は rate-limit、フィルタ short-circuit の 401 は別系列。
5xx 全体を breaker にしない。`no-healthy` の fail-closed 503 と `circuit_open` は別。

## 見出しとテーマ

`title` は所見。`subtitle` は方法（closed-loop VU / open-loop 到着率 / loopback / ジェネレータ /
累積 vs rate）。絶対 throughput を見出しにしない — 回帰で追うのは **比・曲線形・µs/req・時定数**
（`performance/README.md`）。ジェネレータが違う系列を 1 枚に載せない。

既定テーマは **`nature`**（方法を隠さない図）。運用スナップショットをコンパクトにするときだけ
`economist`。プリセット id は `list_themes`。ブランド上書きを頼まれたときだけ
`flint://theme-skill`。ThemeSpec は Vega-Lite にだけ効く。

競合プロダクト名で「他がこう描くから」と正当化しない（RFC / 系列の意味 / 方法論で書く）。

## よくある形 → chartType

登録名は一字一句このまま。レシピの JSON は [recipes.md](recipes.md)。

| 聞き方 | chartType | 前処理 |
|---|---|---|
| status class の内訳 | `"Bar Chart"` または `"Stacked Bar Chart"` | `{status_class, count}`。`sortOrder`: 1xx…5xx |
| レイテンシ分布（`/metrics`） | `"Bar Chart"` | バケットを非累積化。`"Histogram"` 禁止 |
| アクセスログの duration | `"ECDF Plot"` または `"Histogram"` | 生の `duration_ms` |
| in-flight / tunnels / pool / OTLP queue | `"KPI Card"` または時系列なら `"Line Chart"` | ゲージはそのまま |
| upstream 健全性 | `"Stacked Bar Chart"` | long: `{upstream, state, count}`。state の順は healthy / unhealthy / ejected |
| filter executions vs errors vs short-circuit | `"Grouped Bar Chart"` または donut（`"Pie Chart"` + `innerRadius`） | 3 系列を long に。`continue`/`modified` を捏造しない |
| inspection skip | `"Bar Chart"` | `reason` × count |
| sweep throughput vs VUs | `"Line Chart"` | `sweep.csv`: `vus`, `rps` |
| sweep パーセンタイル | `"Line Chart"` | `y: ["p50","p95","p99"]`（単位 ms） |
| WASM / TLS / ceiling の 1 点比較 | `"Bar Chart"` または `"Grouped Bar Chart"` | `wasm_overhead.csv` / `ceiling.csv` / `tls.csv` |
| RR 配分 | `"Bar Chart"` | `rr.csv`: `instance`, `count`。合計行を載せない |
| ejection / swap タイムライン | `"Area Chart"`（インスタンス）+ 失敗は別系列か別図 | wide `a,b,c,failed` を long に。failed を stack に混ぜない |
| open-loop 分布 | `"ECDF Plot"` または非累積バー | `openloop_hist.csv` の形を先に読む |
| 時系列の 2 点変化 | `"Slope Chart"` | 2 期間だけ |

## Semantic types（Plecto 列）

無い名前を作らない。足りなければ family 既定（`Quantity` / `Category` / `DateTime`）。

| 列の種類 | type | 注 |
|---|---|---|
| `status_class`, `reason`, `state`, `route`, `kpi`, `outcome` | `Category` | 意味順があるなら annotation の `sortOrder` |
| `upstream`, `instance` | `Name` | |
| カウンタ / ゲージ個数 / `count` / `vus` | `Count` | |
| `rps`, 許可レート | `Quantity` + `unit`: `"req/s"` | |
| `duration_ms`, `p50`/`p95`/`p99`（ms） | `Quantity` + `unit`: `"ms"` | ゼロ基線を強制しない（`includeZero_y: false` が必要なときだけ） |
| µs/req デルタ | `Quantity` + `unit`: `"µs/req"` | |
| `duration_seconds` 生値 | `Quantity` + `unit`: `"s"` | |
| `failed` が 0–1 | `Percentage`（分数なら 100 倍しない） | すでに 0–100 ならそのまま `Percentage` |
| tunnel / pool バイト | `Quantity` + `unit`: `"B"` | |
| `t`（ベンチ秒） | `Quantity` + `unit`: `"s"` | エポック日時ではない |

`Price` は使わない。`PercentageChange` は比の変化のときだけ。

## MCP ツール

`user-flint` 名前空間（呼ぶ前にスキーマを取る）:

- `create_chart_view` — 対話ビュー。**見せるときの既定**
- `render_chart` — PNG/SVG。静画を頼まれたとき
- `validate_chart` / `compile_chart` / `list_chart_types` / `list_themes`

同じ数字を Markdown 表で重複して出さない（チャートが成果物）。数値の引用が必要なら所見の 1–2 個に留める。

コードへ Flint を入れる作業を頼まれたときだけ `flint-chart` をインストールする。
Plecto 本体（Rust プロキシ）に npm 依存を足さない。

## やってはいけないこと

- バックエンド JSON を手で書いて `render_chart` に渡す
- 編集済み Vega-Lite を Flint に戻す（escape hatch は `compile_chart` の**後**、ホスト側レンダラへ）
- `transforms` プロパティを発明する
- 4xx/5xx を 1 本の「エラー」に畳む
- closed-loop 飽和レイテンシをサービスレイテンシとして見出しにする
- 未実装ラベル（route / filter-ID）をスクレイプに「ある」と書く
- 大きなデータをチャットに貼る

## 検証

返す前に:

1. `chartType` は登録名。必須チャネルがある。
2. `encodings` の `field` は変形後の実列。
3. エンコードした列はすべて `semantic_types` にある。
4. `chartProperties` はその型に存在するキーだけ。
5. 累積バケットを `"Histogram"` に渡していない。
6. 見出しが方法と単位を嘘つかない。

## 参照

- Flint 要約: [authoring.md](authoring.md)（矛盾したら `flint://agent-skill`）
- レシピ: [recipes.md](recipes.md)
- 正文: `flint://agent-skill` / `flint://theme-skill`
- メトリクス実装: `plecto/crates/server/src/metrics.rs`
- 運用契約: `docs/operations.md`
- ベンチ列: `performance/plot.py`、方法: `performance/README.md` / `bench/methodology.md`
