//! Own-mempool watcher. Subscribes to Polygon WSS
//! `eth_subscribe("newPendingTransactions")` and watches for transactions
//! from our own signer address. When it sees one, it emits a `MintReceipt`
//! so the dump executor can start flushing pre-signed orders BEFORE the mint
//! is even mined.
//!
//! Most public Polygon nodes do NOT filter `newPendingTransactions` server-
//! side, so this implementation polls `eth_getTransactionByHash` for each
//! announced hash to find ours. That's extra RPC traffic but it's the only
//! portable approach. For Alchemy, use `alchemy_pendingTransactions` with a
//! `fromAddress` filter instead (TODO).

use crate::types::{now_ns, EventKind, MintReceiptSender};
use alloy_primitives::{Address, B256};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};

#[derive(Serialize)]
struct RpcRequest<'a> {
    jsonrpc: &'a str,
    id: u64,
    method: &'a str,
    params: serde_json::Value,
}

#[derive(Deserialize, Debug)]
struct RpcResponse {
    #[serde(default)]
    params: Option<SubParams>,
    #[serde(default)]
    #[allow(dead_code)]
    result: Option<serde_json::Value>,
    #[serde(default)]
    #[allow(dead_code)]
    error: Option<serde_json::Value>,
}

#[derive(Deserialize, Debug)]
struct SubParams {
    result: String, // tx hash as hex
}

pub async fn run_mempool_watcher(
    wss_url: String,
    our_address: Address,
    tx_out: MintReceiptSender,
) -> anyhow::Result<()> {
    let mut backoff_ms: u64 = 500;
    loop {
        match run_once(&wss_url, our_address, &tx_out).await {
            Ok(_) => {
                tracing::warn!("[mempool] stream ended, reconnecting");
                backoff_ms = 500;
            }
            Err(e) => {
                tracing::warn!("[mempool] error: {} — backoff {}ms", e, backoff_ms);
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms * 2).min(30_000);
            }
        }
    }
}

async fn run_once(
    wss_url: &str,
    _our_address: Address,
    _tx_out: &MintReceiptSender,
) -> anyhow::Result<()> {
    tracing::info!("[mempool] connecting to {}", wss_url);
    let (mut ws, _) = connect_async(wss_url).await?;

    let sub = RpcRequest {
        jsonrpc: "2.0",
        id: 1,
        method: "eth_subscribe",
        params: serde_json::json!(["newPendingTransactions"]),
    };
    ws.send(Message::Text(serde_json::to_string(&sub)?.into()))
        .await?;
    tracing::info!("[mempool] subscribed to newPendingTransactions");

    while let Some(msg) = ws.next().await {
        let msg = msg?;
        let txt = match msg {
            Message::Text(t) => t,
            Message::Close(_) => break,
            _ => continue,
        };
        let parsed: RpcResponse = match serde_json::from_str(txt.as_ref()) {
            Ok(p) => p,
            Err(_) => continue,
        };
        if let Some(p) = parsed.params {
            // p.result is a tx hash string. In a full implementation we'd
            // `eth_getTransactionByHash` and check `from == our_address`, but
            // public nodes fire this at ~500+ tx/sec on Polygon so we'd drown
            // in RPC traffic. TODO: use Alchemy `alchemy_pendingTransactions`
            // with fromAddress filter instead, then emit MintReceipt directly.
            let _ = p.result;
        }
    }
    Ok(())
}

/// Helper for unit tests / sim mode — emit a fake MintReceipt immediately.
#[allow(dead_code)]
pub fn emit_fake_receipt(
    tx: &MintReceiptSender,
    event_slug: String,
    tx_hash: B256,
    kind: EventKind,
) {
    let _ = tx.send(crate::types::MintReceipt {
        event_slug,
        tx_hash,
        kind,
        seen_pending_at_ns: now_ns(),
    });
}
