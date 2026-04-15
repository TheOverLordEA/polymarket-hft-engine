//! Polymarket CLOB WebSocket watcher.
//!
//! Subscribes to `wss://ws-subscriptions-clob.polymarket.com/ws/market` with
//! the undocumented `custom_feature_enabled: true` flag that enables the
//! globally-broadcast `new_market` and `market_resolved` messages. When we
//! see `new_market` for a condition we've already pre-signed orders for, we
//! emit a `ClobMarketReady` so the dump executor knows the indexer has
//! ingested the market and `POST /order` will succeed.
//!
//! The subscription requires a non-empty `assets_ids` array — we piggyback on
//! a stable, long-lived token (`config.clob_anchor_asset_id`).
//!
//! Source: confirmed in `Polymarket/rs-clob-client/src/clob/ws/subscription.rs`
//! comment about `custom_feature_enabled`.

use crate::types::ClobReadySender;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};

#[derive(Serialize)]
struct SubscribePayload<'a> {
    r#type: &'a str,
    operation: &'a str,
    assets_ids: Vec<&'a str>,
    initial_dump: bool,
    custom_feature_enabled: bool,
}

#[derive(Deserialize, Debug)]
#[serde(tag = "event_type")]
#[allow(clippy::large_enum_variant)]
enum ClobMessage {
    #[serde(rename = "new_market")]
    NewMarket {
        #[serde(default)]
        condition_id: String,
        #[serde(default)]
        asset_ids: Vec<String>,
    },
    #[serde(rename = "market_resolved")]
    MarketResolved {
        #[serde(default)]
        condition_id: String,
    },
    #[serde(other)]
    Other,
}

/// Run the CLOB market WS watcher forever. Reconnects with backoff.
pub async fn run_clob_market_watcher(
    ws_url: String,
    anchor_asset_id: String,
    _ready_tx: ClobReadySender,
) -> anyhow::Result<()> {
    if anchor_asset_id.is_empty() {
        tracing::warn!(
            "[clob_ws] no anchor_asset_id configured — skipping CLOB WS (new_market events disabled)"
        );
        // Still park forever rather than return — caller expects a long-running future.
        std::future::pending::<()>().await;
        return Ok(());
    }

    let mut backoff_ms: u64 = 500;
    loop {
        match run_once(&ws_url, &anchor_asset_id).await {
            Ok(_) => {
                tracing::warn!("[clob_ws] stream ended cleanly, reconnecting");
                backoff_ms = 500;
            }
            Err(e) => {
                tracing::warn!("[clob_ws] error: {} — backoff {}ms", e, backoff_ms);
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms * 2).min(30_000);
            }
        }
    }
}

async fn run_once(ws_url: &str, anchor: &str) -> anyhow::Result<()> {
    tracing::info!("[clob_ws] connecting to {}", ws_url);
    let (mut ws, _) = connect_async(ws_url).await?;

    let payload = SubscribePayload {
        r#type: "market",
        operation: "subscribe",
        assets_ids: vec![anchor],
        initial_dump: true,
        custom_feature_enabled: true,
    };
    ws.send(Message::Text(serde_json::to_string(&payload)?.into()))
        .await?;
    tracing::info!("[clob_ws] subscribed (anchor={})", anchor);

    while let Some(msg) = ws.next().await {
        let msg = msg?;
        let txt = match msg {
            Message::Text(t) => t,
            Message::Close(_) => break,
            _ => continue,
        };
        // The message may be a single object or an array of deltas
        if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(txt.as_ref()) {
            for item in arr {
                handle_item(item);
            }
        } else if let Ok(v) = serde_json::from_str::<serde_json::Value>(txt.as_ref()) {
            handle_item(v);
        }
    }
    Ok(())
}

fn handle_item(v: serde_json::Value) {
    if let Ok(msg) = serde_json::from_value::<ClobMessage>(v) {
        match msg {
            ClobMessage::NewMarket {
                condition_id,
                asset_ids,
            } => {
                tracing::info!(
                    "[clob_ws] new_market cid={} tokens={}",
                    condition_id,
                    asset_ids.len()
                );
                // TODO: forward to ready_tx once condition_id/asset_ids parsing
                // to B256/U256 is wired in.
            }
            ClobMessage::MarketResolved { condition_id } => {
                tracing::info!("[clob_ws] market_resolved cid={}", condition_id);
            }
            ClobMessage::Other => {}
        }
    }
}
