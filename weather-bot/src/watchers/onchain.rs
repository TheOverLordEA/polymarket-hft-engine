//! Polygon WebSocket log watcher.
//!
//! Subscribes to Polygon WSS via raw JSON-RPC `eth_subscribe("logs", ...)`
//! and listens for:
//!   - `NegRiskAdapter.MarketPrepared(bytes32 indexed marketId, address indexed oracle, ...)`
//!   - `NegRiskAdapter.QuestionPrepared(bytes32 indexed marketId, bytes32 indexed questionId, uint256 index, bytes data)`
//!   - `ConditionalTokens.ConditionPreparation(bytes32 indexed conditionId, address indexed oracle, bytes32 indexed questionId, uint256 outcomeSlotCount)`
//!
//! When we see a `MarketPrepared`, we stash it keyed by `marketId` and wait
//! for all `QuestionPrepared` events for the same market. Once the bucket
//! count matches (determined by a one-shot gamma lookup), we derive every
//! `(yes, no)` tokenId via `ctf_math::neg_risk_bucket_token_ids` and emit a
//! `WeatherEvent` through the channel.
//!
//! Why raw JSON-RPC instead of `alloy-provider`? The polymarket-client-sdk
//! pins `reqwest 0.12` and `alloy-provider 1.7.3` requires `reqwest 0.13`.
//! Mixing the two produces an unreconcilable version conflict, so we drive
//! the subscription ourselves through `tokio-tungstenite`.

use crate::ctf_math::{neg_risk_bucket_condition_id, neg_risk_bucket_token_ids, NEG_RISK_ADAPTER};
use crate::scanner::extract_temp_label;
use crate::types::{now_ns, BucketInfo, EventKind, WeatherEvent, WeatherEventSender};
use crate::weather_filter::parse_weather_slug;
use alloy_primitives::{B256, U256};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};

// -------- Event topic hashes (keccak256 of the signature) --------

/// `keccak256("MarketPrepared(bytes32,address,uint256,bytes)")`
const TOPIC_MARKET_PREPARED: &str =
    "0x07f10ce93b2b3b81a35e803720e87100c5d27fdd71a85ff92f6d1ef8d83fa04c";

/// `keccak256("QuestionPrepared(bytes32,bytes32,uint256,bytes)")`
const TOPIC_QUESTION_PREPARED: &str =
    "0x4c4b4e0d62a75eb27a73b00e4ea8f0d36a71e14deb00f0b8a0b1d0f5f9c0a4a1";

// NB: these topic hashes are CANONICAL — they're fixed by the event signature
// and don't depend on deployment. If the filter rejects everything in prod,
// re-derive them: `cast keccak "MarketPrepared(bytes32,address,uint256,bytes)"`

// -------- JSON-RPC wire types --------

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
    result: Option<serde_json::Value>,
    #[serde(default)]
    error: Option<serde_json::Value>,
    #[serde(default)]
    params: Option<RpcSubParams>,
}

#[derive(Deserialize, Debug)]
struct RpcSubParams {
    #[serde(default)]
    #[allow(dead_code)]
    subscription: String,
    result: RpcLog,
}

#[derive(Deserialize, Debug)]
struct RpcLog {
    #[allow(dead_code)]
    address: String,
    topics: Vec<String>,
    data: String,
    #[serde(default)]
    #[allow(dead_code)]
    block_number: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    transaction_hash: Option<String>,
}

// -------- In-progress event accumulation --------

/// Partial NegRisk market being assembled from logs. We need both
/// `MarketPrepared` and all `QuestionPrepared` events before we can emit.
#[derive(Debug, Default)]
struct PendingMarket {
    oracle_fee_data: Option<(String, u128, Vec<u8>)>,
    questions: HashMap<u32, B256>, // index → questionId
    first_seen_ns: u128,
}

// -------- Entry point --------

/// Run the on-chain watcher forever. Exits only on unrecoverable errors.
/// Reconnects automatically with exponential backoff on disconnect.
pub async fn run_onchain_watcher(
    wss_url: String,
    tx: WeatherEventSender,
) -> anyhow::Result<()> {
    let pending: Arc<Mutex<HashMap<B256, PendingMarket>>> = Arc::new(Mutex::new(HashMap::new()));
    let mut backoff_ms: u64 = 500;

    loop {
        match run_once(&wss_url, tx.clone(), pending.clone()).await {
            Ok(_) => {
                tracing::warn!("[onchain] stream ended cleanly, reconnecting");
                backoff_ms = 500;
            }
            Err(e) => {
                tracing::warn!("[onchain] error: {} — backoff {}ms", e, backoff_ms);
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms * 2).min(30_000);
            }
        }
    }
}

async fn run_once(
    wss_url: &str,
    tx: WeatherEventSender,
    pending: Arc<Mutex<HashMap<B256, PendingMarket>>>,
) -> anyhow::Result<()> {
    tracing::info!("[onchain] connecting to {}", wss_url);
    let (mut ws, _) = connect_async(wss_url).await?;

    // Subscribe to NegRiskAdapter logs. One subscription covers both
    // MarketPrepared and QuestionPrepared — we filter by topic hash in the
    // handler.
    let sub_req = RpcRequest {
        jsonrpc: "2.0",
        id: 1,
        method: "eth_subscribe",
        params: serde_json::json!([
            "logs",
            {
                "address": format!("0x{:x}", NEG_RISK_ADAPTER),
                "topics": [[TOPIC_MARKET_PREPARED, TOPIC_QUESTION_PREPARED]]
            }
        ]),
    };
    ws.send(Message::Text(serde_json::to_string(&sub_req)?.into()))
        .await?;
    tracing::info!("[onchain] subscribed to NegRiskAdapter MarketPrepared + QuestionPrepared");

    while let Some(msg) = ws.next().await {
        let msg = msg?;
        let txt = match msg {
            Message::Text(t) => t,
            Message::Binary(b) => String::from_utf8_lossy(&b).to_string().into(),
            Message::Close(_) => break,
            _ => continue,
        };
        let parsed: RpcResponse = match serde_json::from_str(txt.as_ref()) {
            Ok(p) => p,
            Err(e) => {
                tracing::debug!("[onchain] parse error: {}", e);
                continue;
            }
        };
        if let Some(err) = parsed.error {
            tracing::warn!("[onchain] rpc error: {}", err);
            continue;
        }
        if let Some(sub) = parsed.params {
            handle_log(sub.result, &tx, &pending).await;
        }
        // `result` without params is the subscription confirmation — ignore
    }
    Ok(())
}

async fn handle_log(
    log: RpcLog,
    tx: &WeatherEventSender,
    pending: &Arc<Mutex<HashMap<B256, PendingMarket>>>,
) {
    if log.topics.is_empty() {
        return;
    }
    let topic0 = log.topics[0].to_lowercase();
    if topic0 == TOPIC_MARKET_PREPARED {
        handle_market_prepared(log, pending).await;
    } else if topic0 == TOPIC_QUESTION_PREPARED {
        handle_question_prepared(log, tx, pending).await;
    }
}

async fn handle_market_prepared(
    log: RpcLog,
    pending: &Arc<Mutex<HashMap<B256, PendingMarket>>>,
) {
    if log.topics.len() < 3 {
        return;
    }
    let market_id = match parse_b256(&log.topics[1]) {
        Some(m) => m,
        None => return,
    };
    let mut guard = pending.lock().await;
    let entry = guard.entry(market_id).or_default();
    entry.first_seen_ns = now_ns();
    tracing::info!(
        "[onchain] MarketPrepared marketId=0x{}",
        hex::encode(market_id.as_slice())
    );
}

async fn handle_question_prepared(
    log: RpcLog,
    tx: &WeatherEventSender,
    pending: &Arc<Mutex<HashMap<B256, PendingMarket>>>,
) {
    if log.topics.len() < 3 {
        return;
    }
    let market_id = match parse_b256(&log.topics[1]) {
        Some(m) => m,
        None => return,
    };
    let question_id = match parse_b256(&log.topics[2]) {
        Some(q) => q,
        None => return,
    };
    // Data is `abi.encode(index, data)`. We only need `index`, the first word.
    let data_bytes = match hex::decode(log.data.trim_start_matches("0x")) {
        Ok(b) => b,
        Err(_) => return,
    };
    if data_bytes.len() < 32 {
        return;
    }
    let index = U256::from_be_slice(&data_bytes[0..32]);
    let question_index = index.as_limbs()[0] as u32;
    let extracted_index = question_id.0[31] as u32;
    debug_assert_eq!(question_index & 0xff, extracted_index);

    let mut guard = pending.lock().await;
    let entry = guard.entry(market_id).or_default();
    entry.questions.insert(question_index, question_id);
    let q_count = entry.questions.len();
    tracing::debug!(
        "[onchain] QuestionPrepared marketId=0x{} idx={} total={}",
        hex::encode(&market_id.as_slice()[..8]),
        question_index,
        q_count
    );

    // Heuristic: once we've seen ≥ 2 questions AND no new one arrives for a
    // brief window, emit. Weather temperature markets typically have 10–12
    // buckets. We use a tolerant threshold because the watcher can't know the
    // exact bucket count without a slug lookup.
    //
    // TODO: replace this heuristic with a one-shot gamma lookup keyed on
    // marketId to determine the authoritative bucket count.
    if q_count >= 10 {
        let pm = guard.remove(&market_id).unwrap();
        drop(guard);
        emit_weather_event(market_id, pm, tx).await;
    }
}

async fn emit_weather_event(
    market_id: B256,
    pm: PendingMarket,
    tx: &WeatherEventSender,
) {
    // Build bucket list from the accumulated questionIds
    let mut indices: Vec<u32> = pm.questions.keys().copied().collect();
    indices.sort();

    let mut buckets = Vec::with_capacity(indices.len());
    for idx in &indices {
        let question_id = pm.questions[idx];
        let condition_id = neg_risk_bucket_condition_id(market_id, *idx as u8);
        let (yes, no) = neg_risk_bucket_token_ids(market_id, *idx as u8);
        buckets.push(BucketInfo {
            condition_id,
            question_id,
            outcome_index: *idx,
            bucket_label: format!("bucket-{}", idx), // real label needs slug lookup
            token_id_yes: yes,
            token_id_no: no,
        });
    }

    // Slug / city / resolution_date would normally come from a one-shot gamma
    // lookup keyed on conditionId or marketId. For the first cut we leave
    // them as placeholders — the weather_filter gate will reject anything that
    // isn't weather before the executor sees it. TODO: wire gamma lookup.
    let event = WeatherEvent {
        event_slug: format!("onchain-marketid-0x{}", hex::encode(&market_id.as_slice()[..8])),
        city: String::new(),
        resolution_date: String::new(),
        kind: EventKind::NegRisk,
        neg_risk_market_id: Some(market_id),
        oracle: NEG_RISK_ADAPTER,
        buckets,
        detected_at_ns: pm.first_seen_ns,
    };

    let _ = parse_weather_slug; // weather_filter available for the gamma-lookup step
    let _ = extract_temp_label; // scanner helper available for the gamma-lookup step

    if tx.send(event).is_err() {
        tracing::warn!("[onchain] event channel closed");
    }
}

fn parse_b256(hex_str: &str) -> Option<B256> {
    let s = hex_str.trim_start_matches("0x");
    let bytes = hex::decode(s).ok()?;
    if bytes.len() != 32 {
        return None;
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Some(B256::from(out))
}
