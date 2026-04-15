//! Event-driven watchers. Each one opens a long-lived WebSocket and pushes
//! events into an unbounded tokio channel consumed by `main.rs`'s
//! `tokio::select!` loop. No polling — if a watcher can't subscribe, it
//! reconnects with backoff; it never falls back to a timer loop.

pub mod clob_ws;
pub mod mempool;
pub mod onchain;
