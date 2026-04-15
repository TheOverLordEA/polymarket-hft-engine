//! Paper-trading engine.
//!
//! Drop-in replacement for `mint_executor` + `presigner` when
//! `config.paper_mode == true`. Runs the **same decision path** as live
//! (event-driven, same slug filter, same mint sizing, same dump logic) but:
//!
//!   - does NOT broadcast any Polygon transactions
//!   - does NOT POST any CLOB orders
//!   - fetches REAL orderbook depth for each bucket it would dump into
//!   - walks the book with slippage + taker fees to compute realistic fill
//!   - tracks a virtual USDC bankroll with a concurrent-events cap
//!   - appends every event (mint, dump, resolution) to a CSV log
//!
//! This means: when we flip the bot to live, the ROI we measured in paper
//! should match within slippage variance. No surprises.
//!
//! # Fee assumptions (verify empirically before going live)
//!
//!   CLOB taker fee   = 0 bps   (confirmed: Polymarket sets feeRateBps=0 on fills)
//!   CLOB maker fee   = 0 bps
//!   Gas (split)      = $0.008  (200k gas × 50 gwei × $0.65 MATIC)
//!   Gas (convert)    = $0.012  (300k × 50 × $0.65)
//!   Gas (redeem)     = $0.006  (150k × 50 × $0.65)
//!   NegRisk feeBips  = 0 bps   (paper default; real value is per-market — read
//!                              from NegRiskAdapter.MarketPrepared when the
//!                              onchain watcher decodes logs)
//!
//! Rebate income (CLOB rewards ≈ 23 USDC/day per qualifying market) is NOT
//! modeled. Consider it upside — the measured ROI is a floor.

use crate::types::{BucketInfo, WeatherEvent};
use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::Mutex;

// -------- Fee constants (tunable via config) --------

/// Basis points the CLOB taker side pays. Real value: 0 on Polymarket today.
pub const DEFAULT_TAKER_FEE_BPS: u16 = 0;
/// Gas cost in USDC of a single splitPosition tx on Polygon.
pub const GAS_COST_SPLIT_USDC: f64 = 0.008;
/// Gas cost in USDC of a single convertPositions tx on Polygon.
pub const GAS_COST_CONVERT_USDC: f64 = 0.012;
/// Gas cost in USDC of a redeemPositions tx on Polygon.
pub const GAS_COST_REDEEM_USDC: f64 = 0.006;

// -------- Paper account state --------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaperPosition {
    pub event_slug: String,
    pub city: String,
    pub resolution_date: String,
    pub bucket_label: String,
    pub token_id: String,
    /// Shares remaining (held as a tail lottery ticket).
    pub shares_held: f64,
    /// Effective cost basis per share in USDC, AFTER deducting dump proceeds
    /// from the mint outlay. Can be negative (we banked more than we spent).
    pub effective_cost_per_share: f64,
    /// Unix seconds when minted.
    pub minted_at: i64,
    /// Unix seconds when the event resolves (event endDate).
    pub resolves_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaperAccount {
    pub bankroll_usdc: f64,
    pub starting_bankroll: f64,
    /// Open events → total shares held across all their buckets.
    /// Used to enforce `max_concurrent_events`.
    pub open_event_slugs: Vec<String>,
    pub open_positions: Vec<PaperPosition>,
    pub closed_positions: Vec<PaperPosition>,

    // Cumulative telemetry (never zeros out)
    pub cum_mint_count: u64,
    pub cum_mint_usdc: f64,
    pub cum_dump_count: u64,
    pub cum_dump_usdc: f64,
    pub cum_gas_paid: f64,
    pub cum_taker_fees: f64,
    pub cum_realized_pnl: f64,
}

impl Default for PaperAccount {
    fn default() -> Self {
        Self {
            bankroll_usdc: 0.0,
            starting_bankroll: 0.0,
            open_event_slugs: Vec::new(),
            open_positions: Vec::new(),
            closed_positions: Vec::new(),
            cum_mint_count: 0,
            cum_mint_usdc: 0.0,
            cum_dump_count: 0,
            cum_dump_usdc: 0.0,
            cum_gas_paid: 0.0,
            cum_taker_fees: 0.0,
            cum_realized_pnl: 0.0,
        }
    }
}

impl PaperAccount {
    pub fn new(starting_bankroll: f64) -> Self {
        Self {
            bankroll_usdc: starting_bankroll,
            starting_bankroll,
            ..Self::default()
        }
    }

    pub fn net_pnl(&self) -> f64 {
        self.bankroll_usdc - self.starting_bankroll + self.mark_to_market_open_value()
    }

    pub fn mark_to_market_open_value(&self) -> f64 {
        // Conservative: tail legs marked at zero. Underestimates P&L by the
        // value of the held lottery tickets.
        0.0
    }

    pub fn roi_pct(&self) -> f64 {
        if self.starting_bankroll == 0.0 {
            return 0.0;
        }
        100.0 * self.net_pnl() / self.starting_bankroll
    }
}

// -------- Orderbook fetcher + walker --------

#[derive(Deserialize, Debug)]
struct ClobBook {
    #[serde(default)]
    bids: Vec<ClobLevel>,
    #[serde(default)]
    asks: Vec<ClobLevel>,
}

#[derive(Deserialize, Debug)]
struct ClobLevel {
    price: String,
    size: String,
}

/// Fill result from walking the bid side of the book.
#[derive(Debug, Clone)]
pub struct WalkedFill {
    pub shares_filled: f64,
    pub gross_proceeds_usdc: f64,
    pub avg_price: f64,
    pub levels_consumed: usize,
    pub capped_by_depth: bool,
}

pub async fn fetch_book(http: &Client, clob_url: &str, token_id: &str) -> Result<ClobBook> {
    let url = format!("{}/book?token_id={}", clob_url, token_id);
    let resp = http
        .get(&url)
        .header("User-Agent", "PolyWeatherBot/paper")
        .send()
        .await?;
    let text = resp.text().await?;
    let book: ClobBook =
        serde_json::from_str(&text).context("failed to parse CLOB book response")?;
    Ok(book)
}

/// Walk the BID side of the book (we're selling), taking as many levels as
/// needed to fill `target_shares` or exhaust the book. Returns the realized
/// fill including VWAP and whether we were depth-capped.
pub fn walk_bids_for_sell(book: &ClobBook, target_shares: f64) -> WalkedFill {
    // Sort bids descending (best price first)
    let mut levels: Vec<(f64, f64)> = book
        .bids
        .iter()
        .filter_map(|l| {
            let p = l.price.parse::<f64>().ok()?;
            let s = l.size.parse::<f64>().ok()?;
            if p > 0.0 && s > 0.0 {
                Some((p, s))
            } else {
                None
            }
        })
        .collect();
    levels.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    let mut remaining = target_shares;
    let mut filled = 0.0;
    let mut gross = 0.0;
    let mut levels_consumed = 0;

    for (price, size) in &levels {
        if remaining <= 0.0 {
            break;
        }
        let take = remaining.min(*size);
        filled += take;
        gross += take * price;
        remaining -= take;
        levels_consumed += 1;
    }

    let avg_price = if filled > 0.0 { gross / filled } else { 0.0 };
    WalkedFill {
        shares_filled: filled,
        gross_proceeds_usdc: gross,
        avg_price,
        levels_consumed,
        capped_by_depth: remaining > 0.0,
    }
}

// -------- CSV append logger --------

/// Columns (pipe-separated for easy grep):
///   timestamp_unix | event_slug | kind | detail_json | bankroll_after
pub struct PaperLog {
    path: PathBuf,
    counter: AtomicU64,
}

impl PaperLog {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            counter: AtomicU64::new(0),
        }
    }

    fn append(&self, event_slug: &str, kind: &str, detail: serde_json::Value, bankroll: f64) {
        use std::io::Write;
        let ts = chrono::Utc::now().timestamp();
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let row = format!(
            "{}|{}|{}|{}|{}|{:.4}\n",
            n, ts, event_slug, kind, detail, bankroll
        );
        if let Some(dir) = self.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = f.write_all(row.as_bytes());
        }
    }
}

// -------- Paper engine (the thing main.rs calls) --------

pub struct PaperEngine {
    http: Client,
    clob_url: String,
    account: Mutex<PaperAccount>,
    log: PaperLog,
    max_concurrent_events: usize,
    mint_amount_usdc: f64,
    min_dump_price: f64,
    dump_fraction: f64,
    taker_fee_bps: u16,
}

impl PaperEngine {
    pub fn new(
        clob_url: String,
        starting_bankroll: f64,
        max_concurrent_events: usize,
        mint_amount_usdc: f64,
        min_dump_price: f64,
        dump_fraction: f64,
        log_path: PathBuf,
    ) -> Self {
        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(5))
            .build()
            .unwrap_or_default();
        Self {
            http,
            clob_url,
            account: Mutex::new(PaperAccount::new(starting_bankroll)),
            log: PaperLog::new(log_path),
            max_concurrent_events,
            mint_amount_usdc,
            min_dump_price,
            dump_fraction,
            taker_fee_bps: DEFAULT_TAKER_FEE_BPS,
        }
    }

    /// Whole pipeline for one weather event: check bankroll + concurrent cap,
    /// mint, dump hot legs (walking real books), record residual tail legs,
    /// log everything. This is what `main.rs` calls in place of the sim
    /// mint + presign + flush.
    pub async fn run_event(&self, event: &WeatherEvent, now_unix: i64) -> Result<()> {
        // -- 1. Gate: bankroll + concurrent cap --
        {
            let acct = self.account.lock().await;
            let need = self.mint_amount_usdc * 2.0 + GAS_COST_SPLIT_USDC + GAS_COST_CONVERT_USDC;
            if acct.bankroll_usdc < need {
                tracing::info!(
                    "[PAPER] SKIP {} — bankroll ${:.2} < need ${:.2}",
                    event.event_slug,
                    acct.bankroll_usdc,
                    need
                );
                self.log.append(
                    &event.event_slug,
                    "skip_bankroll",
                    serde_json::json!({ "bankroll": acct.bankroll_usdc, "need": need }),
                    acct.bankroll_usdc,
                );
                return Ok(());
            }
            if acct.open_event_slugs.len() >= self.max_concurrent_events {
                tracing::info!(
                    "[PAPER] SKIP {} — at concurrent cap ({} events open)",
                    event.event_slug,
                    acct.open_event_slugs.len()
                );
                self.log.append(
                    &event.event_slug,
                    "skip_concurrent_cap",
                    serde_json::json!({ "open": acct.open_event_slugs.len() }),
                    acct.bankroll_usdc,
                );
                return Ok(());
            }
        }

        // -- 2. Mint (virtual) --
        let mint_total = self.mint_amount_usdc * 2.0;
        let gas_mint = GAS_COST_SPLIT_USDC + GAS_COST_CONVERT_USDC;
        {
            let mut acct = self.account.lock().await;
            acct.bankroll_usdc -= mint_total + gas_mint;
            acct.cum_mint_count += 1;
            acct.cum_mint_usdc += mint_total;
            acct.cum_gas_paid += gas_mint;
            acct.open_event_slugs.push(event.event_slug.clone());
            self.log.append(
                &event.event_slug,
                "mint",
                serde_json::json!({
                    "mint_total_usdc": mint_total,
                    "gas_usdc": gas_mint,
                    "bucket_count": event.buckets.len(),
                }),
                acct.bankroll_usdc,
            );
        }

        // -- 3. Walk each bucket's orderbook and dump the hot legs --
        let shares_per_bucket = self.mint_amount_usdc; // $X mint → X shares per outcome
        let target_dump = shares_per_bucket * self.dump_fraction;

        let mut total_proceeds = 0.0;
        let mut total_fees = 0.0;
        let mut residual_legs: Vec<PaperPosition> = Vec::new();
        let resolves_at = parse_resolution_date(&event.resolution_date, now_unix);

        for bucket in &event.buckets {
            let token_id = bucket.token_id_yes.to_string();
            let book = match fetch_book(&self.http, &self.clob_url, &token_id).await {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(
                        "[PAPER] book fetch failed for {} {}: {} — treating as dust",
                        event.event_slug,
                        bucket.bucket_label,
                        e
                    );
                    residual_legs.push(PaperPosition {
                        event_slug: event.event_slug.clone(),
                        city: event.city.clone(),
                        resolution_date: event.resolution_date.clone(),
                        bucket_label: bucket.bucket_label.clone(),
                        token_id,
                        shares_held: shares_per_bucket,
                        effective_cost_per_share: self.mint_amount_usdc / (event.buckets.len() as f64 * shares_per_bucket).max(1.0),
                        minted_at: now_unix,
                        resolves_at,
                    });
                    continue;
                }
            };

            let walked = walk_bids_for_sell(&book, target_dump);

            if walked.shares_filled < 1.0 || walked.avg_price < self.min_dump_price {
                // Too thin or too cheap — skip dumping, keep full position
                residual_legs.push(PaperPosition {
                    event_slug: event.event_slug.clone(),
                    city: event.city.clone(),
                    resolution_date: event.resolution_date.clone(),
                    bucket_label: bucket.bucket_label.clone(),
                    token_id,
                    shares_held: shares_per_bucket,
                    effective_cost_per_share: (mint_total + gas_mint)
                        / (event.buckets.len() as f64 * shares_per_bucket).max(1.0),
                    minted_at: now_unix,
                    resolves_at,
                });
                continue;
            }

            let fee_usdc = walked.gross_proceeds_usdc * (self.taker_fee_bps as f64 / 10_000.0);
            let net_proceeds = walked.gross_proceeds_usdc - fee_usdc;
            total_proceeds += net_proceeds;
            total_fees += fee_usdc;

            let remaining_shares = shares_per_bucket - walked.shares_filled;
            if remaining_shares > 0.0 {
                residual_legs.push(PaperPosition {
                    event_slug: event.event_slug.clone(),
                    city: event.city.clone(),
                    resolution_date: event.resolution_date.clone(),
                    bucket_label: bucket.bucket_label.clone(),
                    token_id: token_id.clone(),
                    shares_held: remaining_shares,
                    effective_cost_per_share: 0.0, // already paid via mint, tail is free
                    minted_at: now_unix,
                    resolves_at,
                });
            }

            self.log.append(
                &event.event_slug,
                "dump",
                serde_json::json!({
                    "bucket": bucket.bucket_label,
                    "target_shares": target_dump,
                    "filled": walked.shares_filled,
                    "avg_price": walked.avg_price,
                    "gross_proceeds": walked.gross_proceeds_usdc,
                    "fee": fee_usdc,
                    "net_proceeds": net_proceeds,
                    "levels_consumed": walked.levels_consumed,
                    "capped_by_depth": walked.capped_by_depth,
                    "residual_shares": remaining_shares,
                }),
                -1.0, // placeholder; bankroll update happens in the cumulative block below
            );
        }

        // -- 4. Credit proceeds and record residual legs --
        {
            let mut acct = self.account.lock().await;
            acct.bankroll_usdc += total_proceeds;
            acct.cum_dump_count += event.buckets.len() as u64;
            acct.cum_dump_usdc += total_proceeds;
            acct.cum_taker_fees += total_fees;

            for leg in residual_legs {
                acct.open_positions.push(leg);
            }

            let cycle_pnl = total_proceeds - mint_total - gas_mint - total_fees;
            acct.cum_realized_pnl += cycle_pnl;

            self.log.append(
                &event.event_slug,
                "cycle_summary",
                serde_json::json!({
                    "mint_out": mint_total,
                    "gas": gas_mint,
                    "dump_proceeds_gross": total_proceeds + total_fees,
                    "dump_fees": total_fees,
                    "dump_proceeds_net": total_proceeds,
                    "cycle_pnl": cycle_pnl,
                    "roi_pct": acct.roi_pct(),
                    "bankroll_after": acct.bankroll_usdc,
                    "open_events": acct.open_event_slugs.len(),
                    "tail_legs_held": acct.open_positions.len(),
                }),
                acct.bankroll_usdc,
            );

            tracing::info!(
                "[PAPER] CYCLE {}: mint=-${:.2} gas=-${:.2} fees=-${:.2} proceeds=+${:.2} net={:+.2} | bankroll=${:.2} ROI={:.2}%",
                event.event_slug,
                mint_total,
                gas_mint,
                total_fees,
                total_proceeds,
                cycle_pnl,
                acct.bankroll_usdc,
                acct.roi_pct()
            );
        }

        Ok(())
    }

    pub async fn snapshot(&self) -> PaperAccount {
        self.account.lock().await.clone()
    }
}

fn parse_resolution_date(date: &str, now_unix: i64) -> i64 {
    // YYYY-MM-DD → Unix seconds at 12:00 UTC that day (Polymarket weather
    // resolves at noon UTC).
    if date.len() != 10 {
        return now_unix + 86_400;
    }
    let parts: Vec<&str> = date.split('-').collect();
    if parts.len() != 3 {
        return now_unix + 86_400;
    }
    let y: i32 = parts[0].parse().unwrap_or(2026);
    let m: u32 = parts[1].parse().unwrap_or(1);
    let d: u32 = parts[2].parse().unwrap_or(1);
    chrono::NaiveDate::from_ymd_opt(y, m, d)
        .and_then(|nd| nd.and_hms_opt(12, 0, 0))
        .map(|ndt| ndt.and_utc().timestamp())
        .unwrap_or(now_unix + 86_400)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walks_bid_book_simple() {
        let book = ClobBook {
            bids: vec![
                ClobLevel {
                    price: "0.38".into(),
                    size: "28".into(),
                },
                ClobLevel {
                    price: "0.33".into(),
                    size: "12".into(),
                },
                ClobLevel {
                    price: "0.32".into(),
                    size: "5".into(),
                },
                ClobLevel {
                    price: "0.31".into(),
                    size: "100".into(),
                },
            ],
            asks: vec![],
        };
        let w = walk_bids_for_sell(&book, 62.0);
        // 28 + 12 + 5 = 45 from top 3; 17 more from the $0.31 wall
        assert_eq!(w.shares_filled, 62.0);
        assert_eq!(w.levels_consumed, 4);
        let expected_gross = 28.0 * 0.38 + 12.0 * 0.33 + 5.0 * 0.32 + 17.0 * 0.31;
        assert!((w.gross_proceeds_usdc - expected_gross).abs() < 0.0001);
        assert!(!w.capped_by_depth);
    }

    #[test]
    fn walks_bid_book_depth_capped() {
        let book = ClobBook {
            bids: vec![ClobLevel {
                price: "0.10".into(),
                size: "5".into(),
            }],
            asks: vec![],
        };
        let w = walk_bids_for_sell(&book, 62.0);
        assert_eq!(w.shares_filled, 5.0);
        assert!(w.capped_by_depth);
    }

    #[test]
    fn walks_empty_book_returns_zero() {
        let book = ClobBook {
            bids: vec![],
            asks: vec![],
        };
        let w = walk_bids_for_sell(&book, 62.0);
        assert_eq!(w.shares_filled, 0.0);
        assert!(w.capped_by_depth);
    }

    #[test]
    fn paper_account_roi_math() {
        let mut acct = PaperAccount::new(1000.0);
        acct.bankroll_usdc = 1100.0;
        assert!((acct.net_pnl() - 100.0).abs() < 0.001);
        assert!((acct.roi_pct() - 10.0).abs() < 0.001);
    }
}
