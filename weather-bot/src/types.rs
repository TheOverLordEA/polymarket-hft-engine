//! Shared types for the hot-path pipeline.
//!
//! This module defines the contract between the on-chain detector (Agent α),
//! the mint/mempool executor (Agent β), and the CLOB order executor (Agent γ).
//! Everything here is deliberately `Clone + Send + Sync` so it can ride the
//! `tokio::sync::mpsc::UnboundedSender` channels without fuss.

use alloy_primitives::{Address, B256, U256};
use tokio::sync::mpsc;

/// Classification of the CTF market we just detected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    /// NegRisk multi-bucket temperature market (the common case for weather).
    /// All buckets share one `marketId` and are linked via the NegRiskAdapter.
    NegRisk,
    /// Plain binary CTF market — e.g. a standalone "or below"/"or above"
    /// question that was prepared directly on the ConditionalTokens contract.
    BinaryCtf,
}

/// One temperature bucket inside a weather event. For a NegRisk event there
/// will be N of these, one per question index. For a plain binary event there
/// will be exactly one.
#[derive(Debug, Clone)]
pub struct BucketInfo {
    /// CTF `conditionId` = keccak256(oracle, questionId, outcomeSlotCount).
    pub condition_id: B256,
    /// Per-bucket `questionId`. For NegRisk, this is
    /// `bytes32(uint256(marketId) + questionIndex)`.
    pub question_id: B256,
    /// Index of this bucket inside the parent NegRisk market (0-based).
    /// Always 0 for a plain binary market.
    pub outcome_index: u32,
    /// Human-readable label from the market slug — e.g. "40°C",
    /// "37°C or below", "47°C or higher".
    pub bucket_label: String,
    /// ERC-1155 positionId for the YES outcome. This is what the executor
    /// mints, holds and sells.
    pub token_id_yes: U256,
    /// ERC-1155 positionId for the NO outcome.
    pub token_id_no: U256,
}

/// Emitted the instant we see a new weather market on chain and have derived
/// enough info to hand off to the executors. Ownership moves across the
/// unbounded mpsc channel — no back-pressure on the hot path.
#[derive(Debug, Clone)]
pub struct WeatherEvent {
    /// Polymarket event slug — e.g. "highest-temperature-in-lucknow-on-april-15-2026".
    pub event_slug: String,
    /// Lowercased city name parsed from the slug ("lucknow").
    pub city: String,
    /// ISO 8601 date parsed from the slug ("2026-04-15").
    pub resolution_date: String,
    pub kind: EventKind,
    /// For NegRisk: the `marketId` carried by `NegRiskAdapter.MarketPrepared`.
    /// `None` for pure binary events.
    pub neg_risk_market_id: Option<B256>,
    /// The NegRiskAdapter address (NegRisk) or the CTF oracle address (binary).
    /// Used to verify the event came from an allowlisted source.
    pub oracle: Address,
    /// All buckets discovered so far. For NegRisk we only emit the event once
    /// we believe we have the full set.
    pub buckets: Vec<BucketInfo>,
    /// Wall-clock nanos since epoch when the log was received from the WSS.
    /// Set exactly once, as close to the socket as possible, by
    /// `run_onchain_watcher`.
    pub detected_at_ns: u128,
}

/// Sender handed to `run_onchain_watcher` — upstream owns the receiver.
pub type WeatherEventSender = mpsc::UnboundedSender<WeatherEvent>;
/// Receiver consumed by the executor (Agent β entry point).
pub type WeatherEventReceiver = mpsc::UnboundedReceiver<WeatherEvent>;

/// Emitted by the mempool watcher (Agent β) when it has seen the first
/// `splitPosition`/`prepareMarket` mint transaction and can hand off to the
/// CLOB executor.
#[derive(Debug, Clone)]
pub struct MintReceipt {
    pub event_slug: String,
    pub tx_hash: B256,
    pub kind: EventKind,
    /// Nanos since epoch when the transaction entered the mempool
    /// (subscribed via `eth_subscribe("newPendingTransactions")`).
    pub seen_pending_at_ns: u128,
}

pub type MintReceiptSender = mpsc::UnboundedSender<MintReceipt>;
pub type MintReceiptReceiver = mpsc::UnboundedReceiver<MintReceipt>;

/// CLOB WebSocket told us a market is indexer-ready and we can POST orders.
#[derive(Debug, Clone)]
pub struct ClobMarketReady {
    pub condition_id: B256,
    pub asset_ids: Vec<U256>,
}

pub type ClobReadySender = mpsc::UnboundedSender<ClobMarketReady>;
pub type ClobReadyReceiver = mpsc::UnboundedReceiver<ClobMarketReady>;

/// Tiny helper — grabs nanos-since-epoch for the `detected_at_ns` field.
/// Inlined to keep the hot-path branchless.
#[inline(always)]
pub fn now_ns() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}
