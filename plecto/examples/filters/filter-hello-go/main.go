package main

import (
	"fmt"
	"os"
	"strconv"

	hostclock "filter-hello-go/internal/plecto/filter/host-clock"
	hostcounter "filter-hello-go/internal/plecto/filter/host-counter"
	filterbodygo "filter-hello-go/internal/plecto/filter/filter-body-go"
	hostlog "filter-hello-go/internal/plecto/filter/host-log"
	hostratelimit "filter-hello-go/internal/plecto/filter/host-ratelimit"
	"filter-hello-go/internal/plecto/filter/types"

	"go.bytecodealliance.org/cm"
)

func hasHeader(headers []types.Header, name string) bool {
	for _, h := range headers {
		if eqFold(h.Name, name) {
			return true
		}
	}
	return false
}

func headerValue(headers []types.Header, name string) (string, bool) {
	for _, h := range headers {
		if eqFold(h.Name, name) {
			return h.Value, true
		}
	}
	return "", false
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

func oneHeader(name, value string) cm.List[types.Header] {
	return cm.ToList([]types.Header{{Name: name, Value: value}})
}

func init() {
	filterbodygo.Exports.Init = func() {
		hostcounter.Increment("init-calls", 1)
	}

	filterbodygo.Exports.OnRequest = func(req types.HTTPRequest) types.RequestDecision {
		hostlog.Log(hostlog.LevelInfo, "filter-hello-go: on-request")

		inits := hostcounter.Get("init-calls")
		hostlog.Log(hostlog.LevelInfo, "init-calls="+strconv.FormatInt(inits, 10))

		// Exercise the fat-guest minimal WASI grant (ADR 000063): TinyGo's runtime routes
		// fmt.Println/os.Stderr through wasi:cli stdout/stderr under the hood. This proves the
		// host's stdio bridge — not just the WIT host-log capability above — carries a real
		// TinyGo guest's output into host-log.
		fmt.Println("filter-hello-go: stdout probe")
		fmt.Fprintln(os.Stderr, "filter-hello-go: stderr probe")

		headers := req.Headers.Slice()

		if hasHeader(headers, "x-plecto-panic") {
			// Regression fixture for the host's trap-path log recovery (ADR 000063 F2/F5): write
			// a stderr line with NO trailing newline, then trap — proving the host still
			// recovers this unterminated line instead of losing it along with the discarded
			// instance.
			fmt.Fprint(os.Stderr, "filter-hello-go: panic probe (no trailing newline)")
			panic("filter-hello-go: intentional trap for host log-recovery test")
		}

		if hasHeader(headers, "x-plecto-addheader") {
			return types.RequestDecisionModified(types.RequestEdit{
				SetHeaders:    oneHeader("x-plecto-added", "1"),
				RemoveHeaders: cm.ToList([]string{}),
			})
		}

		if rl, ok := headerValue(headers, "x-plecto-ratelimit"); ok {
			key := rl
			if key == "" {
				key = "default"
			}
			outcome := hostratelimit.TryAcquire(key, 1)
			if !outcome.Allowed {
				return types.RequestDecisionShortCircuit(types.HTTPResponse{
					Status:  429,
					Headers: oneHeader("retry-after-ms", strconv.FormatUint(outcome.RetryAfterMs, 10)),
					Body:    cm.ToList([]byte("rate limited by filter-hello-go")),
				})
			}
		}

		if hasHeader(headers, "x-plecto-block") {
			return types.RequestDecisionShortCircuit(types.HTTPResponse{
				Status:  403,
				Headers: oneHeader("x-plecto", "blocked"),
				Body:    cm.ToList([]byte("blocked by filter-hello-go")),
			})
		}

		return types.RequestDecisionContinue()
	}

	filterbodygo.Exports.OnRequestBody = func(body cm.List[uint8]) types.RequestBodyDecision {
		hostlog.Log(hostlog.LevelInfo, "filter-hello-go: on-request-body")
		b := body.Slice()
		if containsFold(b, "deny-body") {
			return types.RequestBodyDecisionShortCircuit(types.HTTPResponse{
				Status:  403,
				Headers: oneHeader("x-plecto", "blocked-body"),
				Body:    cm.ToList([]byte("blocked body by filter-hello-go")),
			})
		}
		up := make([]byte, len(b))
		for i, c := range b {
			if c >= 'a' && c <= 'z' {
				c -= 32
			}
			up[i] = c
		}
		return types.RequestBodyDecisionContinue(cm.ToList(up))
	}

	filterbodygo.Exports.OnResponse = func(resp types.HTTPResponse) types.ResponseDecision {
		if hasHeader(resp.Headers.Slice(), "x-plecto-respedit") {
			return types.ResponseDecisionModified(types.ResponseEdit{
				SetStatus:     cm.None[uint16](),
				SetHeaders:    oneHeader("x-plecto-respadded", "1"),
				RemoveHeaders: cm.ToList([]string{}),
			})
		}
		return types.ResponseDecisionContinue()
	}

	// Referenced so the linker cannot dead-code-strip the host-clock import (unused otherwise
	// in this conformance subset) — mirrors the other language ports touching every capability.
	_ = hostclock.NowMs
}

func containsFold(haystack []byte, needle string) bool {
	n := len(needle)
	for i := 0; i+n <= len(haystack); i++ {
		if eqFoldBytes(haystack[i:i+n], needle) {
			return true
		}
	}
	return false
}

func eqFoldBytes(a []byte, b string) bool {
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

func main() {}
