// filter-tokenlimit-js — spend a per-caller token budget on LLM requests, in JavaScript.
//
// A plain request-rate limiter is the wrong unit in front of an LLM upstream: one request can
// cost a thousand times another. This filter charges each request against a host-side token
// bucket by an ESTIMATE of what it will cost — input text plus the output the caller reserved —
// so a caller that sends huge prompts exhausts its budget quickly and one that sends short ones
// keeps going. The estimate is deliberately cheap and deterministic (byte count / divisor, no
// tokenizer): the exact number matters far less than charging in the right unit, and a filter on
// the hot path cannot afford a real tokenizer.
//
// Rate limiting is HOST-NATIVE (ADR 000005): the bucket's capacity and refill live in the
// operator's manifest and the counting never crosses the WASM boundary. This filter only decides
// WHICH key to charge and HOW MUCH — the two decisions a token-cost policy is actually about.
//
// Componentized with ComponentizeJS with random/stdio/clocks/http/fetch-event DISABLED, so the
// produced component imports only the plecto host-API ("pure component"). Consequence:
// Date.now() / Math.random() are unavailable in here by design. Nothing below needs them — the
// bucket refills against the host's own per-request clock snapshot.

import { log } from 'plecto:filter/host-log@0.4.0';
import { get as configGet } from 'plecto:filter/host-config@0.4.0';
import { tryAcquire } from 'plecto:filter/host-ratelimit@0.4.0';

// The largest value `host-ratelimit.try-acquire` can accept (`u64`). A cost above it would throw
// while being lowered across the boundary — a confusing trap where a clean 429 is the honest
// answer, since a bucket that could satisfy this cost cannot be configured either.
const U64_MAX = 18446744073709551615n;

// Ceiling on a client-supplied `max_tokens`. The value arrives from an untrusted body and no real
// upstream generates anywhere near this much, so an absurd reservation is priced at the ceiling
// rather than at whatever the caller typed — which keeps the arithmetic below far away from the
// saturation point where an overflowing cost could round back down into a cheap request.
const MAX_TOKENS_CEILING = 100000000n;

// Longest `model` this filter is willing to turn into a `model-cost-percent.<model>` lookup. The
// name is untrusted body content; nothing plausible is longer, and the fallback (100%) charges
// MORE than any discount would, so refusing to look up an implausible name fails safe.
const MAX_MODEL_NAME_BYTES = 128;

// Effective operator config, resolved once in `init`. Null until then; a hot-path call before
// `init` would throw and trap, which is the correct answer to "ran without configuration".
let config = null;

// Percent multipliers already read from host-config, keyed by model name. Only HITS are cached:
// the model name comes from the request body, so caching misses would let a caller grow this map
// without bound on a pooled instance. Hits are bounded by the manifest.
const modelPercents = new Map();

// The per-request scratch — the only mutable state this filter keeps, and the reason to read the
// scratch rule in the README before copying this pattern. `on-request` OVERWRITES all three
// fields on every path before anything reads them, so a pooled `trusted` instance can never let
// one request see another's key, cost, or remaining budget. Nothing here is business state:
// that belongs in host KV (Tenet: filters are stateless).
let scratch = { key: null, cost: null, remaining: null };

const utf8 = (s) => new TextEncoder().encode(s);
const fromUtf8 = (bytes) => new TextDecoder().decode(bytes);

// Integer ceiling division. Both operands are non-negative, so BigInt's truncating `/` is floor
// division and this identity holds.
const ceilDiv = (n, d) => (n + d - 1n) / d;

// Header values are the contract's `list<u8>` (ADR 000071), lifted as a Uint8Array. Header names
// are compared lowercased because a client picks the case, not the operator.
function findHeader(headers, name) {
  return headers.find((h) => h.name.toLowerCase() === name);
}

// Decode a header value as UTF-8, or `null` if those bytes are not UTF-8 at all.
//
// The rate-limit key crosses the boundary as a WIT `string`, which MUST be valid UTF-8 — a
// malformed value would trap on the way out, and the data plane never traps on client input. The
// check is a round trip rather than `TextDecoder`'s optional `fatal` mode (not something to rely on
// in an engine built with most of its WASI-backed features switched off): the default decoder
// substitutes U+FFFD for every malformed sequence, so bytes that do not survive decode-then-encode
// were not UTF-8. Non-ASCII is perfectly acceptable — only malformed bytes are rejected.
function utf8Text(bytes) {
  const decoded = fromUtf8(bytes);
  const reencoded = utf8(decoded);
  if (reencoded.length !== bytes.length) return null;
  for (let i = 0; i < bytes.length; i++) {
    if (reencoded[i] !== bytes[i]) return null;
  }
  return decoded;
}

// A filter-authored JSON error response. `content-length` is never set: the host owns framing and
// rejects a guest-supplied one fail-closed.
function jsonError(status, message, extraHeaders = []) {
  return {
    status,
    headers: [{ name: 'content-type', value: utf8('application/json') }, ...extraHeaders],
    body: utf8(`{"error":"${message}"}`),
  };
}

// Strict decimal parse. `Number()` / `parseInt()` would quietly accept "12abc", "0x10", " 7 " and
// "1e3"; an operator typo in a cost multiplier must fail loudly instead of meaning something else.
function parseCount(raw) {
  return /^[0-9]+$/.test(raw) ? BigInt(raw) : null;
}

// Read an optional numeric config key, or refuse to load. Trapping in `init` rather than defaulting
// is the fail-closed choice: combined with `isolation = "trusted"` it surfaces as a filter that
// never loads (ADR 000066), so a typo cannot turn into requests being charged the wrong amount for
// as long as nobody notices.
function requireCount(key, fallback, min) {
  const raw = configGet(key);
  if (raw === undefined) return fallback;
  const parsed = parseCount(raw);
  if (parsed === null || parsed < min) {
    // stdio is disabled, so the trap message itself goes nowhere — say why through host-log first.
    log('error', `filter-tokenlimit-js: [filter.config] ${key} must be an integer >= ${min}, got "${raw}"`);
    throw new Error(`filter-tokenlimit-js: invalid config value for ${key}`);
  }
  return parsed;
}

// Per-model cost multiplier, in percent (100 = charge the estimate as-is). Absent means 100; an
// unparseable value traps for the same reason `init` does — a limiter that silently mis-charges is
// worse than one that stops.
function modelPercent(model) {
  // An unnamed model, or a name too long to be one, is never turned into a config lookup: the
  // string is untrusted body content, and 100% is the expensive side of the fallback.
  if (model === '' || utf8(model).length > MAX_MODEL_NAME_BYTES) return 100n;

  const cached = modelPercents.get(model);
  if (cached !== undefined) return cached;

  const raw = configGet(`model-cost-percent.${model}`);
  if (raw === undefined) return 100n;

  const parsed = parseCount(raw);
  if (parsed === null) {
    log('error', `filter-tokenlimit-js: [filter.config] model-cost-percent.${model} must be an integer, got "${raw}"`);
    throw new Error('filter-tokenlimit-js: invalid model cost multiplier');
  }
  modelPercents.set(model, parsed);
  return parsed;
}

// The output reservation. Cost has to be charged BEFORE the model runs, so what the caller asks to
// generate is the only honest estimate of output. Anything that is not a non-negative integer —
// negative, fractional, a string, NaN, or past the exact-integer range of a JSON number — falls
// back to the configured default rather than to zero, so `"max_tokens": -1` or `12.9` cannot buy a
// cheaper ride than saying nothing at all. A genuine integer is clamped, not rejected.
function reservation(raw) {
  if (typeof raw !== 'number' || !Number.isSafeInteger(raw) || raw < 0) {
    return config.maxTokensDefault;
  }
  const asked = BigInt(raw);
  return asked > MAX_TOKENS_CEILING ? MAX_TOKENS_CEILING : asked;
}

// Everything in the request that will be fed to the model as input. Non-string message content
// (tool calls, multi-part arrays) is ignored rather than guessed at: a starter that mis-measures
// is worse than one that measures only what it understands — extend this where your schema differs.
function promptText(doc) {
  let text = typeof doc.prompt === 'string' ? doc.prompt : '';
  if (Array.isArray(doc.messages)) {
    for (const message of doc.messages) {
      if (message && typeof message.content === 'string') text += message.content;
    }
  }
  return text;
}

export function init() {
  const rawKeyHeader = configGet('key-header');
  if (rawKeyHeader === '') {
    // An empty header name matches nothing, so this filter would answer every request 401. That is
    // a broken configuration, not a strict one: refuse to load rather than 401 the world.
    log('error', 'filter-tokenlimit-js: [filter.config] key-header must not be empty');
    throw new Error('filter-tokenlimit-js: empty key-header');
  }
  config = {
    keyHeader: (rawKeyHeader ?? 'x-api-key').toLowerCase(),
    maxTokensDefault: requireCount('max-tokens-default', 1024n, 0n),
    // Minimum 1, not 0: JavaScript would divide by a zero divisor and yield Infinity rather than
    // failing, so the value that cannot work is rejected here instead of poisoning every estimate.
    charsPerToken: requireCount('chars-per-token', 4n, 1n),
  };
  modelPercents.clear();

  log(
    'info',
    `filter-tokenlimit-js: key-header=${config.keyHeader} ` +
      `max-tokens-default=${config.maxTokensDefault} chars-per-token=${config.charsPerToken}`,
  );
}

export function onRequest(req) {
  // Overwrite the whole scratch first, on every path — see the note on `scratch` above.
  scratch = { key: null, cost: null, remaining: null };

  const header = findHeader(req.headers, config.keyHeader);
  // Missing, empty, and not-UTF-8 are all "no usable key" and get the SAME answer: distinguishing
  // them in the response body would tell a prober something about the gate it does not need.
  const key = header === undefined ? null : utf8Text(header.value);
  if (key === null || key === '') {
    // Fail closed: an unidentified caller has no budget to spend, so it never reaches the upstream.
    return { tag: 'short-circuit', val: jsonError(401, 'missing api key') };
  }

  // Headers are NOT available in `on-request-body`, so the key is carried across the two hooks
  // here. Charging happens there, where the body reveals what the request will actually cost.
  scratch.key = key;
  return { tag: 'continue' };
}

export function onRequestBody(body) {
  let parsed;
  try {
    parsed = JSON.parse(fromUtf8(body));
  } catch {
    // A body this filter cannot read is a body it cannot price. Fail closed rather than forward
    // an unmeasured request.
    return { tag: 'short-circuit', val: jsonError(400, 'invalid json body') };
  }
  // A parseable non-object (a bare number, a list) is priced as an empty document: it carries no
  // prompt, so it is charged the default reservation and nothing more.
  const doc = parsed !== null && typeof parsed === 'object' ? parsed : {};

  const inputEst = ceilDiv(BigInt(utf8(promptText(doc)).length), config.charsPerToken);
  const base = inputEst + reservation(doc.max_tokens);
  const model = typeof doc.model === 'string' ? doc.model : '';

  let cost = (base * modelPercent(model)) / 100n;
  // Floor 1: a request that estimates to nothing still consumed an upstream slot, and a bucket
  // that can be spent 0 tokens at a time is not a limiter.
  if (cost < 1n) cost = 1n;
  if (cost > U64_MAX) cost = U64_MAX;

  const outcome = tryAcquire(scratch.key, cost);
  if (!outcome.allowed) {
    // Never log the key: it is the caller's credential and fully caller-controlled text.
    log('warn', `filter-tokenlimit-js: token budget exhausted (cost=${cost})`);
    return {
      tag: 'short-circuit',
      val: jsonError(429, 'token budget exhausted', [
        // RFC 9110 Retry-After in delay-seconds; the host reports milliseconds, and rounding UP is
        // the safe direction — a client that waits the rounded-down value would just be denied again.
        { name: 'retry-after', value: utf8(String(ceilDiv(outcome.retryAfterMs, 1000n))) },
      ]),
    };
  }

  scratch.cost = cost;
  scratch.remaining = outcome.remaining;
  // Bare `%continue` (0.4.0, ADR 000098): this filter only INSPECTED the body, so the host forwards
  // what it already buffered instead of paying a full copy back across the boundary to hear
  // "unchanged".
  return { tag: 'continue' };
}

export function onResponse(_req, _resp) {
  // Only requests that actually reached the acquire path have a cost to report. A 401'd or 400'd
  // request leaves `cost` at the null `on-request` wrote, so it reports nothing.
  if (scratch.cost === null) {
    return { tag: 'continue' };
  }
  return {
    tag: 'modified',
    val: {
      setStatus: undefined,
      setHeaders: [
        { name: 'x-tokenlimit-cost', value: utf8(String(scratch.cost)) },
        { name: 'x-tokenlimit-remaining', value: utf8(String(scratch.remaining)) },
      ],
      removeHeaders: [],
    },
  };
}
