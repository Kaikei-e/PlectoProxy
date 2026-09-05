# Flint recipes (Plecto)

`data` はプレースホルダ。ホストがスクレイプ・CSV・変形済み JSON をバインドする。
`chartType` / チャネル / semantic type は `flint://agent-skill` に従う。

## 1. Status class（単一 `/metrics` スクレイプ）

変形: `plecto_requests_total{status_class="…"}` → `{ status_class, count }`。ゼロ系列も残す。

```json
{
  "data": { "values": [] },
  "semantic_types": {
    "status_class": {
      "semanticType": "Category",
      "sortOrder": ["1xx", "2xx", "3xx", "4xx", "5xx"]
    },
    "count": "Count"
  },
  "chart_spec": {
    "chartType": "Bar Chart",
    "title": "Most completed requests were 2xx",
    "subtitle": "plecto_requests_total by status_class, process-lifetime totals, one admin scrape",
    "encodings": {
      "x": { "field": "status_class" },
      "y": { "field": "count" }
    }
  },
  "theme_spec": "nature"
}
```

4xx を「エラー」と書かない。429 は `plecto_rate_limited_total`、short-circuit は filter 系列。

## 2. レイテンシバケット（Prometheus ヒストグラム → 非累積バー）

`plecto_request_duration_seconds_bucket{le=}` を差分化する。Flint の `"Histogram"` は使わない。

```json
{
  "data": { "values": [] },
  "semantic_types": {
    "le": {
      "semanticType": "Category",
      "sortOrder": [
        "≤1ms", "≤5ms", "≤10ms", "≤25ms", "≤50ms", "≤100ms",
        "≤250ms", "≤500ms", "≤1s", "≤2.5s", "≤5s", "≤10s", ">10s"
      ]
    },
    "count": "Count"
  },
  "chart_spec": {
    "chartType": "Bar Chart",
    "title": "Almost all requests finished inside 5 ms",
    "subtitle": "Non-cumulative plecto_request_duration_seconds buckets, one scrape",
    "encodings": {
      "x": { "field": "le" },
      "y": { "field": "count" }
    }
  },
  "theme_spec": "nature"
}
```

アクセスログの生 `duration_ms` なら `"ECDF Plot"`（`x` = duration）か `"Histogram"`（観測をビンする）。

## 3. Upstream 健全性

`plecto_upstream_instances{upstream,state}` はスクレイプ時ゲージ。long: `{ upstream, state, count }`。

```json
{
  "data": { "values": [] },
  "semantic_types": {
    "upstream": "Name",
    "state": {
      "semanticType": "Category",
      "sortOrder": ["healthy", "unhealthy", "ejected"]
    },
    "count": "Count"
  },
  "chart_spec": {
    "chartType": "Stacked Bar Chart",
    "title": "One upstream has no healthy instances",
    "subtitle": "plecto_upstream_instances at scrape time; ejected folds over unhealthy over healthy (ADR 000099)",
    "encodings": {
      "x": { "field": "upstream" },
      "y": { "field": "count" },
      "color": { "field": "state" }
    }
  },
  "theme_spec": "nature"
}
```

## 4. Filter 実行（host 集約）

見えるのは 3 カウンタだけ。`continue` / `modified` 列は作らない。

long: `{ kind, count }` with `kind` ∈ `executions` / `errors` / `short_circuits`。

```json
{
  "data": { "values": [] },
  "semantic_types": {
    "kind": {
      "semanticType": "Category",
      "sortOrder": ["executions", "short_circuits", "errors"]
    },
    "count": "Count"
  },
  "chart_spec": {
    "chartType": "Bar Chart",
    "title": "Short-circuits are a small share of filter executions",
    "subtitle": "Host-aggregated plecto_filter_* counters, no filter-id label, process-lifetime totals",
    "encodings": {
      "x": { "field": "kind" },
      "y": { "field": "count" }
    }
  },
  "theme_spec": "nature"
}
```

## 5. Shed vs retry（混同しない 3 本）

long: `{ signal, count }`。`circuit_open` / `rate_limited` / `retries` は別の故障。

```json
{
  "data": { "values": [] },
  "semantic_types": {
    "signal": "Category",
    "count": "Count"
  },
  "field_display_names": {
    "signal": "Fast-path signal"
  },
  "chart_spec": {
    "chartType": "Bar Chart",
    "title": "Rate-limit 429s outnumber breaker 503s",
    "subtitle": "plecto_rate_limited_total vs plecto_circuit_open_total vs plecto_upstream_retries_total, one scrape",
    "encodings": {
      "x": { "field": "signal" },
      "y": { "field": "count" }
    }
  },
  "theme_spec": "nature"
}
```

## 6. Response-body inspection skip（ADR 000098）

`reason`: `streaming-content-type` / `content-encoding` / `partial-content` / `over-cap`。ゼロ系列も出す。

```json
{
  "data": { "values": [] },
  "semantic_types": {
    "reason": "Category",
    "count": "Count"
  },
  "chart_spec": {
    "chartType": "Bar Chart",
    "title": "Over-cap bodies account for every inspection skip",
    "subtitle": "plecto_response_body_inspection_skipped_total by reason",
    "encodings": {
      "x": { "field": "reason" },
      "y": { "field": "count" }
    }
  },
  "theme_spec": "nature"
}
```

## 7. Sweep: throughput vs concurrency

`performance/data/sweep.csv` 列: `vus`, `rps`, `p50`, `p95`, `p99`, …。closed-loop 天井。VU 400/800 の failed は per-IP cap（ADR 000092）との相互作用になり得る — 見出しで「LB の崖」と言わない。

```json
{
  "data": { "url": "performance/data/sweep.csv" },
  "semantic_types": {
    "vus": "Count",
    "rps": { "semanticType": "Quantity", "unit": "req/s" }
  },
  "chart_spec": {
    "chartType": "Line Chart",
    "title": "Throughput plateaus then declines; it does not cliff under 200 VUs",
    "subtitle": "Closed-loop constant-VUs sweep, loopback, single host; not service latency",
    "encodings": {
      "x": { "field": "vus" },
      "y": { "field": "rps" }
    },
    "chartProperties": { "showPoints": true }
  },
  "theme_spec": "nature"
}
```

パーセンタイル（同じ CSV、配列 fold）:

```json
{
  "data": { "url": "performance/data/sweep.csv" },
  "semantic_types": {
    "vus": "Count",
    "p50": { "semanticType": "Quantity", "unit": "ms" },
    "p95": { "semanticType": "Quantity", "unit": "ms" },
    "p99": { "semanticType": "Quantity", "unit": "ms" }
  },
  "chart_spec": {
    "chartType": "Line Chart",
    "title": "Tail latency rises with concurrency while the 200-VU rung still has zero failures",
    "subtitle": "Closed-loop sweep p50/p95/p99, milliseconds, loopback",
    "encodings": {
      "x": { "field": "vus" },
      "y": ["p50", "p95", "p99"]
    },
    "chartProperties": { "showPoints": true, "logScale_y": true }
  },
  "theme_spec": "nature"
}
```

## 8. Ceiling RR vs CRR

`ceiling.csv`: `variant` / `kpi` / `rps` / `p50` / `p99`（`performance/plot.py` と README 表を正とする。列が違えば CSV ヘッダに合わせる）。

```json
{
  "data": { "url": "performance/data/ceiling.csv" },
  "semantic_types": {
    "kpi": "Category",
    "rps": { "semanticType": "Quantity", "unit": "req/s" }
  },
  "chart_spec": {
    "chartType": "Bar Chart",
    "title": "A TCP handshake per request costs about half the keep-alive ceiling",
    "subtitle": "Plain HTTP/1.1 RR vs CRR, oha, loopback; ratio is the durable signal",
    "encodings": {
      "x": { "field": "kpi" },
      "y": { "field": "rps" }
    }
  },
  "theme_spec": "nature"
}
```

## 9. WASM ladder throughput

`wasm_overhead.csv`: `route`, `rps`, `p50`, `p90`, `p95`, `p99`。`baseline` 行は `ceiling.csv` 参照のことがある — 同一ジェネレータの行だけを並べる。

```json
{
  "data": { "url": "performance/data/wasm_overhead.csv" },
  "semantic_types": {
    "route": "Category",
    "rps": { "semanticType": "Quantity", "unit": "req/s" }
  },
  "chart_spec": {
    "chartType": "Bar Chart",
    "title": "Re-paying init every request collapses the on-demand rung",
    "subtitle": "WASM overhead ladder, full-throttle ceilings at fixed VUs, loopback",
    "encodings": {
      "x": { "field": "route" },
      "y": { "field": "rps" }
    }
  },
  "theme_spec": "nature"
}
```

パーセンタイル比較は wide のまま grouped できない。long `{ route, percentile, ms }` に畳んで `"Grouped Bar Chart"`（`x` = percentile, `group` = route, `y` = ms）。

## 10. Round-robin 配分

`rr.csv`: `instance`, `count`。合計行を足さない。

```json
{
  "data": { "url": "performance/data/rr.csv" },
  "semantic_types": {
    "instance": "Name",
    "count": "Count"
  },
  "chart_spec": {
    "chartType": "Bar Chart",
    "title": "Healthy instances split work to the request",
    "subtitle": "Round-robin under steady load, X-Instance tally",
    "encodings": {
      "x": { "field": "instance" },
      "y": { "field": "count" }
    }
  },
  "theme_spec": "nature"
}
```

## 11. Ejection タイムライン

`ejection_timeline.csv` は wide（`t,a,b,c,failed`）。long `{ t, instance, rps }` に畳む。`failed` は stack に入れず、別 `"Line Chart"` か、同じ long に `series=failed` として折れ線専用図にする。

インスタンス分（`"Area Chart"`）:

```json
{
  "data": { "values": [] },
  "semantic_types": {
    "t": { "semanticType": "Quantity", "unit": "s" },
    "rps": { "semanticType": "Quantity", "unit": "req/s" },
    "instance": "Name"
  },
  "chart_spec": {
    "chartType": "Area Chart",
    "title": "Traffic leaves the ejected instance on a one-second time constant",
    "subtitle": "Fault-injection timeline, per-instance req/s; 503/s is a separate series",
    "encodings": {
      "x": { "field": "t" },
      "y": { "field": "rps" },
      "color": { "field": "instance" }
    }
  },
  "theme_spec": "nature"
}
```

`swap.csv` も同じ（インスタンス列は `t`/`failed` 以外。集合が実行中に変わる）。

## 12. KPI タイル（in-flight / tunnels / OTLP queue）

ゲージ 1 値。`goal` が無ければ `"Bar Chart"` 1 本か、目標があるときだけ `"KPI Card"` / `"Bullet Chart"`。

```json
{
  "data": { "values": [] },
  "semantic_types": {
    "metric": "Name",
    "value": "Count",
    "goal": "Count"
  },
  "chart_spec": {
    "chartType": "KPI Card",
    "title": "Drain is waiting on open upgrade tunnels",
    "subtitle": "plecto_tunnels_active vs operator drain target, scrape time",
    "encodings": {
      "metric": { "field": "metric" },
      "value": { "field": "value" },
      "goal": { "field": "goal" }
    }
  },
  "theme_spec": "nature"
}
```

OTLP 系列（`otlp_endpoint` 設定時のみ）: `plecto_otlp_queue_spans`（ゲージ）、`plecto_otlp_dropped_spans_total`（累積）。drop が増えているなら見出しで queue cap（2048）に触れる。

## 13. Accept vs reject mix（`wasm_mixed.csv`）

列: `outcome`（`accept` / `reject`）、パーセンタイル、`count`。`no_status` 行があれば別カテゴリとして残す（401 に折らない）。

long `{ outcome, percentile, ms }` にして `"Grouped Bar Chart"`。
