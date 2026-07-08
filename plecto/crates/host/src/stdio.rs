//! The fat-guest stdio bridge (ADR 000063): captures a WASI guest's stdout/stderr and queues it
//! as `host-log` lines — the same `Vec<LogLine>` → OTLP span-event path
//! (`observe::build_filter_span`) `host-log` itself feeds, so a trapped guest's own debug output
//! shows up in the same trace as the failing request (TTFVF, ADR 000065).
//!
//! Implements `wasi:io/streams.output-stream` DIRECTLY (not via `StdoutStream::async_stream`'s
//! default `AsyncWriteStream` adapter, which spawns a background worker task per stream to
//! bridge a possibly-blocking `AsyncWrite` sink) — this stream is an in-memory bounded buffer
//! that never blocks, so a direct `OutputStream` impl (the same shape wasmtime's own
//! `MemoryOutputPipe` and its official embedding examples use) avoids that overhead entirely.

use std::sync::{Arc, Mutex, PoisonError};

use bytes::Bytes;
use wasmtime_wasi::cli::{IsTerminal, StdoutStream};
use wasmtime_wasi::p2::{OutputStream, Pollable, StreamResult};

use crate::LogLevel;
use crate::state::{LogBudget, LogLine, sanitize_log_message};

/// Per-line truncation cap (ADR 000063 Decision 2): a longer line is cut on a char boundary.
const MAX_STDIO_LINE_BYTES: usize = 4 * 1024;
/// Per-request budget (ADR 000063 Decision 2), shared across stdout AND stderr combined — a
/// chatty guest cannot double its budget by writing to both streams.
const MAX_STDIO_BYTES_PER_REQUEST: usize = 64 * 1024;

/// Which stream a [`StdioStream`] handle bridges — fixes its `host-log` level (ADR 000063
/// Decision 2: stdout is debug-level noise, stderr is warn-level — often a panic/trap message).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StdioKind {
    Stdout,
    Stderr,
}

impl StdioKind {
    fn level(self) -> LogLevel {
        match self {
            StdioKind::Stdout => LogLevel::Debug,
            StdioKind::Stderr => LogLevel::Warn,
        }
    }
}

/// [`LogBudget`] for the stdio bridge: caps the per-request BYTE total (not line count) at
/// `remaining_bytes`, truncating any single line to `max_line_bytes` first. Unlike
/// `LineCountBudget`, once the budget is exhausted every further line is dropped silently after
/// one truncation marker (`warned`) — a verbose guest never traps, it just goes quiet.
struct ByteBudget {
    max_line_bytes: usize,
    remaining_bytes: usize,
    warned: bool,
}

impl ByteBudget {
    fn new(max_line_bytes: usize, total_bytes: usize) -> Self {
        Self {
            max_line_bytes,
            remaining_bytes: total_bytes,
            warned: false,
        }
    }

    fn reset(&mut self, total_bytes: usize) {
        self.remaining_bytes = total_bytes;
        self.warned = false;
    }
}

impl LogBudget for ByteBudget {
    fn admit(&mut self, level: LogLevel, message: String) -> Option<(LogLevel, String)> {
        if self.warned {
            return None;
        }
        let text = sanitize_log_message(message, self.max_line_bytes);
        // Charge at least 1 byte per line even for an empty line (CWE-770): otherwise a guest
        // could loop bare `\n` writes forever without ever exhausting the budget, growing
        // `pending` without bound.
        let cost = text.len().max(1);
        if cost > self.remaining_bytes {
            self.warned = true;
            return Some((
                LogLevel::Warn,
                "… stdio truncated (per-request byte budget reached)".to_string(),
            ));
        }
        self.remaining_bytes -= cost;
        Some((level, text))
    }
}

struct StdioBridgeInner {
    /// Bytes buffered for the current, not-yet-`\n`-terminated stdout line.
    stdout_partial: Vec<u8>,
    /// Same as `stdout_partial`, for stderr — kept separate so interleaved stdout/stderr writes
    /// never merge into one garbled line.
    stderr_partial: Vec<u8>,
    /// Completed lines waiting for `HostState::take_logs` to drain them.
    pending: Vec<LogLine>,
    budget: ByteBudget,
}

impl StdioBridgeInner {
    /// Bound an unterminated partial line independently of a `\n` ever arriving (CWE-770): a
    /// guest that never emits one must not grow this buffer past the per-line cap.
    fn admit_line(&mut self, kind: StdioKind, raw: Vec<u8>) {
        let text = String::from_utf8_lossy(&raw).into_owned();
        if let Some((level, message)) = self.budget.admit(kind.level(), text) {
            self.pending.push(LogLine { level, message });
        }
    }

    fn partial(&mut self, kind: StdioKind) -> &mut Vec<u8> {
        match kind {
            StdioKind::Stdout => &mut self.stdout_partial,
            StdioKind::Stderr => &mut self.stderr_partial,
        }
    }

    fn write(&mut self, kind: StdioKind, bytes: &[u8]) {
        // Combine the buffered partial line with the new bytes into one owned buffer, then split
        // on newlines with a single forward scan and one final copy-back for the remainder.
        // Repeatedly `Vec::drain(..=pos)`-ing off the front (the prior approach) shifts every
        // trailing byte on EACH newline found, making one `write()` call with k embedded
        // newlines O(n*k) — an untrusted guest can hand a single `output-stream.write` a
        // newline-only payload up to the linear-memory cap (CWE-770-adjacent: unbounded host CPU
        // from one guest call, which epoch interruption cannot preempt since this runs inside a
        // host function). This rewrite is O(n) regardless of newline count.
        let mut buf = std::mem::take(self.partial(kind));
        buf.extend_from_slice(bytes);

        let mut start = 0;
        while let Some(rel_pos) = buf
            .get(start..)
            .unwrap_or_default()
            .iter()
            .position(|&b| b == b'\n')
        {
            let pos = start + rel_pos;
            self.admit_line(kind, buf.get(start..pos).unwrap_or_default().to_vec());
            start = pos + 1;
        }
        let remainder = buf.get(start..).unwrap_or_default();
        if remainder.len() > MAX_STDIO_LINE_BYTES {
            // No '\n' yet, but the still-unterminated line has already grown past what a single
            // stored line could ever hold — flush it now as its own (likely truncated) line
            // instead of buffering an unbounded amount waiting for a newline that may never come.
            let line = remainder.to_vec();
            self.admit_line(kind, line);
        } else {
            *self.partial(kind) = remainder.to_vec();
        }
    }

    /// Flush an unterminated partial line for `kind`, if any, as a line of its own. Used by
    /// `drain_final` so a guest's final, newline-less output (e.g. a panic message written just
    /// before it traps) is not silently lost once the instance is discarded. NOT called from the
    /// plain `drain()` below — a partial line may legitimately still be completed by a later
    /// write within the same still-live instance (e.g. across separate hook calls), so routine
    /// draining must not flush it early.
    fn flush_partial(&mut self, kind: StdioKind) {
        let line = std::mem::take(self.partial(kind));
        if !line.is_empty() {
            self.admit_line(kind, line);
        }
    }

    fn drain(&mut self) -> Vec<LogLine> {
        std::mem::take(&mut self.pending)
    }

    /// Like `drain`, but also flushes any still-unterminated partial line first. For use when the
    /// guest instance is about to be discarded (a trap) and a buffered partial line would
    /// otherwise never be completed nor recovered.
    fn drain_final(&mut self) -> Vec<LogLine> {
        self.flush_partial(StdioKind::Stdout);
        self.flush_partial(StdioKind::Stderr);
        self.drain()
    }

    fn begin_request(&mut self) {
        self.stdout_partial.clear();
        self.stderr_partial.clear();
        self.pending.clear();
        self.budget.reset(MAX_STDIO_BYTES_PER_REQUEST);
    }
}

/// A WASI guest's stdout/stderr, bridged into `host-log` lines (ADR 000063). Cloneable — two
/// [`StdioStream`] handles (stdout, stderr) share one `Arc<Mutex<_>>` so the byte budget is
/// combined across both streams, and `HostState` holds its own clone to drain/reset it.
#[derive(Clone)]
pub(crate) struct StdioBridge {
    inner: Arc<Mutex<StdioBridgeInner>>,
}

impl StdioBridge {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(StdioBridgeInner {
                stdout_partial: Vec::new(),
                stderr_partial: Vec::new(),
                pending: Vec::new(),
                budget: ByteBudget::new(MAX_STDIO_LINE_BYTES, MAX_STDIO_BYTES_PER_REQUEST),
            })),
        }
    }

    /// A `StdoutStream`-implementing handle bound to `kind`, for `WasiCtxBuilder::stdout`/`.stderr`.
    pub(crate) fn stream(&self, kind: StdioKind) -> StdioStream {
        StdioStream {
            bridge: self.clone(),
            kind,
        }
    }

    /// Drain every line queued so far. Called once per guest call, from `HostState::take_logs`.
    pub(crate) fn drain(&self) -> Vec<LogLine> {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain()
    }

    /// Like `drain`, but also flushes any still-unterminated partial line first (ADR 000063):
    /// called from `HostState::take_logs_after_trap` on the trap path, where the instance is
    /// about to be discarded.
    pub(crate) fn drain_final(&self) -> Vec<LogLine> {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain_final()
    }

    /// Reset for the next request (pooled/trusted instances reuse the same bridge).
    pub(crate) fn begin_request(&self) {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .begin_request();
    }
}

/// One direction (stdout or stderr) of a [`StdioBridge`] — the type actually handed to
/// `WasiCtxBuilder::stdout`/`.stderr`.
#[derive(Clone)]
pub(crate) struct StdioStream {
    bridge: StdioBridge,
    kind: StdioKind,
}

impl IsTerminal for StdioStream {
    fn is_terminal(&self) -> bool {
        false
    }
}

impl StdoutStream for StdioStream {
    fn async_stream(&self) -> Box<dyn tokio::io::AsyncWrite + Send + Sync> {
        // Never reached: `p2_stream` below is overridden and is the only path the p2 WASI host
        // uses. A real no-op sink (not a panic) keeps this safe even if that assumption changes.
        Box::new(tokio::io::sink())
    }

    fn p2_stream(&self) -> Box<dyn OutputStream> {
        Box::new(self.clone())
    }
}

#[wasmtime_wasi::async_trait]
impl OutputStream for StdioStream {
    fn write(&mut self, bytes: Bytes) -> StreamResult<()> {
        self.bridge
            .inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .write(self.kind, &bytes);
        Ok(())
    }

    fn flush(&mut self) -> StreamResult<()> {
        Ok(())
    }

    fn check_write(&mut self) -> StreamResult<usize> {
        // Always ready, unbounded permit: this sink never blocks and never signals backpressure
        // (the ADR 000063 truncate-and-warn-once policy lives in `write`/`ByteBudget`, not here)
        // — the same choice wasmtime's own official custom-`OutputStream` example makes.
        Ok(usize::MAX)
    }
}

#[wasmtime_wasi::async_trait]
impl Pollable for StdioStream {
    async fn ready(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bridge() -> StdioBridge {
        StdioBridge::new()
    }

    #[test]
    fn splits_on_newline_and_tags_stream_levels() {
        let b = bridge();
        b.inner
            .lock()
            .unwrap()
            .write(StdioKind::Stdout, b"first\nsecond\n");
        b.inner.lock().unwrap().write(StdioKind::Stderr, b"oops\n");
        let lines = b.drain();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].message, "first");
        assert_eq!(lines[0].level, LogLevel::Debug);
        assert_eq!(lines[1].message, "second");
        assert_eq!(lines[2].message, "oops");
        assert_eq!(lines[2].level, LogLevel::Warn);
    }

    #[test]
    fn a_trailing_partial_line_stays_buffered_until_drained_by_the_next_write() {
        let b = bridge();
        b.inner
            .lock()
            .unwrap()
            .write(StdioKind::Stdout, b"no newline yet");
        assert!(
            b.drain().is_empty(),
            "an unterminated line is not queued until it completes or overflows"
        );
        b.inner.lock().unwrap().write(StdioKind::Stdout, b" done\n");
        let lines = b.drain();
        assert_eq!(lines[0].message, "no newline yet done");
    }

    #[test]
    fn an_unterminated_line_past_the_per_line_cap_flushes_without_waiting_for_a_newline() {
        let b = bridge();
        // No '\n' anywhere — a naive impl would buffer this forever (CWE-770).
        b.inner.lock().unwrap().write(
            StdioKind::Stdout,
            "x".repeat(MAX_STDIO_LINE_BYTES + 100).as_bytes(),
        );
        let lines = b.drain();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].message.len() <= MAX_STDIO_LINE_BYTES);
    }

    #[test]
    fn the_per_request_byte_budget_is_combined_across_stdout_and_stderr() {
        let b = bridge();
        // Each line stays under the 4 KiB per-line cap, so the total-budget path (not the
        // per-line truncation path) is what's under test: enough ~1 KiB lines on stdout to
        // nearly fill the 64 KiB request budget, then stderr lines push it over.
        let line = "a".repeat(1024);
        for _ in 0..(MAX_STDIO_BYTES_PER_REQUEST / line.len()) {
            b.inner
                .lock()
                .unwrap()
                .write(StdioKind::Stdout, format!("{line}\n").as_bytes());
        }
        b.inner
            .lock()
            .unwrap()
            .write(StdioKind::Stderr, format!("{line}\n").as_bytes());
        // A further line, after the combined budget is exhausted, is dropped — not just truncated.
        b.inner
            .lock()
            .unwrap()
            .write(StdioKind::Stdout, b"after budget\n");
        let lines = b.drain();
        let truncated = lines
            .iter()
            .filter(|l| l.message.contains("truncated"))
            .count();
        assert_eq!(
            truncated, 1,
            "exactly one truncation marker, not one per stream"
        );
        assert!(
            lines.iter().all(|l| l.message != "after budget"),
            "a line arriving after the budget warns is dropped silently"
        );
    }

    #[test]
    fn begin_request_resets_budget_and_partial_buffers() {
        let b = bridge();
        b.inner.lock().unwrap().write(
            StdioKind::Stdout,
            "x".repeat(MAX_STDIO_BYTES_PER_REQUEST).as_bytes(),
        );
        b.drain();
        b.begin_request();
        b.inner.lock().unwrap().write(StdioKind::Stdout, b"fresh\n");
        let lines = b.drain();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].message, "fresh");
    }
}
