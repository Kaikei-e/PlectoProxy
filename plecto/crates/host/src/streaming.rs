//! Experimental streaming body-filter runtime (direction_0003 gates 1+2), behind the
//! `streaming-body` feature (OFF by default). It drives an async `stream<u8>` body filter on
//! wasmtime's component-model-async: the host feeds the request body as a stream, the guest pulls it
//! lazily (no whole-body buffer on either side) and returns continue / short-circuit.
//!
//! This deliberately stays OUT of the shipped `plecto:filter@0.1.0` path. It carries the same
//! sandbox discipline as the sync host — epoch deadline + per-instance memory cap (ADR 000006) — and
//! lends a MINIMAL WASI slice: the `wasi:http/proxy` interfaces plus the rest of `wasi:cli` the std
//! guest's runtime imports (io / clocks / random / cli), each inert under an empty `WasiCtx`. It
//! deliberately adds NO filesystem and NO sockets, so those stay denied (security audit F-002; aligns
//! with the `wasi:http/middleware` convergence, ADR 000020). Server-side body-path wiring is a later
//! increment (it is coupled to the spawn_blocking removal, ADR 000021 §4).

use anyhow::Result;
use wasmtime::component::{Component, Linker, ResourceTable, StreamReader};
use wasmtime::{Config, Engine, Store, StoreLimits, StoreLimitsBuilder};
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

use crate::engine::EpochTicker;

mod bindings {
    wasmtime::component::bindgen!({
        path: "../../wit-streaming",
        world: "streaming-filter",
        exports: { default: async },
    });
}

use bindings::exports::plecto::filter_streaming::body_filter::RequestBodyDecision;

/// The host-visible outcome of a streaming body filter (the guest's decision, lowered to owned data).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamingDecision {
    /// Forward the body to the upstream unchanged.
    Continue,
    /// Stop the chain and synthesise this response (the filter inspected the body and rejected it).
    ShortCircuit { status: u16, body: Vec<u8> },
}

/// Sandbox bounds for one streaming run (ADR 000006).
pub struct StreamingLimits {
    /// Per-instance linear-memory cap (bytes). Bounds the guest's own buffering — the streaming
    /// property is that this stays flat in body size, unlike a buffered `list<u8>` filter.
    pub memory_cap: usize,
    /// Epoch deadline (ms): a CPU-spinning filter traps fail-closed. NOTE: epoch interruption bounds
    /// running wasm, not a cooperatively-idle guest that awaits forever without consuming the stream
    /// — bounding that needs a wall-clock timeout, which the (deferred) server wiring adds around the
    /// whole run. Per-chunk deadline semantics for very large bodies are likewise a follow-on.
    pub deadline_ms: u64,
}

struct Ctx {
    limits: StoreLimits,
    table: ResourceTable,
    wasi: WasiCtx,
}

impl WasiView for Ctx {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

fn build_engine() -> Result<Engine> {
    let mut config = Config::new();
    config.wasm_component_model(true);
    config.wasm_component_model_async(true);
    // Same metering as the sync host (ADR 000006): a background ticker advances the epoch so the
    // per-instance deadline can trap a runaway guest fail-closed.
    config.epoch_interruption(true);
    Ok(Engine::new(&config)?)
}

/// Run a streaming body filter `component` over `body`: feed the bytes as a `stream<u8>`, drive
/// `process-body` on component-model-async with a minimal WASI slice and the sandbox bounds in
/// `limits`, and return the decision. A trap / deadline / link failure is an `Err` the caller maps
/// fail-closed (never fail-open).
pub fn run_streaming_body(
    component: &[u8],
    body: Vec<u8>,
    limits: &StreamingLimits,
) -> Result<StreamingDecision> {
    let engine = build_engine()?;
    // The ticker lives for the whole run; dropping it stops and joins the thread.
    let _ticker = EpochTicker::spawn(vec![engine.clone()]);

    let component = Component::from_binary(&engine, component)?;

    // deny-by-default WASI (security audit F-002): the wasi:http/proxy interfaces (io / clocks /
    // random / stdio) PLUS the rest of the wasi:cli set the std guest's runtime imports (environment
    // / exit / terminal-*), every one inert under an empty `WasiCtx` (environment returns `[]`, exit
    // traps, the terminals are not TTYs). Filesystem and sockets are deliberately NOT added, so a
    // guest importing them fails to link — the capability boundary that matters. Aligns with the
    // `wasi:http/middleware` convergence (ADR 000020).
    let mut linker: Linker<Ctx> = Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_proxy_interfaces_async(&mut linker)?;
    crate::state::add_cli_runtime::<Ctx>(&mut linker)?;

    let store_limits = StoreLimitsBuilder::new()
        .memory_size(limits.memory_cap)
        .build();
    let mut store = Store::new(
        &engine,
        Ctx {
            limits: store_limits,
            table: ResourceTable::new(),
            wasi: WasiCtxBuilder::new().build(),
        },
    );
    store.limiter(|c| &mut c.limits);
    store.set_epoch_deadline(limits.deadline_ms);

    // Drive with pollster (the host stays no-tokio; ADR 000021). The guest never blocks on real I/O
    // — it only pulls the host-fed stream — so a no-reactor executor suffices.
    let instance = pollster::block_on(bindings::StreamingFilter::instantiate_async(
        &mut store, &component, &linker,
    ))?;

    let reader = StreamReader::new(&mut store, body)?;
    let decision = pollster::block_on(store.run_concurrent(async move |accessor| {
        instance
            .plecto_filter_streaming_body_filter()
            .call_process_body(accessor, reader)
            .await
    }))??;

    Ok(map_decision(decision))
}

/// Lower the guest's typed decision to the host-visible `StreamingDecision` (pure — no wasmtime
/// involvement, so it is unit-testable without an engine, component, or Store).
fn map_decision(decision: RequestBodyDecision) -> StreamingDecision {
    match decision {
        RequestBodyDecision::Continue => StreamingDecision::Continue,
        RequestBodyDecision::ShortCircuit(resp) => StreamingDecision::ShortCircuit {
            status: resp.status,
            body: resp.body,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bindings::plecto::filter_streaming::types::HttpResponse as GuestResponse;

    #[test]
    fn map_decision_lowers_guest_variants() {
        struct Case {
            name: &'static str,
            guest: RequestBodyDecision,
            want: StreamingDecision,
        }
        let cases = vec![
            Case {
                name: "continue",
                guest: RequestBodyDecision::Continue,
                want: StreamingDecision::Continue,
            },
            Case {
                name: "short-circuit",
                guest: RequestBodyDecision::ShortCircuit(GuestResponse {
                    status: 403,
                    headers: Vec::new(),
                    body: b"denied".to_vec(),
                }),
                want: StreamingDecision::ShortCircuit {
                    status: 403,
                    body: b"denied".to_vec(),
                },
            },
        ];
        for case in cases {
            let got = map_decision(case.guest);
            assert_eq!(got, case.want, "case: {}", case.name);
        }
    }
}
