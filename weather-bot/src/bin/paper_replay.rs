//! One-shot paper replay harness.
//!
//! Fetches the current set of live Polymarket weather events from gamma,
//! constructs `WeatherEvent`s in-process via `ctf_math::neg_risk_bucket_token_ids`
//! (no on-chain subscription needed), and feeds each to `PaperEngine::run_event`.
//!
//! Purpose: measure paper ROI against REAL live orderbook state without
//! needing a Polygon WSS pubsub endpoint. Runs once, prints the aggregate
//! account state, and exits.
//!
//! Config: same env vars as `weather-bot`. Key differences from the
//! long-running bot:
//!   - No WSS subscriptions (no `onchain`/`clob_ws`/`mempool` watchers)
//!   - One deterministic sweep through `n` events, then exit
//!   - Skips events that the paper gate rejects (bankroll, concurrent cap)
//!
//! Usage:
//!   cargo run --bin paper-replay
//!
//! Env:
//!   PAPER_STARTING_BANKROLL   default 1000
//!   PAPER_MAX_CONCURRENT_EVENTS  default 8
//!   MINT_AMOUNT_USDC          default 62
//!   PAPER_REPLAY_LIMIT        max events to replay (default 20)

use alloy_primitives::B256;
use serde::Deserialize;
use std::path::PathBuf;
use weather_bot::config::Config;
use weather_bot::ctf_math::{neg_risk_bucket_condition_id, neg_risk_bucket_token_ids};
use weather_bot::paper::PaperEngine;
use weather_bot::types::{now_ns, BucketInfo, EventKind, WeatherEvent};

#[derive(Debug, Deserialize)]
struct GammaEvent {
    slug: String,
    #[serde(rename = "negRisk", default)]
    neg_risk: bool,
    #[serde(rename = "negRiskMarketID", default)]
    neg_risk_market_id: Option<String>,
    #[serde(default)]
    markets: Vec<GammaMarket>,
    #[serde(rename = "endDate", default)]
    end_date: Option<String>,
    #[serde(default)]
    #[allow(dead_code)]
    closed: bool,
}

/// Parse `"2026-04-15T12:00:00Z"` → unix seconds. Returns 0 on any failure,
/// which the filter treats as "already past" and drops.
fn parse_end_date(s: &str) -> i64 {
    chrono::DateTime::parse_from_rfc3339(s)
        .map(|dt| dt.timestamp())
        .unwrap_or(0)
}

#[derive(Debug, Deserialize)]
struct GammaMarket {
    #[serde(rename = "conditionId", default)]
    condition_id: String,
    #[serde(default)]
    question: String,
    #[serde(rename = "clobTokenIds", default)]
    clob_token_ids: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("weather_bot=info,paper_replay=info")
        .init();

    let config = Config::load();
    let replay_limit: usize = std::env::var("PAPER_REPLAY_LIMIT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);

    tracing::info!("===============================================");
    tracing::info!("  paper-replay — offline live-state replay     ");
    tracing::info!("===============================================");
    tracing::info!(
        "[CONFIG] bankroll=${:.0} max_concurrent={} mint=${:.2} min_dump=${:.3} replay_limit={}",
        config.paper_starting_bankroll,
        config.paper_max_concurrent_events,
        config.mint_amount_usdc,
        config.min_dump_price,
        replay_limit,
    );

    let paper = PaperEngine::new(
        config.clob_api_url.clone(),
        config.paper_starting_bankroll,
        config.paper_max_concurrent_events,
        config.mint_amount_usdc,
        config.min_dump_price,
        config.dump_fraction,
        PathBuf::from(&config.paper_log_path),
    );

    // -- Fetch live weather events from gamma --
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()?;
    let url = format!(
        "{}/events?tag_slug=weather&closed=false&limit=200",
        config.gamma_api_url
    );
    tracing::info!("[REPLAY] fetching gamma events from {}", url);

    let resp = http.get(&url).send().await?;
    let raw: serde_json::Value = resp.json().await?;
    let events: Vec<GammaEvent> = serde_json::from_value(raw)?;
    let now_unix = chrono::Utc::now().timestamp();

    // Require at least 6h of trading window remaining — drops events that
    // have already settled (gamma's `closed` flag updates lazily and returns
    // resolved events as `closed: false` for hours post-resolution).
    let min_hours_ahead: i64 = std::env::var("REPLAY_MIN_HOURS_AHEAD")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(6);
    let earliest_end = now_unix + (min_hours_ahead * 3600);

    let mut candidates: Vec<GammaEvent> = events
        .into_iter()
        .filter(|e| e.neg_risk && !e.markets.is_empty())
        .filter(|e| e.slug.starts_with("highest-temperature-in-"))
        .filter(|e| {
            let ends = e
                .end_date
                .as_deref()
                .map(parse_end_date)
                .unwrap_or(0);
            ends >= earliest_end
        })
        .collect();

    // Group by endDate and pick the SOONEST-resolving batch — that's the
    // closest analogue to a "mint-near-expiry" run against a mature book.
    candidates.sort_by_key(|e| e.end_date.as_deref().map(parse_end_date).unwrap_or(0));
    let target_end = candidates
        .first()
        .and_then(|e| e.end_date.clone())
        .unwrap_or_default();
    let weather_events: Vec<WeatherEvent> = candidates
        .into_iter()
        .filter(|e| e.end_date.as_deref() == Some(&target_end))
        .take(replay_limit)
        .filter_map(|e| gamma_to_weather_event(e).ok())
        .collect();

    tracing::info!(
        "[REPLAY] {} live weather events selected (endDate={}, min_hours_ahead={}h)",
        weather_events.len(),
        target_end,
        min_hours_ahead,
    );

    if weather_events.is_empty() {
        tracing::warn!("[REPLAY] no live weather events found — nothing to replay");
        return Ok(());
    }

    // -- Feed each event through the paper engine, sequentially --
    for (i, event) in weather_events.iter().enumerate() {
        tracing::info!(
            "[REPLAY {}/{}] {} buckets={}",
            i + 1,
            weather_events.len(),
            event.event_slug,
            event.buckets.len()
        );
        if let Err(e) = paper.run_event(event, now_unix).await {
            tracing::warn!("[REPLAY] {} failed: {}", event.event_slug, e);
        }
    }

    // -- Final account snapshot --
    let acct = paper.snapshot().await;
    tracing::info!("===============================================");
    tracing::info!("  FINAL ACCOUNT STATE");
    tracing::info!("===============================================");
    tracing::info!("  starting_bankroll:      ${:.2}", acct.starting_bankroll);
    tracing::info!("  bankroll_now:           ${:.2}", acct.bankroll_usdc);
    tracing::info!("  net_pnl:                ${:+.2}", acct.net_pnl());
    tracing::info!("  ROI:                    {:+.3}%", acct.roi_pct());
    tracing::info!("  ---");
    tracing::info!("  cum_mint_count:         {}", acct.cum_mint_count);
    tracing::info!("  cum_mint_usdc_out:      ${:.2}", acct.cum_mint_usdc);
    tracing::info!("  cum_dump_count:         {}", acct.cum_dump_count);
    tracing::info!("  cum_dump_usdc_in:       ${:.2}", acct.cum_dump_usdc);
    tracing::info!("  cum_gas_paid:           ${:.4}", acct.cum_gas_paid);
    tracing::info!("  cum_taker_fees:         ${:.4}", acct.cum_taker_fees);
    tracing::info!("  cum_realized_pnl:       ${:+.2}", acct.cum_realized_pnl);
    tracing::info!("  ---");
    tracing::info!("  open_event_slugs:       {}", acct.open_event_slugs.len());
    tracing::info!("  open_positions_held:    {}", acct.open_positions.len());
    tracing::info!("  closed_positions:       {}", acct.closed_positions.len());
    tracing::info!("  log_path:               {}", config.paper_log_path);

    Ok(())
}

/// Build a WeatherEvent from a gamma `event` record. All ERC-1155 token IDs
/// are derived OFF-CHAIN via `ctf_math` rather than parsed from the gamma
/// `clobTokenIds` — this is the exact same code path the live on-chain
/// watcher uses, so the replay exercises the token-ID derivation end-to-end.
fn gamma_to_weather_event(ev: GammaEvent) -> anyhow::Result<WeatherEvent> {
    let market_id_hex = ev
        .neg_risk_market_id
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("no negRiskMarketID on {}", ev.slug))?;
    let market_id = parse_b256(market_id_hex)?;

    let (city, resolution_date) =
        weather_bot::weather_filter::parse_weather_slug(&ev.slug).unwrap_or_default();

    let mut buckets = Vec::with_capacity(ev.markets.len());
    for (idx, m) in ev.markets.iter().enumerate() {
        let bucket_label = weather_bot::scanner::extract_temp_label(&m.question);
        let q_idx = idx as u8;
        let condition_id = neg_risk_bucket_condition_id(market_id, q_idx);
        let (yes, no) = neg_risk_bucket_token_ids(market_id, q_idx);
        buckets.push(BucketInfo {
            condition_id,
            question_id: alloy_primitives::B256::from({
                let mut b = market_id.0;
                b[31] = q_idx;
                b
            }),
            outcome_index: idx as u32,
            bucket_label,
            token_id_yes: yes,
            token_id_no: no,
        });
    }

    Ok(WeatherEvent {
        event_slug: ev.slug,
        city,
        resolution_date,
        kind: EventKind::NegRisk,
        neg_risk_market_id: Some(market_id),
        oracle: weather_bot::ctf_math::NEG_RISK_ADAPTER,
        buckets,
        detected_at_ns: now_ns(),
    })
}

fn parse_b256(s: &str) -> anyhow::Result<B256> {
    let s = s.trim_start_matches("0x");
    let bytes = hex::decode(s)?;
    anyhow::ensure!(bytes.len() == 32, "expected 32 bytes, got {}", bytes.len());
    let mut out = [0u8; 32];
    out.copy_from_slice(&bytes);
    Ok(B256::from(out))
}
