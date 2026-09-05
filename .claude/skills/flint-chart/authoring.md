# Flint authoring (distilled)

Plecto 作業中は [SKILL.md](SKILL.md) を先に読む。ここは `flint://agent-skill` の要約。矛盾したら
MCP リソース側を正とする。テーマを書くときは `flint://theme-skill`。

## 出力

書くのは `ChartAssemblyInput` の **`chart_spec` と `semantic_types`**。データ列は名前で参照する。
バックエンド JSON（Vega-Lite 等）は書かない。構造は Flint、見た目の微調整だけ compile 後。

```ts
interface ChartAssemblyInput {
  data: { values: any[] } | { url: string };
  semantic_types?: Record<string, string | SemanticAnnotation>;
  chart_spec: {
    chartType: string;
    title?: string;
    subtitle?: string;
    encodings: Record<string, EncodingValue>;
    baseSize?: { width: number; height: number };
    canvasSize?: { width: number; height: number };
    chartProperties?: Record<string, any>;
  };
  field_display_names?: Record<string, string>;
  theme_spec?: string | { extends: string; [key: string]: any };
}
```

MCP 描画: 小さい表は `data.values`。ローカル `.json`/`.csv`/`.tsv` は `data.url`。
生成コード内だけ `data: { values: rows }` の変数束縛を使う。

## Semantic types（最重要）

登録名以外を作らない。分からなければ数値 `Quantity`、文字列 `Category`、日付 `Date`/`DateTime`。

| 族 | 型 |
|---|---|
| 時点 | `DateTime`, `Date`, `Time`, `Timestamp` |
| 粒度 | `Year`, `Quarter`, `Month`, `Week`, `Day`, `Hour`, `YearMonth`, `YearQuarter`, `YearWeek`, `Decade` |
| 幅 | `Duration` |
| 量 | `Amount`, `Price`, `Quantity`, `Count`, `Number` |
| 割合 | `Percentage` |
| 符号 | `Profit`, `PercentageChange`, `Sentiment`, `Correlation` |
| 物理 | `Temperature` |
| 離散 | `Rank`, `Score`, `ID` |
| 地理 | `Latitude`, `Longitude`, `Country`, `State`, `City`, `Region`, `Address`, `ZipCode` |
| カテゴリ | `Category`, `Name`, `Status`, `Boolean`, `Direction`, `Range` |
| 予備 | `Unknown` |

注釈オブジェクト: `{ semanticType, unit?, intrinsicDomain?, divergingMidpoint?, sortOrder? }`。

Plecto での典型: 件数 → `Count`、RPS → `Quantity` + `unit: "req/s"`、レイテンシ ms →
`Quantity` + `unit: "ms"`（Flint の `Duration` は時間幅の意味型なのでリクエスト所要時間には使わない）、
µs/req → `Quantity` + `unit: "µs/req"`、status class / state / reason / verdict → `Category`、
割合 → `Percentage`（0–100 か 0–1 を先に確定）。

## chartType（登録名を正確に）

Vega-Lite が既定で最も広い。必須チャネルは表のとおり。

| chartType | チャネル | 必須 / メモ |
|---|---|---|
| `"Bar Chart"` | x, y, color, opacity, column, row | 1 離散 + 1 量。`color` に第 2 カテゴリを置くと積み上げになる |
| `"Grouped Bar Chart"` | x, y, group, column, row | 横並び。第 2 カテゴリは **`group`** |
| `"Stacked Bar Chart"` | x, y, color, column, row | 部分と全体。`stackMode` |
| `"Line Chart"` | x, y, color, strokeDash, detail, opacity, column, row | `interpolate`, `showPoints` |
| `"Area Chart"` | x, y, color, opacity, column, row | |
| `"Histogram"` | x, color, column, row | **生の量**をビン分割。既ビン（Prom `le`）には使わない |
| `"ECDF Plot"` | x, color, detail, column, row | 生の量の累積分布。`showPoints` |
| `"Pie Chart"` | size, color, column, row | `size` = 値、`color` = カテゴリ。ドーナツは `innerRadius > 0` |
| `"KPI Card"` | metric, value, goal | |
| `"Bullet Chart"` | y, x, goal, color, column, row | `goal` 必須 |
| `"Scatter Plot"` | x, y, color, size, opacity, column, row | |
| `"Lollipop Chart"` | x, y, color, column, row | |
| `"Heatmap"` | x, y, color, column, row | color = 量 |
| `"Boxplot"` | x, y, color, opacity, column, row | |
| `"Bump Chart"` | x, y, color, detail, column, row | 順位の時系列 |
| `"Sparkline"` | x, y, color, detail, row, column | |

その他（必要になったら `flint://agent-skill` の表を見る）: Regression, Connected Scatter,
Ranged Dot, Strip, Pyramid, Waterfall, Gantt, Calendar Heatmap, Slope, Range Area,
Violin, Streamgraph, Density, Rose, Radar, Candlestick, Bar Table, Map, Choropleth。

棒の使い分け: 1 系列 → Bar。部分と全体 → Stacked（第 2 カテゴリは `color`）。値の横比較 →
Grouped（第 2 カテゴリは `group`）。

## Encodings

```json
"encodings": {
  "x": { "field": "status_class", "sortBy": "y", "sortOrder": "descending" },
  "y": { "field": "count" }
}
```

任意フィールド: `type`, `aggregate` (`count`/`sum`/`average`/`mean`), `sortOrder`, `sortBy`,
`scheme`。推論と衝突する意図があるときだけ書く。

**wide → long:** `x` または `y` に量列の配列。同時に `color` は置けない。

```json
"encodings": { "x": { "field": "vus" }, "y": ["p50", "p95", "p99"] }
```

## 見出しとサイズ

- `title`: 発見を 1 文
- `subtitle`: 何を、誰の、いつ、単位
- `baseSize`: 目標サイズ（既定 400×320）。データが密なら天井まで伸びる
- `canvasSize`: はみ出さない上限。固定スロットならこれだけ

## テーマ

プリセット id: `nyt`, `economist`, `swiss`, `nature`, `mckinsey`, `datawrapper`, `powerbi`,
`powerbi-light`, `cartoon`（`list_themes` で確認）。ThemeSpec は Vega-Lite のみ。
override は狭いキーだけ（`ink.series.single` 等）。`categorical` を置き換えたら
`categoricalExtended` も。

## chartProperties（ユーザが挙動を頼んだときだけ）

よく使うもの: Bar `cornerRadius`; Stacked `stackMode` (`stacked`/`normalize`/`layered`);
Grouped `dodge`; Line `interpolate`, `showPoints`; Histogram `binCount`; Pie `innerRadius`,
`sortSlices`; KPI `behindThreshold`; 横断 `logScale_y`, `includeZero_y`, `independentYAxis`。
値ラベルは `showValueLabels`（対応するマーク型のみ）。

## 検証

1. `chartType` が登録名で、ターゲットバックエンドがそれを持つ
2. encoding の `field` が表の列名と一致する
3. 使った列すべてに `semantic_types` がある
4. 必須チャネルがある（Bullet→`goal`、Pie→`size`+`color`）
5. `chartProperties` がその型に存在し範囲内
6. 大きいデータをインラインしていない。スタイルを手で塗っていない
7. stacked / grouped / color に全体行と部品行が混ざっていない
