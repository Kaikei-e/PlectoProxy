// filter-hello-js — the filter-hello conformance subset in JavaScript, proving the
// `plecto:filter` contract is language-neutral (Component Model, zero WASI imports).
//
// Componentized with ComponentizeJS with random/stdio/clocks/http/fetch-event DISABLED,
// so the produced component imports only the plecto host-API ("pure component").
// Consequence: Date.now() / Math.random() are unavailable in here by design — time
// comes from host-clock, exactly as the contract intends.

import { log } from 'plecto:filter/host-log@0.1.0';
import { increment, get as counterGet } from 'plecto:filter/host-counter@0.1.0';
import { tryAcquire } from 'plecto:filter/host-ratelimit@0.1.0';

function findHeader(headers, name) {
  return headers.find((h) => h.name.toLowerCase() === name);
}

function text(s) {
  return new TextEncoder().encode(s);
}

export function init() {
  increment('init-calls', 1n);
}

export function onRequest(req) {
  log('info', 'filter-hello-js: on-request');
  log('info', `init-calls=${counterGet('init-calls')}`);

  if (findHeader(req.headers, 'x-plecto-addheader')) {
    return {
      tag: 'modified',
      val: {
        setHeaders: [{ name: 'x-plecto-added', value: '1' }],
        removeHeaders: [],
      },
    };
  }

  const rl = findHeader(req.headers, 'x-plecto-ratelimit');
  if (rl) {
    const key = rl.value === '' ? 'default' : rl.value;
    const outcome = tryAcquire(key, 1n);
    if (!outcome.allowed) {
      return {
        tag: 'short-circuit',
        val: {
          status: 429,
          headers: [{ name: 'retry-after-ms', value: String(outcome.retryAfterMs) }],
          body: text('rate limited by filter-hello-js'),
        },
      };
    }
  }

  if (findHeader(req.headers, 'x-plecto-block')) {
    return {
      tag: 'short-circuit',
      val: {
        status: 403,
        headers: [{ name: 'x-plecto', value: 'blocked' }],
        body: text('blocked by filter-hello-js'),
      },
    };
  }

  return { tag: 'continue' };
}

export function onRequestBody(body) {
  log('info', 'filter-hello-js: on-request-body');

  const marker = 'deny-body';
  outer: for (let i = 0; i + marker.length <= body.length; i++) {
    for (let j = 0; j < marker.length; j++) {
      let c = body[i + j];
      if (c >= 0x41 && c <= 0x5a) c += 32;
      if (c !== marker.charCodeAt(j)) continue outer;
    }
    return {
      tag: 'short-circuit',
      val: {
        status: 403,
        headers: [{ name: 'x-plecto', value: 'blocked-body' }],
        body: text('blocked body by filter-hello-js'),
      },
    };
  }

  const upper = new Uint8Array(body.length);
  for (let i = 0; i < body.length; i++) {
    const c = body[i];
    upper[i] = c >= 0x61 && c <= 0x7a ? c - 32 : c;
  }
  return { tag: 'continue', val: upper };
}

export function onResponse(resp) {
  if (findHeader(resp.headers, 'x-plecto-respedit')) {
    return {
      tag: 'modified',
      val: {
        setStatus: undefined,
        setHeaders: [{ name: 'x-plecto-respadded', value: '1' }],
        removeHeaders: [],
      },
    };
  }
  return { tag: 'continue' };
}
