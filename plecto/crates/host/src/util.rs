//! Tiny shared helpers with no other natural home — used from both `state.rs` and `pool.rs`.

pub(crate) fn wall_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
