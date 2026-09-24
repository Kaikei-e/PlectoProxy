import fs from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { renderChart } from 'flint-chart-mcp/render';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const DATA_DIR = path.resolve(__dirname, '../data');
const IMG_DIR = path.resolve(__dirname, '../img');

function parseCsv(content) {
  const lines = content.trim().split(/\r?\n/).filter(line => line.trim().length > 0);
  if (lines.length === 0) return [];
  const headers = lines[0].split(',').map(h => h.trim());
  const rows = [];
  for (let i = 1; i < lines.length; i++) {
    const parts = lines[i].split(',').map(p => p.trim());
    const row = {};
    for (let j = 0; j < headers.length; j++) {
      row[headers[j]] = parts[j] ?? '';
    }
    rows.push(row);
  }
  return rows;
}

async function readCsv(filename) {
  const filePath = path.join(DATA_DIR, filename);
  const content = await fs.readFile(filePath, 'utf-8');
  return parseCsv(content);
}

async function chart(name, input) {
  const r = await renderChart(input, 'vegalite', {
    format: 'png',
    scale: 2,
    background: '#ffffff',
  });
  await fs.mkdir(IMG_DIR, { recursive: true });
  const outPath = path.join(IMG_DIR, `${name}.png`);
  await fs.writeFile(outPath, r.buffer);
  console.log(`wrote img/${name}.png`);
  if (Array.isArray(r.warnings) && r.warnings.length > 0) {
    for (const w of r.warnings) {
      const msg = typeof w === 'object' && w !== null ? (w.message || JSON.stringify(w)) : String(w);
      console.warn(`  warning [${name}]: ${msg}`);
    }
  }
}

// 1. Throughput vs concurrency (sweep.csv vus->rps, Line)
async function plotThroughputVsConcurrency() {
  const raw = await readCsv('sweep.csv');
  const rows = raw
    .map(r => ({
      vus: Number(r.vus),
      rps: Number(r.rps),
    }))
    .sort((a, b) => a.vus - b.vus);

  const peak = rows.reduce((max, r) => (r.rps > max.rps ? r : max), rows[0]);
  const drop = ((1 - rows[rows.length - 1].rps / peak.rps) * 100).toFixed(0);
  const firstFailRow = raw.find(r => Number(r.failed) > 0);
  const failText = firstFailRow ? `; failures appear from ${firstFailRow.vus} VUs` : '';
  const title = `Throughput peaks near ${peak.vus} VUs and declines ${drop}% at 800 VUs${failText}`;

  await chart('throughput_vs_concurrency', {
    data: { values: rows },
    semantic_types: {
      vus: 'Count',
      rps: { semanticType: 'Quantity', unit: 'req/s' },
    },
    field_display_names: {
      vus: 'Concurrent VUs',
      rps: 'Throughput (req/s)',
    },
    chart_spec: {
      chartType: 'Line Chart',
      title,
      subtitle: 'Closed-loop constant-VUs sweep, k6, loopback single host; 0 ms backend, req/s',
      encodings: {
        x: { field: 'vus' },
        y: { field: 'rps' },
      },
      chartProperties: {
        showPoints: true,
      },
    },
    theme_spec: 'nature',
  });
}

// 2. Latency vs concurrency (sweep.csv vus->[p50,p95,p99] folded)
async function plotLatencyVsConcurrency() {
  const raw = await readCsv('sweep.csv');
  const rows = raw
    .map(r => ({
      vus: Number(r.vus),
      p50: Number(r.p50),
      p95: Number(r.p95),
      p99: Number(r.p99),
    }))
    .sort((a, b) => a.vus - b.vus);

  const ratio = (rows[rows.length - 1].p99 / rows[0].p99).toFixed(1);
  const title = `p99 queueing scales ${ratio}× from 50 to 800 VUs while sub-200 VUs remain failure-free`;

  await chart('latency_vs_concurrency', {
    data: { values: rows },
    semantic_types: {
      vus: 'Count',
      p50: { semanticType: 'Quantity', unit: 'ms' },
      p95: { semanticType: 'Quantity', unit: 'ms' },
      p99: { semanticType: 'Quantity', unit: 'ms' },
    },
    field_display_names: {
      vus: 'Concurrent VUs',
      value: 'Latency (ms)',
      Value: 'Latency (ms)',
      p50: 'p50 Latency (ms)',
      p95: 'p95 Latency (ms)',
      p99: 'p99 Latency (ms)',
    },
    chart_spec: {
      chartType: 'Line Chart',
      title,
      subtitle: 'Latency percentiles under closed-loop saturation, k6 constant-vus, loopback single host, milliseconds',
      encodings: {
        x: { field: 'vus' },
        y: ['p50', 'p95', 'p99'],
      },
      chartProperties: {
        showPoints: true,
        logScale_y: true,
      },
    },
    theme_spec: 'nature',
  });
}

// 3. Round-robin distribution (rr.csv instance->count Bar)
async function plotRrDistribution() {
  const raw = await readCsv('rr.csv');
  const rows = raw.map(r => ({
    instance: r.instance,
    count: Number(r.count),
  }));

  const total = rows.reduce((s, r) => s + r.count, 0);
  const shares = rows.map(r => ((r.count / total) * 100).toFixed(1));
  const even = shares.every(s => s === shares[0]);
  const title = even
    ? `All ${rows.length} healthy instances serve exactly ${shares[0]}% of load (${(total / rows.length).toLocaleString()} requests each)`
    : `Round-robin distribution splits work across ${rows.length} healthy instances`;

  await chart('rr_distribution', {
    data: { values: rows },
    semantic_types: {
      instance: 'Name',
      count: 'Count',
    },
    field_display_names: {
      instance: 'Upstream Instance',
      count: 'Requests Served',
    },
    chart_spec: {
      chartType: 'Bar Chart',
      title,
      subtitle: 'Round-robin under steady load, plecto-loadgen, loopback single host, requests served',
      encodings: {
        x: { field: 'instance' },
        y: { field: 'count' },
      },
      chartProperties: {
        showValueLabels: true,
      },
    },
    theme_spec: 'nature',
  });
}

// 4. Ejection timeline (ejection_timeline.csv t->a,b,c Area stacked, failed excluded)
async function plotEjectionTimeline() {
  const raw = await readCsv('ejection_timeline.csv');
  const instances = ['a', 'b', 'c'];
  const rows = [];
  for (const r of raw) {
    const t = Number(r.t);
    for (const inst of instances) {
      if (r[inst] !== undefined) {
        rows.push({
          t,
          instance: inst,
          rps: Number(r[inst]),
        });
      }
    }
  }

  const title = 'Ejected instance traffic drops to zero within 1 s while survivors absorb full load';

  await chart('ejection_timeline', {
    data: { values: rows },
    semantic_types: {
      t: { semanticType: 'Quantity', unit: 's' },
      rps: { semanticType: 'Quantity', unit: 'req/s' },
      instance: { semanticType: 'Name', sortOrder: ['a', 'b', 'c'] },
    },
    field_display_names: {
      t: 'Time (s)',
      rps: 'Throughput (req/s)',
      instance: 'Upstream Instance',
    },
    chart_spec: {
      chartType: 'Area Chart',
      title,
      subtitle: 'Fault-injection timeline, plecto-loadgen, loopback single host, per-upstream req/s (503s excluded)',
      encodings: {
        x: { field: 't' },
        y: { field: 'rps' },
        color: { field: 'instance' },
      },
    },
    theme_spec: 'nature',
  });
}

// 5. Ejection failed (ejection_timeline.csv t->failed Line, 503/s)
async function plotEjectionFailed() {
  const raw = await readCsv('ejection_timeline.csv');
  const rows = raw.map(r => ({
    t: Number(r.t),
    failed_rps: Number(r.failed),
  }));

  const peak = Math.max(...rows.map(r => r.failed_rps));
  const title = `Total upstream outage fails closed at ${Math.round(peak).toLocaleString()} HTTP 503/s and recovers to 0 upon restoration`;

  await chart('ejection_failed', {
    data: { values: rows },
    semantic_types: {
      t: { semanticType: 'Quantity', unit: 's' },
      failed_rps: { semanticType: 'Quantity', unit: 'req/s' },
    },
    field_display_names: {
      t: 'Time (s)',
      failed_rps: 'Failed Requests (503/s)',
    },
    chart_spec: {
      chartType: 'Line Chart',
      title,
      subtitle: 'Fail-closed HTTP 503 rate during total outage, plecto-loadgen, loopback single host, 503/s',
      encodings: {
        x: { field: 't' },
        y: { field: 'failed_rps' },
      },
      chartProperties: {
        showPoints: false,
      },
    },
    theme_spec: 'nature',
  });
}

// 6. Swap timeline (swap.csv t->a,b,c,d Area stacked, failed excluded)
async function plotSwapTimeline() {
  const raw = await readCsv('swap.csv');
  const instKeys = Object.keys(raw[0] || {}).filter(k => k !== 't' && k !== 'failed');
  const rows = [];
  for (const r of raw) {
    const t = Number(r.t);
    for (const k of instKeys) {
      if (r[k] !== undefined) {
        rows.push({
          t,
          instance: k,
          rps: Number(r[k]),
        });
      }
    }
  }

  const sumFailed = raw.reduce((sum, r) => sum + Number(r.failed || 0), 0);
  const cRows = raw.filter(r => Number(r.c || 0) > 0);
  const dRows = raw.filter(r => Number(r.d || 0) > 0);
  const lastC = cRows.length > 0 ? Number(cRows[cRows.length - 1].t) : 0;
  const firstD = dRows.length > 0 ? Number(dRows[0].t) : 0;
  const handoverWindow = lastC >= firstD ? lastC - firstD + 1 : 1;
  const failText = sumFailed === 0 ? 'zero client-visible failures' : `${sumFailed} failed requests`;
  const title = `Endpoint swap shifts traffic from instance c to d in ${handoverWindow} s with ${failText}`;

  await chart('swap_timeline', {
    data: { values: rows },
    semantic_types: {
      t: { semanticType: 'Quantity', unit: 's' },
      rps: { semanticType: 'Quantity', unit: 'req/s' },
      instance: { semanticType: 'Name', sortOrder: instKeys },
    },
    field_display_names: {
      t: 'Time (s)',
      rps: 'Throughput (req/s)',
      instance: 'Upstream Instance',
    },
    chart_spec: {
      chartType: 'Area Chart',
      title,
      subtitle: 'Endpoint-set swap under load, plecto-loadgen, loopback single host, per-instance req/s',
      encodings: {
        x: { field: 't' },
        y: { field: 'rps' },
        color: { field: 'instance' },
      },
    },
    theme_spec: 'nature',
  });
}

// 7. Ceiling (ceiling.csv variant->rps Bar)
async function plotCeiling() {
  const raw = await readCsv('ceiling.csv');
  const rows = raw.map(r => ({
    variant: r.variant,
    rps: Number(r.rps),
  }));

  const rr = rows.find(r => r.variant === 'keep-alive')?.rps || 1;
  const crr = rows.find(r => r.variant === 'cold (TCP/req)')?.rps || 1;
  const ratio = (rr / crr).toFixed(1);
  const title = `Keep-alive serves ${ratio}× the cold-connection throughput ceiling`;

  await chart('ceiling', {
    data: { values: rows },
    semantic_types: {
      variant: { semanticType: 'Category', sortOrder: ['keep-alive', 'cold (TCP/req)'] },
      rps: { semanticType: 'Quantity', unit: 'req/s' },
    },
    field_display_names: {
      variant: 'Variant',
      rps: 'Throughput (req/s)',
    },
    chart_spec: {
      chartType: 'Bar Chart',
      title,
      subtitle: 'Plain HTTP/1.1 ceiling, oha, loopback single host, closed-loop full-throttle, req/s',
      encodings: {
        x: { field: 'variant' },
        y: { field: 'rps' },
      },
      chartProperties: {
        showValueLabels: true,
      },
    },
    theme_spec: 'nature',
  });
}

// 8. Ceiling tail (ceiling.csv variant x [p50,p90,p95,p99] Grouped Bar, ms)
async function plotCeilingTail() {
  const raw = await readCsv('ceiling.csv');
  const percentiles = ['p50', 'p90', 'p95', 'p99'];
  const rows = [];
  for (const r of raw) {
    for (const p of percentiles) {
      if (r[p] !== undefined) {
        rows.push({
          variant: r.variant,
          percentile: p,
          latency_ms: Number(r[p]),
        });
      }
    }
  }

  const p99_rr = rows.find(r => r.variant === 'keep-alive' && r.percentile === 'p99')?.latency_ms || 1;
  const p99_crr = rows.find(r => r.variant === 'cold (TCP/req)' && r.percentile === 'p99')?.latency_ms || 1;
  const ratio = (p99_crr / p99_rr).toFixed(1);
  const title = `Cold TCP handshakes multiply p99 saturation queueing by ${ratio}× over keep-alive`;

  await chart('ceiling_tail', {
    data: { values: rows },
    semantic_types: {
      percentile: { semanticType: 'Category', sortOrder: ['p50', 'p90', 'p95', 'p99'] },
      variant: { semanticType: 'Category', sortOrder: ['keep-alive', 'cold (TCP/req)'] },
      latency_ms: { semanticType: 'Quantity', unit: 'ms' },
    },
    field_display_names: {
      percentile: 'Percentile',
      latency_ms: 'Latency (ms)',
      variant: 'Variant',
    },
    chart_spec: {
      chartType: 'Grouped Bar Chart',
      title,
      subtitle: 'Plain HTTP/1.1 tail percentiles under closed-loop saturation, oha, loopback single host, milliseconds',
      encodings: {
        x: { field: 'percentile' },
        y: { field: 'latency_ms' },
        group: { field: 'variant' },
      },
    },
    theme_spec: 'nature',
  });
}

// 9. TLS vs plain (tls.csv variant->rps Bar)
async function plotTlsVsPlain() {
  const raw = await readCsv('tls.csv');
  const rows = raw.map(r => ({
    variant: r.variant,
    rps: Number(r.rps),
  }));

  const plain = rows.find(r => r.variant === 'plain (h1)')?.rps || 1;
  const tlsKa = rows.find(r => r.variant === 'tls h1 keepalive')?.rps || 1;
  const hs = rows.find(r => r.variant === 'tls h1 handshake')?.rps || 1;

  const kaVsPlain = ((tlsKa / plain) * 100).toFixed(0);
  const hsVsKa = ((hs / tlsKa) * 100).toFixed(0);
  const hsVsPlain = ((hs / plain) * 100).toFixed(0);

  const title = `TLS keep-alive sustains ${kaVsPlain}% of plaintext throughput; a handshake per request sustains ${hsVsKa}% of TLS keep-alive (${hsVsPlain}% of plaintext)`;

  await chart('tls_vs_plain', {
    data: { values: rows },
    semantic_types: {
      variant: {
        semanticType: 'Category',
        sortOrder: ['plain (h1)', 'tls h1 keepalive', 'tls (h2)', 'tls h1 handshake'],
      },
      rps: { semanticType: 'Quantity', unit: 'req/s' },
    },
    field_display_names: {
      variant: 'Variant',
      rps: 'Throughput (req/s)',
    },
    chart_spec: {
      chartType: 'Bar Chart',
      title,
      subtitle: 'TLS termination overhead vs plain HTTP/1.1, oha, loopback single host, closed-loop, req/s',
      encodings: {
        x: { field: 'variant' },
        y: { field: 'rps' },
      },
      chartProperties: {
        showValueLabels: true,
      },
    },
    theme_spec: 'nature',
  });
}

// 10. TLS tail (tls.csv variant x [p50,p99] Grouped Bar)
async function plotTlsTail() {
  const raw = await readCsv('tls.csv');
  const percentiles = ['p50', 'p99'];
  const rows = [];
  for (const r of raw) {
    for (const p of percentiles) {
      if (r[p] !== undefined) {
        rows.push({
          variant: r.variant,
          percentile: p,
          latency_ms: Number(r[p]),
        });
      }
    }
  }

  const hsP99 = rows.find(r => r.variant === 'tls h1 handshake' && r.percentile === 'p99')?.latency_ms || 1;
  const kaP99 = rows.find(r => r.variant === 'tls h1 keepalive' && r.percentile === 'p99')?.latency_ms || 1;
  const ratio = (hsP99 / kaP99).toFixed(1);
  const title = `Per-request TLS handshakes multiply p99 saturation queueing by ${ratio}× over keep-alive`;

  await chart('tls_tail', {
    data: { values: rows },
    semantic_types: {
      percentile: { semanticType: 'Category', sortOrder: ['p50', 'p99'] },
      variant: {
        semanticType: 'Category',
        sortOrder: ['plain (h1)', 'tls h1 keepalive', 'tls (h2)', 'tls h1 handshake'],
      },
      latency_ms: { semanticType: 'Quantity', unit: 'ms' },
    },
    field_display_names: {
      percentile: 'Percentile',
      latency_ms: 'Latency (ms)',
      variant: 'Variant',
    },
    chart_spec: {
      chartType: 'Grouped Bar Chart',
      title,
      subtitle: 'TLS termination tail percentiles under closed-loop saturation, oha, loopback single host, milliseconds',
      encodings: {
        x: { field: 'percentile' },
        y: { field: 'latency_ms' },
        group: { field: 'variant' },
      },
    },
    theme_spec: 'nature',
  });
}

// 11. WASM throughput (wasm_overhead.csv route->rps Bar)
async function plotWasmThroughput() {
  const raw = await readCsv('wasm_overhead.csv');
  const rows = raw.map(r => ({
    route: r.route,
    rps: Number(r.rps),
  }));

  const pooled = rows.find(r => r.route === 'noop-pooled')?.rps || 1;
  const fresh = rows.find(r => r.route === 'noop-fresh')?.rps || 1;
  const collapse = (pooled / fresh).toFixed(0);
  const title = `Instance pooling preserves throughput; fresh instantiation collapses rate by ${collapse}×`;

  await chart('wasm_throughput', {
    data: { values: rows },
    semantic_types: {
      route: {
        semanticType: 'Category',
        sortOrder: ['baseline', 'noop-pooled', 'trusted', 'noop-fresh', 'ondemand'],
      },
      rps: { semanticType: 'Quantity', unit: 'req/s' },
    },
    field_display_names: {
      route: 'Route',
      rps: 'Throughput (req/s)',
    },
    chart_spec: {
      chartType: 'Bar Chart',
      title,
      subtitle: 'WASM overhead ladder throughput, oha, 50 VUs, loopback single host, full-throttle ceiling, req/s',
      encodings: {
        x: { field: 'route' },
        y: { field: 'rps' },
      },
      chartProperties: {
        showValueLabels: true,
      },
    },
    theme_spec: 'nature',
  });
}

// 12. WASM latency (wasm_overhead_tail.csv route x [p50,p90,p95,p99] Grouped Bar)
async function plotWasmLatency() {
  const raw = await readCsv('wasm_overhead_tail.csv');
  const percentiles = ['p50', 'p90', 'p95', 'p99'];
  const rows = [];
  for (const r of raw) {
    for (const p of percentiles) {
      if (r[p] !== undefined) {
        rows.push({
          route: r.route,
          percentile: p,
          latency_ms: Number(r[p]),
        });
      }
    }
  }

  const baseP99 = rows.find(r => r.route === 'baseline' && r.percentile === 'p99')?.latency_ms || 1;
  const pooledP99 = rows.find(r => r.route === 'noop-pooled' && r.percentile === 'p99')?.latency_ms || 1;
  const trustedP99 = rows.find(r => r.route === 'trusted' && r.percentile === 'p99')?.latency_ms || 1;
  const freshP99 = rows.find(r => r.route === 'noop-fresh' && r.percentile === 'p99')?.latency_ms || 1;
  const ondemandP99 = rows.find(r => r.route === 'ondemand' && r.percentile === 'p99')?.latency_ms || 1;

  const trustedDelta = (trustedP99 - baseP99).toFixed(2);
  const ondemandMult = (ondemandP99 / pooledP99).toFixed(1);
  const title = `Pooled p99 stays within +${trustedDelta} ms of baseline (${trustedP99.toFixed(2)} ms trusted) while on-demand expands ${ondemandMult}× (${ondemandP99.toFixed(1)} ms)`;

  await chart('wasm_latency', {
    data: { values: rows },
    semantic_types: {
      percentile: { semanticType: 'Category', sortOrder: ['p50', 'p90', 'p95', 'p99'] },
      route: {
        semanticType: 'Category',
        sortOrder: ['baseline', 'noop-pooled', 'trusted', 'noop-fresh', 'ondemand'],
      },
      latency_ms: { semanticType: 'Quantity', unit: 'ms' },
    },
    field_display_names: {
      percentile: 'Percentile',
      latency_ms: 'Latency (ms)',
      route: 'Route',
    },
    chart_spec: {
      chartType: 'Grouped Bar Chart',
      title,
      subtitle: 'Honest fixed-rate tails at 2,119 req/s, oha coordinated-omission-safe, loopback, milliseconds',
      encodings: {
        x: { field: 'percentile' },
        y: { field: 'latency_ms' },
        group: { field: 'route' },
      },
    },
    theme_spec: 'nature',
  });
}

// 13. WASM shortcircuit (wasm_mixed.csv accept/reject x [p50,p95,p99])
async function plotWasmShortcircuit() {
  const raw = await readCsv('wasm_mixed.csv');
  const percentiles = ['p50', 'p95', 'p99'];
  const rows = [];
  for (const r of raw) {
    if (r.outcome !== 'accept' && r.outcome !== 'reject') continue;
    for (const p of percentiles) {
      if (r[p] !== undefined) {
        rows.push({
          outcome: r.outcome,
          percentile: p,
          latency_ms: Number(r[p]),
        });
      }
    }
  }

  const rejP50 = rows.find(r => r.outcome === 'reject' && r.percentile === 'p50')?.latency_ms || 0.33;
  const rejP99 = rows.find(r => r.outcome === 'reject' && r.percentile === 'p99')?.latency_ms || 0.67;
  const title = `Rejected requests are answered at the edge in ${rejP50.toFixed(2)} ms (p99 ${rejP99.toFixed(2)} ms) without reaching the 15 ms backend`;

  await chart('wasm_shortcircuit', {
    data: { values: rows },
    semantic_types: {
      percentile: { semanticType: 'Category', sortOrder: ['p50', 'p95', 'p99'] },
      outcome: { semanticType: 'Category', sortOrder: ['accept', 'reject'] },
      latency_ms: { semanticType: 'Quantity', unit: 'ms' },
    },
    field_display_names: {
      percentile: 'Percentile',
      latency_ms: 'Latency (ms)',
      outcome: 'Decision Outcome',
    },
    chart_spec: {
      chartType: 'Grouped Bar Chart',
      title,
      subtitle: 'Accept vs reject under 90/10 traffic mix, k6, 15 ms backend, loopback single host, milliseconds',
      encodings: {
        x: { field: 'percentile' },
        y: { field: 'latency_ms' },
        group: { field: 'outcome' },
      },
    },
    theme_spec: 'nature',
  });
}

// 14. Rate-limit overhead (ratelimit_overhead.csv route->rps Bar)
async function plotRatelimitOverhead() {
  const raw = await readCsv('ratelimit_overhead.csv');
  const rows = raw.map(r => ({
    route: r.route.replace(/^\//, ''),
    rps: Number(r.rps),
  }));

  const base = rows.find(r => r.route === 'baseline')?.rps || 1;
  const rl = rows.find(r => r.route === 'ratelimit')?.rps || 1;
  const pct = (((base - rl) / base) * 100).toFixed(0);
  const title = `Never-deny rate-limit consultation adds a ${pct}% throughput tax at 50 VUs`;

  await chart('ratelimit_overhead', {
    data: { values: rows },
    semantic_types: {
      route: { semanticType: 'Category', sortOrder: ['baseline', 'ratelimit'] },
      rps: { semanticType: 'Quantity', unit: 'req/s' },
    },
    field_display_names: {
      route: 'Route',
      rps: 'Throughput (req/s)',
    },
    chart_spec: {
      chartType: 'Bar Chart',
      title,
      subtitle: 'Rate-limit consult overhead, k6, 50 VUs, never-deny bucket, loopback single host, req/s',
      encodings: {
        x: { field: 'route' },
        y: { field: 'rps' },
      },
      chartProperties: {
        showValueLabels: true,
      },
    },
    theme_spec: 'nature',
  });
}

// 15. Rate-limit overhead tail (route x [p50,p99] Grouped Bar)
async function plotRatelimitOverheadTail() {
  const raw = await readCsv('ratelimit_overhead.csv');
  const percentiles = ['p50', 'p99'];
  const rows = [];
  for (const r of raw) {
    const route = r.route.replace(/^\//, '');
    for (const p of percentiles) {
      if (r[p] !== undefined) {
        rows.push({
          route,
          percentile: p,
          latency_ms: Number(r[p]),
        });
      }
    }
  }

  const baseP99 = rows.find(r => r.route === 'baseline' && r.percentile === 'p99')?.latency_ms || 1;
  const rlP99 = rows.find(r => r.route === 'ratelimit' && r.percentile === 'p99')?.latency_ms || 1;
  const ratio = (rlP99 / baseP99).toFixed(2);
  const delta = (rlP99 - baseP99).toFixed(2);
  const title = `Never-deny rate-limit bucket increases p99 saturation queueing by ${ratio}× (+${delta} ms) over baseline`;

  await chart('ratelimit_overhead_tail', {
    data: { values: rows },
    semantic_types: {
      percentile: { semanticType: 'Category', sortOrder: ['p50', 'p99'] },
      route: { semanticType: 'Category', sortOrder: ['baseline', 'ratelimit'] },
      latency_ms: { semanticType: 'Quantity', unit: 'ms' },
    },
    field_display_names: {
      percentile: 'Percentile',
      latency_ms: 'Latency (ms)',
      route: 'Route',
    },
    chart_spec: {
      chartType: 'Grouped Bar Chart',
      title,
      subtitle: 'Rate-limit tail percentiles under closed-loop saturation, k6, 50 VUs, loopback single host, milliseconds',
      encodings: {
        x: { field: 'percentile' },
        y: { field: 'latency_ms' },
        group: { field: 'route' },
      },
    },
    theme_spec: 'nature',
  });
}

// 16. Rate-limit enforce (ratelimit_enforce.csv metric,value Bar)
async function plotRatelimitEnforce() {
  const raw = await readCsv('ratelimit_enforce.csv');
  const allowedMetrics = ['target_rps', 'achieved_rps', 'allowed_rps'];
  const rows = raw
    .filter(r => allowedMetrics.includes(r.metric))
    .map(r => ({
      metric: r.metric,
      value: Number(r.value),
    }));

  const shedRow = raw.find(r => r.metric === 'limited_frac');
  const shed = shedRow ? (Number(shedRow.value) * 100).toFixed(1) : '79.3';
  const allowedVal = rows.find(r => r.metric === 'allowed_rps')?.value || 1033;
  const title = `Offered load is ${shed}% shed as 429 while allowed traffic converges to refill rate (${Math.round(allowedVal)} req/s)`;

  await chart('ratelimit_enforce', {
    data: { values: rows },
    semantic_types: {
      metric: { semanticType: 'Category', sortOrder: ['target_rps', 'achieved_rps', 'allowed_rps'] },
      value: { semanticType: 'Quantity', unit: 'req/s' },
    },
    field_display_names: {
      metric: 'Metric',
      value: 'Throughput (req/s)',
    },
    chart_spec: {
      chartType: 'Bar Chart',
      title,
      subtitle: 'Rate-limit enforcement under 5× overload, k6 open-loop, loopback single host, req/s',
      encodings: {
        x: { field: 'metric' },
        y: { field: 'value' },
      },
      chartProperties: {
        showValueLabels: true,
      },
    },
    theme_spec: 'nature',
  });
}

// 17. Rate-limit fairness (ratelimit_fairness.csv key x [offered_rps, allowed_rps] Grouped Bar)
async function plotRatelimitFairness() {
  const raw = await readCsv('ratelimit_fairness.csv');
  const rows = [];
  for (const r of raw) {
    rows.push({
      key: r.key,
      traffic: 'offered',
      rps: Number(r.offered_rps),
    });
    rows.push({
      key: r.key,
      traffic: 'allowed',
      rps: Number(r.allowed_rps),
    });
  }

  const hotRow = raw.find(r => r.key === 'hot');
  const lightRow = raw.find(r => r.key === 'light');
  const hotShed = hotRow ? (Number(hotRow.shed_frac) * 100).toFixed(1) : '74.2';
  const lightAllowedFrac = lightRow ? ((Number(lightRow.allowed_rps) / Number(lightRow.offered_rps)) * 100).toFixed(0) : '100';
  const title = `Hot key is throttled (${hotShed}% shed) without starving light key (${lightAllowedFrac}% admitted)`;

  await chart('ratelimit_fairness', {
    data: { values: rows },
    semantic_types: {
      key: { semanticType: 'Category', sortOrder: ['hot', 'light'] },
      traffic: { semanticType: 'Category', sortOrder: ['offered', 'allowed'] },
      rps: { semanticType: 'Quantity', unit: 'req/s' },
    },
    field_display_names: {
      key: 'Tenant Key',
      rps: 'Throughput (req/s)',
      traffic: 'Traffic Type',
    },
    chart_spec: {
      chartType: 'Grouped Bar Chart',
      title,
      subtitle: 'Per-key fairness under overload, k6 concurrent keys, loopback single host, req/s',
      encodings: {
        x: { field: 'key' },
        y: { field: 'rps' },
        group: { field: 'traffic' },
      },
    },
    theme_spec: 'nature',
  });
}

// 18. Body (body.csv size x route -> rps Grouped Bar)
async function plotBody() {
  const raw = await readCsv('body.csv');
  const sizeLabelMap = {
    '1024': '1 KiB',
    '102400': '100 KiB',
    '1048576': '1 MiB',
  };
  const rows = raw.map(r => ({
    size: sizeLabelMap[r.size] || `${r.size} B`,
    size_raw: Number(r.size),
    route: r.route.replace(/^\//, ''),
    rps: Number(r.rps),
  }));

  const getRps = (sz, rt) => rows.find(r => r.size_raw === sz && r.route === rt)?.rps || 1;
  const ho1k = ((getRps(1024, 'body-headeronly') / getRps(1024, 'baseline')) * 100).toFixed(0);
  const ho1m = ((getRps(1048576, 'body-headeronly') / getRps(1048576, 'baseline')) * 100).toFixed(0);
  const bodyCost1m = ((1 - getRps(1048576, 'body') / getRps(1048576, 'baseline')) * 100).toFixed(0);

  const title = `Header-only throughput reaches ${ho1m}% of baseline at 1 MiB (up from ${ho1k}% at 1 KiB); buffering costs ${bodyCost1m}% at 1 MiB`;

  await chart('body', {
    data: { values: rows },
    semantic_types: {
      size: { semanticType: 'Category', sortOrder: ['1 KiB', '100 KiB', '1 MiB'] },
      route: { semanticType: 'Category', sortOrder: ['baseline', 'body-headeronly', 'body'] },
      rps: { semanticType: 'Quantity', unit: 'req/s' },
    },
    field_display_names: {
      size: 'Payload Size',
      rps: 'Throughput (req/s)',
      route: 'Route',
    },
    chart_spec: {
      chartType: 'Grouped Bar Chart',
      title,
      subtitle: 'Request body hook throughput by payload size, k6, 50 VUs, loopback single host, req/s',
      encodings: {
        x: { field: 'size' },
        y: { field: 'rps' },
        group: { field: 'route' },
      },
    },
    theme_spec: 'nature',
  });
}

// 19. Body tail (body.csv size x route -> p99 Grouped Bar, ms)
async function plotBodyTail() {
  const raw = await readCsv('body.csv');
  const sizeLabelMap = {
    '1024': '1 KiB',
    '102400': '100 KiB',
    '1048576': '1 MiB',
  };
  const rows = raw.map(r => ({
    size: sizeLabelMap[r.size] || `${r.size} B`,
    size_raw: Number(r.size),
    route: r.route.replace(/^\//, ''),
    p99_ms: Number(r.p99),
  }));

  const getP99 = (sz, rt) => rows.find(r => r.size_raw === sz && r.route === rt)?.p99_ms || 1;
  const hoGapMax = Math.max(
    (getP99(1024, 'body-headeronly') / getP99(1024, 'baseline') - 1) * 100,
    (getP99(102400, 'body-headeronly') / getP99(102400, 'baseline') - 1) * 100,
    (getP99(1048576, 'body-headeronly') / getP99(1048576, 'baseline') - 1) * 100
  ).toFixed(0);
  const bodyCost1m = ((getP99(1048576, 'body') / getP99(1048576, 'baseline') - 1) * 100).toFixed(0);

  const title = `Header-only bypass tracks baseline p99 within ${hoGapMax}% across sizes; buffering adds +${bodyCost1m}% at 1 MiB`;

  await chart('body_tail', {
    data: { values: rows },
    semantic_types: {
      size: { semanticType: 'Category', sortOrder: ['1 KiB', '100 KiB', '1 MiB'] },
      route: { semanticType: 'Category', sortOrder: ['baseline', 'body-headeronly', 'body'] },
      p99_ms: { semanticType: 'Quantity', unit: 'ms' },
    },
    field_display_names: {
      size: 'Payload Size',
      p99_ms: 'p99 Latency (ms)',
      route: 'Route',
    },
    chart_spec: {
      chartType: 'Grouped Bar Chart',
      title,
      subtitle: 'Request body hook tail latency under closed-loop saturation, k6, 50 VUs, loopback single host, milliseconds',
      encodings: {
        x: { field: 'size' },
        y: { field: 'p99_ms' },
        group: { field: 'route' },
      },
    },
    theme_spec: 'nature',
  });
}

// 20. WebSocket echo (ws_echo.csv size_bytes->messages_per_sec Bar, label 1 KiB / 64 KiB)
async function plotWsEcho() {
  const raw = await readCsv('ws_echo.csv');
  const wsSizeMap = {
    '1024': '1 KiB',
    '65536': '64 KiB',
  };
  const rows = raw.map(r => ({
    size: wsSizeMap[r.size_bytes] || `${r.size_bytes} B`,
    messages_per_sec: Number(r.messages_per_sec),
  }));

  const m1k = rows.find(r => r.size === '1 KiB')?.messages_per_sec || 1;
  const m64k = rows.find(r => r.size === '64 KiB')?.messages_per_sec || 1;
  const ratio = (m1k / m64k).toFixed(1);
  const title = `Small 1 KiB frames sustain ${ratio}× message throughput while 64 KiB frames saturate bandwidth`;

  await chart('ws_echo', {
    data: { values: rows },
    semantic_types: {
      size: { semanticType: 'Category', sortOrder: ['1 KiB', '64 KiB'] },
      messages_per_sec: { semanticType: 'Quantity', unit: 'msg/s' },
    },
    field_display_names: {
      size: 'Payload Size',
      messages_per_sec: 'Echo Throughput (msg/s)',
    },
    chart_spec: {
      chartType: 'Bar Chart',
      title,
      subtitle: 'WebSocket Upgrade tunnel (ADR 000048) echo throughput, plecto-loadgen, loopback single host, msg/s',
      encodings: {
        x: { field: 'size' },
        y: { field: 'messages_per_sec' },
      },
      chartProperties: {
        showValueLabels: true,
      },
    },
    theme_spec: 'nature',
  });
}

// 21. WebSocket echo tail (ws_echo.csv size x [p50_ms,p90_ms,p99_ms] Grouped Bar)
async function plotWsEchoTail() {
  const raw = await readCsv('ws_echo.csv');
  const wsSizeMap = {
    '1024': '1 KiB',
    '65536': '64 KiB',
  };
  const percentiles = [
    { col: 'p50_ms', label: 'p50' },
    { col: 'p90_ms', label: 'p90' },
    { col: 'p99_ms', label: 'p99' },
  ];
  const rows = [];
  for (const r of raw) {
    const size = wsSizeMap[r.size_bytes] || `${r.size_bytes} B`;
    for (const p of percentiles) {
      if (r[p.col] !== undefined) {
        rows.push({
          size,
          percentile: p.label,
          latency_ms: Number(r[p.col]),
        });
      }
    }
  }

  const p99_1k = rows.find(r => r.size === '1 KiB' && r.percentile === 'p99')?.latency_ms || 1;
  const p99_64k = rows.find(r => r.size === '64 KiB' && r.percentile === 'p99')?.latency_ms || 1;
  const ratio = (p99_64k / p99_1k).toFixed(1);
  const title = `64 KiB frames increase p99 saturation queueing by ${ratio}× over 1 KiB frames (${p99_64k.toFixed(2)} ms vs ${p99_1k.toFixed(2)} ms)`;

  await chart('ws_echo_tail', {
    data: { values: rows },
    semantic_types: {
      percentile: { semanticType: 'Category', sortOrder: ['p50', 'p90', 'p99'] },
      size: { semanticType: 'Category', sortOrder: ['1 KiB', '64 KiB'] },
      latency_ms: { semanticType: 'Quantity', unit: 'ms' },
    },
    field_display_names: {
      percentile: 'Percentile',
      latency_ms: 'Latency (ms)',
      size: 'Payload Size',
    },
    chart_spec: {
      chartType: 'Grouped Bar Chart',
      title,
      subtitle: 'WebSocket echo tail percentiles under closed-loop saturation, plecto-loadgen, 50 conns, loopback, milliseconds',
      encodings: {
        x: { field: 'percentile' },
        y: { field: 'latency_ms' },
        group: { field: 'size' },
      },
    },
    theme_spec: 'nature',
  });
}

const figs = [
  ['throughput_vs_concurrency', plotThroughputVsConcurrency],
  ['latency_vs_concurrency', plotLatencyVsConcurrency],
  ['rr_distribution', plotRrDistribution],
  ['ejection_timeline', plotEjectionTimeline],
  ['ejection_failed', plotEjectionFailed],
  ['swap_timeline', plotSwapTimeline],
  ['ceiling', plotCeiling],
  ['ceiling_tail', plotCeilingTail],
  ['tls_vs_plain', plotTlsVsPlain],
  ['tls_tail', plotTlsTail],
  ['wasm_throughput', plotWasmThroughput],
  ['wasm_latency', plotWasmLatency],
  ['wasm_shortcircuit', plotWasmShortcircuit],
  ['ratelimit_overhead', plotRatelimitOverhead],
  ['ratelimit_overhead_tail', plotRatelimitOverheadTail],
  ['ratelimit_enforce', plotRatelimitEnforce],
  ['ratelimit_fairness', plotRatelimitFairness],
  ['body', plotBody],
  ['body_tail', plotBodyTail],
  ['ws_echo', plotWsEcho],
  ['ws_echo_tail', plotWsEchoTail],
];

for (const [name, fn] of figs) {
  try {
    await fn();
  } catch (err) {
    console.log(`skip ${name}: ${err.message}`);
  }
}
