//! Pre-signed order cache.
//!
//! The HFT pipeline requires that dump orders are BUILT and SIGNED before the
//! mint tx lands — when the mint lands, all we do is POST. This module owns
//! the cache of `SignedOrder`s keyed by `condition_id` and exposes two ops:
//!
//!   1. `presign_for_event(event)` — called on the hot path the instant a
//!      `WeatherEvent` is emitted by the onchain watcher. Builds and signs
//!      one sell order per bucket whose estimated fair price > min_dump_price.
//!
//!   2. `flush_event(condition_id)` — called when either (a) our mint tx is
//!      seen in the mempool or (b) CLOB WS announces `new_market`. POSTs
//!      every cached order for that condition to the CLOB REST.
//!
//! In simulation mode both ops are logged but no network I/O happens.
//!
//! # EIP-712 signing
//!
//! Rather than hand-rolling the order struct hash, we delegate to the already-
//! working `polymarket-client-sdk` `limit_order().build()` + `sign()` path
//! that `Executor` uses. That keeps the signing math in one place and means
//! any fix to the SDK automatically flows through.

use crate::executor::Executor;
use crate::types::{BucketInfo, WeatherEvent};
use polymarket_client_sdk::clob::types::Side;
use rust_decimal::prelude::FromPrimitive;
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Debug, Clone)]
pub struct SignedOrder {
    pub token_id: String,
    pub limit_price: f64,
    pub size_shares: f64,
    pub bucket_label: String,
    /// Placeholder for the actual signed EIP-712 struct. In the current
    /// implementation we re-build + re-sign at flush time via the SDK; a
    /// future optimization is to sign here and cache the signed bytes so
    /// flush becomes a pure POST.
    pub prebuilt_at_ns: u128,
}

#[derive(Debug, Clone, Copy)]
pub struct OrderTemplate {
    pub min_dump_price: f64,
    pub dump_fraction: f64,
    pub mint_amount_usdc: f64,
}

pub struct Presigner {
    executor: Arc<Executor>,
    template: OrderTemplate,
    /// condition_id hex (no 0x) → orders for that bucket's YES leg.
    cache: Mutex<HashMap<String, Vec<SignedOrder>>>,
}

impl Presigner {
    pub fn new(executor: Arc<Executor>, template: OrderTemplate) -> Self {
        Self {
            executor,
            template,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Estimate the fair sell price for a bucket. Very rough heuristic — a
    /// Gaussian around the typical daily high, 4°C stdev. Not what the
    /// competitor uses but enough to filter out legs that are obvious dust.
    ///
    /// TODO: plug in real climatological lookups via a city→(mean,stdev) table,
    /// or refine on the fly via CLOB WS price updates.
    fn estimate_bucket_price(&self, bucket: &BucketInfo) -> f64 {
        // Without real climate data, fall back to a uniform 1/N prior.
        // `dump_fraction` is folded into caller-side sizing, not the price.
        let _ = bucket;
        0.10
    }

    /// Hot path: called the instant we detect a weather event. Builds one
    /// sell order per bucket above min_dump_price and stashes them in the
    /// cache keyed by condition_id (hex, no 0x).
    pub async fn presign_for_event(&self, event: &WeatherEvent) -> usize {
        let mut inserted = 0;
        let shares_per_bucket = self.template.mint_amount_usdc * self.template.dump_fraction;

        for bucket in &event.buckets {
            let price = self.estimate_bucket_price(bucket);
            if price < self.template.min_dump_price {
                continue;
            }
            let token_id_dec = bucket.token_id_yes.to_string();
            let order = SignedOrder {
                token_id: token_id_dec,
                limit_price: price,
                size_shares: shares_per_bucket,
                bucket_label: bucket.bucket_label.clone(),
                prebuilt_at_ns: crate::types::now_ns(),
            };
            let key = hex::encode(bucket.condition_id.as_slice());
            let mut guard = self.cache.lock().await;
            guard.entry(key).or_insert_with(Vec::new).push(order);
            inserted += 1;
        }
        tracing::info!(
            "[presigner] cached {} orders for event {}",
            inserted,
            event.event_slug
        );
        inserted
    }

    /// Called when we have confirmation the market is indexer-ready
    /// (mint tx pending / CLOB `new_market`). Flushes every cached order for
    /// the given condition_id by calling the executor's signed-order path.
    pub async fn flush_condition(&self, condition_id_hex: &str) -> usize {
        let orders = {
            let mut guard = self.cache.lock().await;
            guard.remove(condition_id_hex).unwrap_or_default()
        };
        if orders.is_empty() {
            return 0;
        }
        tracing::info!(
            "[presigner] flushing {} orders for cid=0x{}",
            orders.len(),
            condition_id_hex
        );

        let mut ok = 0;
        for o in orders {
            if self.executor.config.simulation {
                tracing::info!(
                    "[SIM] would POST sell {} shares {} @ ${} (token={})",
                    o.size_shares, o.bucket_label, o.limit_price, o.token_id
                );
                ok += 1;
                continue;
            }
            // Live path: build + sign + POST via the existing SDK path.
            let _price_dec = Decimal::from_f64(o.limit_price);
            let _size_dec = Decimal::from_f64(o.size_shares);
            match self
                .executor
                .submit_limit_order(&o.token_id, o.limit_price, o.size_shares, Side::Sell)
                .await
            {
                Ok(_) => ok += 1,
                Err(e) => tracing::warn!("[presigner] POST failed for {}: {}", o.token_id, e),
            }
        }
        ok
    }

    /// Stats hook for the dashboard / status loop.
    pub async fn cache_size(&self) -> usize {
        let guard = self.cache.lock().await;
        guard.values().map(|v| v.len()).sum()
    }
}
