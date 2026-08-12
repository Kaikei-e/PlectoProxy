// filter-tokenlimit-go — a token-cost rate limiter for LLM-style JSON APIs, written in Go and
// compiled with TinyGo to a `plecto:filter@0.4.0` component (Tier B / fat guest, ADR 000063).
//
// The idea: a plain request-per-second limiter is the wrong unit in front of a model-serving
// upstream, where one request can be a thousand times more expensive than the next. This filter
// prices each request from its own JSON payload — input size plus the output the caller reserved —
// and spends that price against the HOST's token bucket (`host-ratelimit`), whose capacity and
// refill the operator owns in the manifest. The filter decides WHAT to charge; it can never widen
// its own budget.
//
// It is a copy-paste starter, not a shipped reference filter (ADR 000080) — read it, fork it,
// change the cost formula to whatever your upstream actually bills.
package main

import (
	"strconv"
	"unicode/utf8"

	hostconfig "filter-tokenlimit-go/internal/plecto/filter/host-config"
	hostlog "filter-tokenlimit-go/internal/plecto/filter/host-log"
	hostratelimit "filter-tokenlimit-go/internal/plecto/filter/host-ratelimit"
	"filter-tokenlimit-go/internal/plecto/filter/types"
	guest "filter-tokenlimit-go/internal/plecto/tokenlimit-go/filter-request-body"

	"github.com/tidwall/gjson"
	"go.bytecodealliance.org/cm"
)

// --- operator-owned configuration ------------------------------------------------------------
//
// Defaults, and the `[filter.config]` keys that override them. Every key is optional; a key that
// IS present must parse, and `init` traps if it does not (ADR 000066): with
// `isolation = "trusted"` the host eager-builds one instance at load, so a typo in the manifest
// surfaces as a load failure instead of a surprise 500 on the first real request.
const (
	defaultKeyHeader     = "x-api-key"
	defaultMaxTokens     = uint64(1024)
	defaultCharsPerToken = uint64(4)
	// A model with no configured multiplier is charged at face value.
	defaultCostPercent = uint64(100)
	// `model-cost-percent.<model>` — one config key per model name.
	costPercentPrefix = "model-cost-percent."
	// An upper bound on the `model` string we are willing to turn into a config lookup key.
	// The value comes from an untrusted body, and nothing useful is longer than this.
	maxModelNameLen = 128
	// Ceiling on a client-supplied `max_tokens`. No real upstream generates anywhere near this
	// much, and clamping keeps the cost arithmetic far from the saturation point where an
	// overflow could round an expensive request back down into a cheap one.
	maxTokensCeiling = uint64(100_000_000)
	// Largest JSON number that still denotes an exact integer (2^53 - 1). Past it the number
	// that carried the value could not represent it exactly, so it is not a token count the
	// client can have meant.
	maxExactJSONInt = float64(9_007_199_254_740_991)
)

var (
	keyHeader        = defaultKeyHeader
	maxTokensDefault = defaultMaxTokens
	charsPerToken    = defaultCharsPerToken
)

// --- per-request scratch ----------------------------------------------------------------------
//
// The three hooks of one request are three separate calls into the same instance, and WIT passes
// no context between them, so what `on-request` learns has to survive in an instance global until
// `on-request-body` and `on-response` need it.
//
// THE SCRATCH RULE: `on-request` overwrites the WHOLE struct before it reads anything from it, and
// no hook reads a field the same request has not written. That is what keeps the filter stateless
// in the sense that matters (Tenet: filters are stateless; durable state belongs to the host):
// with `isolation = "trusted"` the host pools and REUSES instances, so a leftover key or cost from
// the previous request is live data from another caller. Resetting on entry — not on exit — means
// even a trapped or short-circuited request cannot leak into the next one.
type requestScratch struct {
	// the rate-limit key taken from `key-header`, written by on-request
	key string
	// what this request cost and what is left, written by on-request-body on the acquire path
	cost      uint64
	remaining uint64
	// whether that acquire path ran at all — on-response reports nothing without it
	acquired bool
}

var scratch requestScratch

func init() {
	guest.Exports.Init = onInit
	guest.Exports.OnRequest = onRequest
	guest.Exports.OnRequestBody = onRequestBody
	guest.Exports.OnResponse = onResponse
}

// onInit resolves the manifest config once per instance (Tenet: init is where the expensive and
// the fallible work goes; the per-request hooks stay hot and total).
func onInit() {
	if raw, ok := configString("key-header"); ok {
		if raw == "" {
			// An empty header name can never match anything, so every request would 401. Refuse
			// the config instead of failing every request at runtime.
			panic("filter-tokenlimit-go: key-header must not be empty")
		}
		keyHeader = raw
	}
	maxTokensDefault = configUint("max-tokens-default", defaultMaxTokens, 0)
	// A zero would divide by zero in the cost formula, so 1 byte per token is the floor.
	charsPerToken = configUint("chars-per-token", defaultCharsPerToken, 1)

	hostlog.Log(hostlog.LevelInfo, "filter-tokenlimit-go: key-header="+keyHeader+
		" max-tokens-default="+strconv.FormatUint(maxTokensDefault, 10)+
		" chars-per-token="+strconv.FormatUint(charsPerToken, 10))
}

// onRequest sees headers only — the body has not been buffered yet — so all it does is establish
// the rate-limit key. Everything that needs the payload waits for on-request-body.
func onRequest(req types.HTTPRequest) types.RequestDecision {
	// The scratch rule, in one line: this request starts from zero, whatever the previous one
	// left behind in this pooled instance.
	scratch = requestScratch{}

	value, found := headerValue(req.Headers.Slice(), keyHeader)
	if !found || len(value) == 0 {
		// Fail closed: an unattributable request cannot be charged to anyone, so it does not
		// reach the upstream at all.
		return types.RequestDecisionShortCircuit(jsonResponse(401, `{"error":"missing api key"}`))
	}
	if !utf8.Valid(value) {
		// Header values are raw bytes (ADR 000071) but `host-ratelimit.try-acquire` takes a WIT
		// `string`, which must be valid UTF-8. Rejecting here keeps a malformed byte from an
		// untrusted client from turning into a trap on the boundary — the data plane never
		// panics on input it was handed.
		return types.RequestDecisionShortCircuit(jsonResponse(401, `{"error":"missing api key"}`))
	}

	scratch.key = string(value)
	return types.RequestDecisionContinue()
}

// onRequestBody is where the request is priced and charged. The host buffers the request body and
// calls this hook because the component exports it (ADR 000025 / 000098); the headers are NOT
// available here, which is exactly why on-request stashed the key.
func onRequestBody(body cm.List[uint8]) types.RequestBodyDecision {
	// One copy into a string, deliberately: it keeps gjson on its plain-string path instead of
	// the unsafe slice-header cast of `GetBytes`, which is worth more than the copy in a guest
	// whose runtime is not the one that code was written against.
	raw := string(body.Slice())

	// gjson is a scanning reader, not a validating parser — it will happily "find" fields in
	// malformed input, so validity is asserted first and a bad body is refused.
	if !gjson.Valid(raw) {
		return types.RequestBodyDecisionShortCircuit(jsonResponse(400, `{"error":"invalid json body"}`))
	}
	payload := gjson.Parse(raw)

	model := ""
	if m := payload.Get("model"); m.Type == gjson.String {
		model = m.String()
	}

	// The caller's own output reservation is part of the price: a request that asks for 4k output
	// tokens must not cost the same as one asking for 16.
	//
	// Only a NON-NEGATIVE INTEGER counts. Negative, fractional, NaN, and past-exact-integer values
	// all fall back to the operator's default rather than being coerced: truncating `12.9` to 12 or
	// clamping `-1` to 0 would make a malformed value CHEAPER than omitting the field entirely,
	// which is the one direction a limiter must never fail. A genuine integer is clamped, not
	// rejected — an absurd reservation should price absurdly and be denied by the bucket.
	maxTokens := maxTokensDefault
	if mt := payload.Get("max_tokens"); mt.Type == gjson.Number {
		n := mt.Num
		// `n == n` rejects NaN without importing math; the truncation round-trip rejects fractions.
		if n == n && n >= 0 && n <= maxExactJSONInt && float64(uint64(n)) == n {
			maxTokens = uint64(n)
			if maxTokens > maxTokensCeiling {
				maxTokens = maxTokensCeiling
			}
		}
	}

	// The billable text: `prompt` plus every string `messages[i].content`. Non-string contents
	// (tool calls, multimodal parts) are ignored rather than guessed at — a starter should be
	// obvious about what it does and does not price.
	//
	// Only the LENGTH is accumulated, never the concatenation itself: `len(string)` in Go is
	// already the UTF-8 byte count, so a megabyte body is priced without a megabyte of copying.
	textBytes := 0
	if p := payload.Get("prompt"); p.Type == gjson.String {
		textBytes += len(p.String())
	}
	payload.Get("messages").ForEach(func(_, message gjson.Result) bool {
		if c := message.Get("content"); c.Type == gjson.String {
			textBytes += len(c.String())
		}
		return true
	})

	// Integer math end to end — no floats, so the price is exactly reproducible across languages
	// and across a retry. `ceil` on the input estimate rounds toward charging more.
	inputEst := ceilDiv(uint64(textBytes), charsPerToken)
	base := satAdd(inputEst, maxTokens)
	cost := satMul(base, modelCostPercent(model)) / 100
	if cost == 0 {
		// Even a free-looking request costs something; otherwise an empty-body flood is unmetered.
		cost = 1
	}

	// The bucket itself is host-native (ADR 000005): the refill arithmetic never crosses the WASM
	// boundary, the filter only decides the key and the price.
	outcome := hostratelimit.TryAcquire(scratch.key, cost)
	if !outcome.Allowed {
		return types.RequestBodyDecisionShortCircuit(types.HTTPResponse{
			Status: 429,
			Headers: cm.ToList([]types.Header{
				header("content-type", "application/json"),
				// Seconds, rounded UP: telling a client to come back in 0 s when the bucket needs
				// 900 ms just buys another rejection.
				header("retry-after", strconv.FormatUint(ceilDiv(outcome.RetryAfterMs, 1000), 10)),
			}),
			Body: cm.ToList([]byte(`{"error":"token budget exhausted"}`)),
		})
	}

	scratch.cost = cost
	scratch.remaining = outcome.Remaining
	scratch.acquired = true

	// The bare `%continue` arm (0.4.0, ADR 000098): this filter inspects the body, it does not
	// rewrite it, so the buffered bytes are never handed back across the boundary.
	return types.RequestBodyDecisionContinue()
}

// onResponse reports what the request was charged, so a client can see its budget draining
// without a separate API. `req` is the as-forwarded request snapshot (ADR 000073); nothing here
// needs it, because the cost is already in scratch.
func onResponse(req types.HTTPRequest, resp types.HTTPResponse) types.ResponseDecision {
	if !scratch.acquired {
		// A request that never reached the acquire path (missing key, bad JSON) has nothing to
		// report — and reading `cost` here would be reading a field this request never wrote.
		return types.ResponseDecisionContinue()
	}
	return types.ResponseDecisionModified(types.ResponseEdit{
		SetStatus: cm.None[uint16](),
		SetHeaders: cm.ToList([]types.Header{
			header("x-tokenlimit-cost", strconv.FormatUint(scratch.cost, 10)),
			header("x-tokenlimit-remaining", strconv.FormatUint(scratch.remaining, 10)),
		}),
		RemoveHeaders: cm.ToList([]string{}),
	})
}

// modelCostPercent looks up the per-model multiplier, defaulting to face value. A model name that
// is empty or implausibly long is never looked up: the value is untrusted body content, and the
// default charges MORE than a discount would, so falling back is the safe direction.
func modelCostPercent(model string) uint64 {
	if model == "" || len(model) > maxModelNameLen {
		return defaultCostPercent
	}
	raw, ok := configString(costPercentPrefix + model)
	if !ok {
		return defaultCostPercent
	}
	percent, err := strconv.ParseUint(raw, 10, 64)
	if err != nil {
		// Unlike the keys `init` validates, this one is only discoverable once a request names the
		// model, so it cannot fail the load. It fails the REQUEST instead: falling back to 100%
		// would quietly undercharge exactly the model the operator singled out as expensive, and a
		// limiter that silently mis-charges is worse than one that stops. The log goes out first —
		// the trap message itself does not reach the operator's own filter log.
		hostlog.Log(hostlog.LevelError, "filter-tokenlimit-go: "+
			costPercentPrefix+model+"="+raw+" is not an integer")
		panic("filter-tokenlimit-go: unparseable " + costPercentPrefix + model)
	}
	return percent
}

func configString(key string) (string, bool) {
	opt := hostconfig.Get(key)
	if value := opt.Some(); value != nil {
		return *value, true
	}
	return "", false
}

// configUint parses a required-to-be-numeric config value, trapping on anything else. Fail closed
// at load, never silently at face value: a limiter configured by a typo is a limiter that is not
// limiting.
func configUint(key string, fallback, minimum uint64) uint64 {
	raw, ok := configString(key)
	if !ok {
		return fallback
	}
	value, err := strconv.ParseUint(raw, 10, 64)
	if err != nil {
		panic("filter-tokenlimit-go: " + key + " must be a non-negative integer, got " + raw)
	}
	if value < minimum {
		panic("filter-tokenlimit-go: " + key + " must be at least " + strconv.FormatUint(minimum, 10))
	}
	return value
}

// headerValue finds a header by ASCII-case-insensitive name and returns its raw bytes.
func headerValue(headers []types.Header, name string) ([]byte, bool) {
	for i := range headers {
		if eqFold(headers[i].Name, name) {
			return headers[i].Value.Slice(), true
		}
	}
	return nil, false
}

func eqFold(a, b string) bool {
	if len(a) != len(b) {
		return false
	}
	for i := 0; i < len(a); i++ {
		ca, cb := a[i], b[i]
		if ca >= 'A' && ca <= 'Z' {
			ca += 32
		}
		if cb >= 'A' && cb <= 'Z' {
			cb += 32
		}
		if ca != cb {
			return false
		}
	}
	return true
}

func header(name, value string) types.Header {
	return types.Header{Name: name, Value: cm.ToList([]byte(value))}
}

// jsonResponse builds a filter-authored error response. `content-length` is deliberately absent:
// the host owns it and recomputes it from the body (a guest-supplied one is rejected fail-closed,
// because a length that disagrees with the bytes is a response-desync primitive).
func jsonResponse(status uint16, body string) types.HTTPResponse {
	return types.HTTPResponse{
		Status:  status,
		Headers: cm.ToList([]types.Header{header("content-type", "application/json")}),
		Body:    cm.ToList([]byte(body)),
	}
}

func ceilDiv(value, divisor uint64) uint64 {
	if value == 0 {
		return 0
	}
	return (value-1)/divisor + 1
}

// satAdd / satMul saturate instead of wrapping. The inputs are attacker-chosen numbers from a JSON
// body, and a wrapped cost would be a SMALL cost — the one failure mode a limiter must not have.
func satAdd(a, b uint64) uint64 {
	if a+b < a {
		return ^uint64(0)
	}
	return a + b
}

func satMul(a, b uint64) uint64 {
	if a == 0 || b == 0 {
		return 0
	}
	if a > ^uint64(0)/b {
		return ^uint64(0)
	}
	return a * b
}

// The component is a reactor: the exports above are the only entry points, and nothing runs
// between them. No goroutines, no timers, no work in main.
func main() {}
