//! On-chain mint executor.
//!
//! Builds and broadcasts `NegRiskAdapter.splitPosition(conditionId, amount)`
//! transactions — one per bucket in a weather event. In simulation mode this
//! is a no-op log; in live mode it serializes the tx and POSTs
//! `eth_sendRawTransaction` to every configured RPC endpoint in parallel so
//! at least one propagates to a validator quickly.
//!
//! # Sim vs live
//!
//! The live path requires correct Polygon EIP-1559 tx construction — nonce
//! management, gas estimation, RLP encoding, EIP-155 signing — which is
//! enough code to justify its own PR. For now:
//!
//!   - **Sim mode** (default): logs what would be sent and returns a fake tx
//!     hash so the downstream pipeline (main loop, presigner flush) can
//!     exercise end-to-end. This is what the user opted into for the first
//!     run.
//!
//!   - **Live mode**: returns an error with a clear TODO message. Wiring
//!     this up is a bounded follow-up task: alloy-consensus `TxEip1559` +
//!     `alloy-signer::Signer::sign_hash` + raw POST via the executor's
//!     existing `reqwest::Client`.
//!
//! The public API is stable across both modes so main.rs doesn't care.

use crate::config::Config;
use crate::ctf_math::neg_risk_bucket_condition_id;
use crate::types::{now_ns, EventKind, MintReceipt, WeatherEvent};
use alloy_primitives::B256;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

pub struct MintExecutor {
    config: Arc<Config>,
    /// Fake-hash counter for sim mode, so each sim mint gets a unique "hash"
    /// and main.rs can distinguish them.
    sim_counter: AtomicU64,
}

impl MintExecutor {
    pub fn new(config: Arc<Config>) -> Self {
        Self {
            config,
            sim_counter: AtomicU64::new(1),
        }
    }

    /// Mint `amount_usdc` worth of complete sets on every bucket of `event`.
    /// Returns a `MintReceipt` per bucket (sim) or the first broadcast
    /// `MintReceipt` with the real tx hash (live).
    ///
    /// In live mode this returns an error because the tx-building path is
    /// stubbed — see the module doc for the unblock.
    pub async fn mint_event(
        &self,
        event: &WeatherEvent,
        amount_usdc: f64,
    ) -> anyhow::Result<Vec<MintReceipt>> {
        if self.config.simulation {
            self.mint_event_sim(event, amount_usdc).await
        } else {
            self.mint_event_live(event, amount_usdc).await
        }
    }

    async fn mint_event_sim(
        &self,
        event: &WeatherEvent,
        amount_usdc: f64,
    ) -> anyhow::Result<Vec<MintReceipt>> {
        let mut out = Vec::with_capacity(event.buckets.len());
        for bucket in &event.buckets {
            let fake_nonce = self.sim_counter.fetch_add(1, Ordering::Relaxed);
            let mut hash_bytes = [0u8; 32];
            hash_bytes[24..32].copy_from_slice(&fake_nonce.to_be_bytes());
            let tx_hash = B256::from(hash_bytes);

            tracing::info!(
                "[SIM-MINT] splitPosition cid=0x{} amount=${:.2} (bucket={}, sim_hash=0x{})",
                hex::encode(&bucket.condition_id.as_slice()[..8]),
                amount_usdc,
                bucket.bucket_label,
                hex::encode(&tx_hash.as_slice()[..8])
            );

            out.push(MintReceipt {
                event_slug: event.event_slug.clone(),
                tx_hash,
                kind: event.kind,
                seen_pending_at_ns: now_ns(),
            });
        }
        Ok(out)
    }

    async fn mint_event_live(
        &self,
        _event: &WeatherEvent,
        _amount_usdc: f64,
    ) -> anyhow::Result<Vec<MintReceipt>> {
        anyhow::bail!(
            "[mint] live on-chain mint path is not yet wired — set SIMULATION=true or \
             finish the alloy-consensus TxEip1559 builder in mint_executor.rs"
        );
    }

    /// Precompute all per-bucket conditionIds for an event. Cheap — no I/O.
    /// Used by the main loop to size the presigner cache before calling mint.
    pub fn expected_condition_ids(&self, event: &WeatherEvent) -> Vec<B256> {
        if let Some(market_id) = event.neg_risk_market_id {
            event
                .buckets
                .iter()
                .map(|b| neg_risk_bucket_condition_id(market_id, b.outcome_index as u8))
                .collect()
        } else {
            event.buckets.iter().map(|b| b.condition_id).collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{BucketInfo, EventKind};
    use alloy_primitives::{B256, U256};

    fn sample_config() -> Arc<Config> {
        Arc::new(Config {
            simulation: true,
            ..Config::default()
        })
    }

    fn sample_event() -> WeatherEvent {
        WeatherEvent {
            event_slug: "highest-temperature-in-lucknow-on-april-15-2026".to_string(),
            city: "lucknow".to_string(),
            resolution_date: "2026-04-15".to_string(),
            kind: EventKind::NegRisk,
            neg_risk_market_id: Some(B256::repeat_byte(0xaa)),
            oracle: alloy_primitives::Address::ZERO,
            buckets: vec![BucketInfo {
                condition_id: B256::repeat_byte(0x01),
                question_id: B256::repeat_byte(0x02),
                outcome_index: 3,
                bucket_label: "40°C".to_string(),
                token_id_yes: U256::from(1u64),
                token_id_no: U256::from(2u64),
            }],
            detected_at_ns: 0,
        }
    }

    #[tokio::test]
    async fn sim_mint_returns_receipt_per_bucket() {
        let cfg = sample_config();
        let me = MintExecutor::new(cfg);
        let event = sample_event();
        let receipts = me.mint_event(&event, 10.0).await.unwrap();
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].event_slug, event.event_slug);
        assert_eq!(receipts[0].kind, EventKind::NegRisk);
    }

    #[tokio::test]
    async fn sim_mint_returns_unique_hashes() {
        let cfg = sample_config();
        let me = MintExecutor::new(cfg);
        let event = sample_event();
        let r1 = me.mint_event(&event, 10.0).await.unwrap();
        let r2 = me.mint_event(&event, 10.0).await.unwrap();
        assert_ne!(r1[0].tx_hash, r2[0].tx_hash);
    }
}
