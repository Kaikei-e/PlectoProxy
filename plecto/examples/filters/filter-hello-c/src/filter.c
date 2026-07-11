// filter-hello-c — the filter-hello conformance subset in C, proving the
// `plecto:filter` contract is language-neutral (Component Model, zero WASI imports).
//
// Behaviour (mirrors filter-hello's contract-exercising subset):
//   - init: bump the host counter `init-calls`.
//   - on-request: log; `x-plecto-addheader` -> modified (adds x-plecto-added: 1);
//     `x-plecto-ratelimit` -> consult the host token bucket, short-circuit 429 when empty;
//     `x-plecto-block` -> short-circuit 403; otherwise continue.
//   - on-request-body: short-circuit 403 on a `deny-body` marker, else uppercase + continue.
//   - on-response: `x-plecto-respedit` -> modified (adds x-plecto-respadded: 1), else continue.
//
// Ownership: parameters are callee-owned (freed before returning); returned values are
// heap-allocated because the generated post-return frees them after lowering.

#include <stdlib.h>
#include <string.h>

#include "filter_body.h"

static int ascii_ieq(const uint8_t *a, size_t alen, const char *b) {
    size_t blen = strlen(b);
    if (alen != blen) {
        return 0;
    }
    for (size_t i = 0; i < alen; i++) {
        uint8_t ca = a[i];
        uint8_t cb = (uint8_t)b[i];
        if (ca >= 'A' && ca <= 'Z') {
            ca += 32;
        }
        if (cb >= 'A' && cb <= 'Z') {
            cb += 32;
        }
        if (ca != cb) {
            return 0;
        }
    }
    return 1;
}

static const plecto_filter_types_header_t *find_header(
    const plecto_filter_types_list_header_t *headers, const char *name) {
    for (size_t i = 0; i < headers->len; i++) {
        const plecto_filter_types_header_t *h = &headers->ptr[i];
        if (ascii_ieq(h->name.ptr, h->name.len, name)) {
            return h;
        }
    }
    return NULL;
}

// import arguments are lifted (copied) by the host during the call, so a static
// reference is enough — only EXPORT return values need heap allocation.
static void log_info(const char *msg) {
    filter_body_string_t s;
    filter_body_string_set(&s, msg);
    plecto_filter_host_log_log(PLECTO_FILTER_HOST_LOG_LEVEL_INFO, &s);
}

static size_t u64_to_str(uint64_t v, char *out) {
    char digits[20];
    size_t d = 0;
    do {
        digits[d++] = (char)('0' + (v % 10));
        v /= 10;
    } while (v > 0);
    size_t n = 0;
    while (d > 0) {
        out[n++] = digits[--d];
    }
    out[n] = '\0';
    return n;
}

// header.value is list<u8> (ADR 000071) — dup the C string's bytes into a heap buffer the
// generated post-return will free, same ownership rule as filter_body_string_dup.
static void list_u8_dup(filter_body_list_u8_t *ret, const char *s) {
    size_t len = strlen(s);
    ret->ptr = malloc(len);
    ret->len = len;
    memcpy(ret->ptr, s, len);
}

static plecto_filter_types_list_header_t one_header(const char *name, const char *value) {
    plecto_filter_types_list_header_t list;
    list.ptr = malloc(sizeof(plecto_filter_types_header_t));
    list.len = 1;
    filter_body_string_dup(&list.ptr[0].name, name);
    list_u8_dup(&list.ptr[0].value, value);
    return list;
}

static plecto_filter_types_http_response_t make_response(
    uint16_t status, const char *header_name, const char *header_value, const char *body) {
    plecto_filter_types_http_response_t resp;
    resp.status = status;
    resp.headers = one_header(header_name, header_value);
    size_t body_len = strlen(body);
    resp.body.ptr = malloc(body_len);
    resp.body.len = body_len;
    memcpy(resp.body.ptr, body, body_len);
    return resp;
}

void exports_filter_body_init(void) {
    filter_body_string_t key;
    filter_body_string_set(&key, "init-calls");
    plecto_filter_host_counter_increment(&key, 1);
}

void exports_filter_body_on_request(
    filter_body_http_request_t *req, filter_body_request_decision_t *ret) {
    log_info("filter-hello-c: on-request");

    // observable init-once signal, mirroring filter-hello's `init-calls=N` log line
    filter_body_string_t counter_key;
    filter_body_string_set(&counter_key, "init-calls");
    int64_t inits = plecto_filter_host_counter_get(&counter_key);
    char line[32] = "init-calls=";
    u64_to_str(inits < 0 ? 0 : (uint64_t)inits, line + 11);
    log_info(line);

    if (find_header(&req->headers, "x-plecto-addheader") != NULL) {
        ret->tag = PLECTO_FILTER_TYPES_REQUEST_DECISION_MODIFIED;
        ret->val.modified.set_headers = one_header("x-plecto-added", "1");
        ret->val.modified.remove_headers.ptr = NULL;
        ret->val.modified.remove_headers.len = 0;
        filter_body_http_request_free(req);
        return;
    }

    const plecto_filter_types_header_t *rl = find_header(&req->headers, "x-plecto-ratelimit");
    if (rl != NULL) {
        filter_body_string_t key;
        if (rl->value.len == 0) {
            filter_body_string_dup(&key, "default");
        } else {
            filter_body_string_dup_n(&key, (const char *)rl->value.ptr, rl->value.len);
        }
        plecto_filter_host_ratelimit_acquire_t outcome;
        plecto_filter_host_ratelimit_try_acquire(&key, 1, &outcome);
        filter_body_string_free(&key);
        if (!outcome.allowed) {
            char retry[21];
            u64_to_str(outcome.retry_after_ms, retry);
            ret->tag = PLECTO_FILTER_TYPES_REQUEST_DECISION_SHORT_CIRCUIT;
            ret->val.short_circuit =
                make_response(429, "retry-after-ms", retry, "rate limited by filter-hello-c");
            filter_body_http_request_free(req);
            return;
        }
    }

    if (find_header(&req->headers, "x-plecto-block") != NULL) {
        ret->tag = PLECTO_FILTER_TYPES_REQUEST_DECISION_SHORT_CIRCUIT;
        ret->val.short_circuit =
            make_response(403, "x-plecto", "blocked", "blocked by filter-hello-c");
        filter_body_http_request_free(req);
        return;
    }

    ret->tag = PLECTO_FILTER_TYPES_REQUEST_DECISION_CONTINUE;
    filter_body_http_request_free(req);
}

void exports_filter_body_on_request_body(
    filter_body_list_u8_t *body, filter_body_request_body_decision_t *ret) {
    log_info("filter-hello-c: on-request-body");

    if (body->len >= 9) {
        for (size_t i = 0; i + 9 <= body->len; i++) {
            if (ascii_ieq(body->ptr + i, 9, "deny-body")) {
                ret->tag = PLECTO_FILTER_TYPES_REQUEST_BODY_DECISION_SHORT_CIRCUIT;
                ret->val.short_circuit = make_response(
                    403, "x-plecto", "blocked-body", "blocked body by filter-hello-c");
                filter_body_list_u8_free(body);
                return;
            }
        }
    }

    // uppercase in place and hand the same buffer back (ownership moves to the return)
    for (size_t i = 0; i < body->len; i++) {
        if (body->ptr[i] >= 'a' && body->ptr[i] <= 'z') {
            body->ptr[i] -= 32;
        }
    }
    ret->tag = PLECTO_FILTER_TYPES_REQUEST_BODY_DECISION_CONTINUE;
    ret->val.continue_ = *body;
}

void exports_filter_body_on_response(
    filter_body_http_request_t *req, filter_body_http_response_t *resp,
    filter_body_response_decision_t *ret) {
    // The as-forwarded request snapshot (plecto:filter@0.3.0, ADR 000073) — unused by this
    // conformance subset, but ownership is ours to release.
    filter_body_http_request_free(req);
    if (find_header(&resp->headers, "x-plecto-respedit") != NULL) {
        ret->tag = PLECTO_FILTER_TYPES_RESPONSE_DECISION_MODIFIED;
        ret->val.modified.set_status.is_some = false;
        ret->val.modified.set_status.val = 0;
        ret->val.modified.set_headers = one_header("x-plecto-respadded", "1");
        ret->val.modified.remove_headers.ptr = NULL;
        ret->val.modified.remove_headers.len = 0;
        filter_body_http_response_free(resp);
        return;
    }
    ret->tag = PLECTO_FILTER_TYPES_RESPONSE_DECISION_CONTINUE;
    filter_body_http_response_free(resp);
}
