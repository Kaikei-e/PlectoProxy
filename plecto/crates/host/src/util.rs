//! Tiny shared helpers with no other natural home — used from both `state.rs` and `pool.rs`.

/// Keep a reactor available throughout polling, including P3 host calls which directly create
/// Tokio timers. The ambient runtime's workers drive timers while pollster drives the Store.
#[cfg(any(feature = "fat-guest", feature = "streaming-body"))]
pub(crate) fn block_on_wasi<F: std::future::Future>(fut: F) -> F::Output {
    wasmtime_wasi::runtime::with_ambient_tokio_runtime(|| pollster::block_on(fut))
}

pub(crate) fn wall_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
