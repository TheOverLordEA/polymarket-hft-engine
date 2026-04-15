//! Paper-trading engine.
//!
//! Drop-in replacement for `mint_executor` + `presigner` when
//! `config.paper_mode == true`. Runs the same decision path as live
//! (event-driven, same slug filter, same mint sizing) but shadows the
//! on-chain + CLOB sides:
//!
//!   - does NOT broadcast any Polygon transactions
//!   - does NOT POST any CLOB orders
//!   - fetches REAL orderbook depth for each bucket it would dump into
//!   - walks the book with taker fees to compute a realistic fill
//!   - tracks a virtual USDC bankroll with a concurrent-events cap
//!   - appends every event (mint, dump, cycle) to a structured log
//!
//! # KNOWN LIMITATIONS — read before interpreting the numbers
//!
//! Paper ROI is a **lower bound**, not a faithful replica of live P&L.
//! Three systematic biases push the measured number below reality:
//!
//! **1. Taker-dump assumption.** `run_event` walks the BID side of the
//!    book and consumes levels as a taker. The reference wallet
//!    (`@IWantYourMoney`) does NOT do this — he posts ASKS above the top
//!    bid and waits for flow, realizing ~$0.42/share avg on Lucknow 40°C
//!    where the top bid today is $0.25. A taker walk underestimates his
//!    realized price by $0.15–0.20/share × ~200 hot-leg shares/cycle =
//!    $30–40 per cycle of missing edge. Paper measures the PESSIMISTIC
//!    bound of an impatient-dumper strategy, NOT the maker-flush strategy
//!    you would run live. TODO: add `MakerDumpStrategy { markup, fill_rate_model }`
//!    and keep taker as the floor.
//!
//! **2. Rebate income not modeled.** CLOB liquidity rewards pay ~23 USDC/day
//!    per qualifying market to makers. `MINT_AMOUNT_USDC = $62` is chosen
//!    specifically to clear the `rewardsMinSize = 50` eligibility floor, so
//!    the live strategy DOES earn this rebate stream. Paper treats it as
//!    zero. At ~80 weather markets/day and fractional share of eligible
//!    liquidity, missing income is on the order of $5–$50/day.
//!
//! **3. Tail-leg resolution EV not modeled.** Residual shares from
//!    incomplete dumps are stored at `effective_cost_per_share = 0` (free
//!    lottery tickets) but are NEVER marked to $1 on win or $0 on loss at
//!    resolution. For a $62 mint across 11 buckets, tail-leg EV is
//!    ~$5–7/cycle, or roughly 30–50% of total edge. Paper swallows all of
//!    it. A gamma-events poller that marks `open_positions` at resolution
//!    is a must-have before comparing paper to anything meaningful. TODO,
//!    not in this commit.
//!
//! **Net effect**: paper's `cycle_pnl` is a go/no-go FLOOR. If paper is
//! profitable at a given mint size, live should be MORE profitable. If
//! paper is unprofitable, live *might* still be profitable, but you've
//! ruled out the pessimistic case. Do NOT compare paper ROI directly to
//! `@IWantYourMoney`'s realized numbers — you're measuring a different
//! execution style.
//!
//! # Pre-live gotcha: presigner stub
//!
//! `presigner::estimate_bucket_price` returns `$0.10` for every bucket.
//! Paper bypasses it entirely, but live mode uses it for maker-side
//! quoting. Replace it with a real estimator (fetch `/book` at presign
//! time, or use a climatology prior) BEFORE any live order is posted,
//! or every first live cycle will post asks at stub-price nonsense.
//!
//! # Fee assumptions (verified against CLOB endpoints)
//!
//!   CLOB taker fee   = 0 bps   (Polymarket sets feeRateBps=0 on fills)
//!   CLOB maker fee   = 0 bps
//!   Gas (split)      = $0.008  (200k gas × 50 gwei × $0.65 MATIC)
//!   Gas (convert)    = $0.012  (300k × 50 × $0.65)
//!   Gas (redeem)     = $0.006  (150k × 50 × $0.65)
//!   NegRisk feeBips  = 0 bps   (per-market default; real value emitted in
//!                              NegRiskAdapter.MarketPrepared — onchain
//!                              watcher TODO to decode and override)

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
        self.net_pnl_at_mark(0.0)
    }

    /// Compute realized P&L (no mark-to-market). Conservative — the tail
    /// legs we hold are worth *something*, but this method pretends they
    /// aren't. Use `net_pnl_at_mark(open_value)` when you have a real
    /// estimate of the held shares' value from `PaperEngine::mark_to_market`.
    pub fn realized_pnl(&self) -> f64 {
        self.bankroll_usdc - self.starting_bankroll
    }

    /// Realized P&L plus an externally-computed mark-to-market of open
    /// positions. The mark usually comes from `PaperEngine::mark_to_market`
    /// which walks each held token's current CLOB midpoint.
    pub fn net_pnl_at_mark(&self, open_value: f64) -> f64 {
        self.bankroll_usdc - self.starting_bankroll + open_value
    }

    pub fn roi_pct(&self) -> f64 {
        if self.starting_bankroll == 0.0 {
            return 0.0;
        }
        100.0 * self.realized_pnl() / self.starting_bankroll
    }

    pub fn roi_pct_at_mark(&self, open_value: f64) -> f64 {
        if self.starting_bankroll == 0.0 {
            return 0.0;
        }
        100.0 * self.net_pnl_at_mark(open_value) / self.starting_bankroll
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

// -------- Dump strategy: taker walk vs maker-ask simulation --------
//
// # Background
//
// The taker walk (`walk_bids_for_sell`) models an impatient dumper: post a
// market-sell, consume the bid side top-down. Gets certain execution at
// prevailing bid prices. For weather markets today that's ~$0.33-0.42 on
// hot legs (depending on book shape), which is strictly below what a
// patient maker realizes.
//
// The reference competitor @IWantYourMoney doesn't dump as a taker. He
// posts ASKS above the top bid and lets flow come to him — realized
// ~$0.42/share avg on Lucknow 40°C vs current top bid of $0.25. The edge
// is the maker-to-taker spread: roughly $40/cycle at the current book
// shapes, or ~50% of the total measurement gap between paper and his
// realized numbers.
//
// This module adds a simulated-maker alternative. It does NOT actually
// post orders (that's a live-mode concern). It simulates what a maker
// dump WOULD realize given:
//
//   1. An ask price: `max(clob_mid, mint_cost_per_share) * markup`.
//      The `max` handles brand-new markets where the mid is the uniform
//      prior `1/n_buckets`. Markup defaults to 1.10 (10% above fair).
//
//   2. A fill rate: constant probability that a maker-posted order gets
//      lifted before the event resolves. Defaults to 0.75 — empirically
//      calibrated against a casual read of the competitor's activity but
//      NOT rigorously validated. This is the biggest free parameter in
//      the model.
//
//   3. Partial-fill semantics: the filled fraction credits proceeds, the
//      unfilled residual stays as a tail lottery ticket (handled by the
//      existing residual_legs + resolver path — no special flatten).
//
// # Design decisions
//
// - **No ensemble fair-value model**: v1 uses the CLOB mid directly. An
//   ensemble prior is a v2 task if the mid turns out to be a bad estimate
//   (it isn't — the n=1 calibration against resolved outcomes agrees
//   within 0.6pp).
//
// - **No timeout-flatten**: the 3b skip rule's whole reason for existing
//   is that holding a tail leg dominates a below-cost dump. A T-2h
//   "flatten residual to taker" would contradict that and surrender the
//   ~$10/cycle tail resolution EV that 3c just measured. Unfilled maker
//   asks stay as tails, period.
//
// - **No re-asks**: post once per cycle at the chosen markup. If the
//   book moves, we don't chase. Keeps the v1 model tight and the fill
//   rate a single knob.
//
// - **Same price floor as taker**: if `ask_price < max(min_dump_price,
//   mint_cost_per_share)` the bucket gets skipped via the same logic as
//   the taker path — maker doesn't let you dump at below cost either.
//
// # What v2 should add, in order
//
// 1. Empirical fill-rate from recent trade count on the token (replace
//    the constant 0.75 with `recent_trades_60m >= 5 ? 0.75 : 0.40`).
// 2. Ensemble fair-value model for the ask floor (replaces `max(mid,
//    mint_cost)` with `max(mid, ensemble_fair, mint_cost)`).
// 3. Per-bucket markup scaling (hot buckets at 1.10, tail buckets at
//    1.05 or skip entirely).
// 4. Timeout flatten ONLY if measurements show tail EV is below
//    mid-1-tick at T-2h — an open empirical question.

#[derive(Debug, Clone, Copy)]
pub enum DumpStrategy {
    /// Walk the bid side top-down with a per-level price floor. Models
    /// an impatient dumper that gets certain execution at prevailing
    /// bids. This is the pessimistic floor.
    Taker,
    /// Simulate posting a maker ask at `max(mid, mint_cost) * markup`
    /// with a constant `fill_rate`. Partial fills credit the filled
    /// fraction; unfilled residual stays as tail leg.
    Maker { markup: f64, fill_rate: f64 },
}

impl Default for DumpStrategy {
    fn default() -> Self {
        DumpStrategy::Taker
    }
}

/// Book-state-aware fill-rate estimate for a maker ask posted at `ask_price`.
///
/// Rules:
///   - If the book has no existing asks (fresh market), OR our ask sits at
///     or below the current best ask, we become (or remain) the new best
///     ask and should be lifted quickly — `max_rate`.
///
///   - If our ask is ABOVE the best ask, we're sitting behind a wall of
///     existing liquidity that taker flow has to clear first. Fill rate
///     decays linearly with the percentage gap, reaching a floor of
///     `max_rate / 2` at 10% above the best ask.
///
/// This replaces the constant-fill-rate assumption of the first maker
/// model, which produced a linear markup sweep (every +0.05 added the
/// same ~2.8pp of ROI). With the dynamic rate, higher markups push us
/// further behind the wall and depress the fill rate — so the sweep
/// should turn non-linear with an empirically discoverable optimum.
pub fn dynamic_fill_rate(book: &ClobBook, ask_price: f64, max_rate: f64) -> f64 {
    let best_ask = book
        .asks
        .iter()
        .filter_map(|l| l.price.parse::<f64>().ok())
        .fold(f64::MAX, f64::min);

    if best_ask == f64::MAX || ask_price <= best_ask {
        return max_rate;
    }

    let gap_pct = (ask_price - best_ask) / best_ask;
    let decay_window = 0.10; // linearly decay over 10% above best ask
    let floor = max_rate * 0.5;
    let t = (gap_pct / decay_window).min(1.0);
    max_rate * (1.0 - t) + floor * t
}

/// Simulate a maker dump: post one ask at `ask_price`, assume the
/// book-aware `dynamic_fill_rate` of the target fills before expiry.
/// Returns a WalkedFill compatible with the taker path so the rest of
/// `run_event` doesn't need to branch.
///
/// `max_fill_rate` is the ceiling — what you'd get if you were inside
/// the spread. Behind-the-wall asks decay from there.
pub fn simulate_maker_dump(
    book: &ClobBook,
    target_shares: f64,
    markup: f64,
    max_fill_rate: f64,
    mint_cost_per_share: f64,
) -> WalkedFill {
    // Fair value = max(clob mid, mint_cost_per_share). The latter is the
    // uniform prior (1/n_buckets) and acts as a floor for brand-new books.
    let mid = book_midpoint(book).unwrap_or(mint_cost_per_share);
    let fair = mid.max(mint_cost_per_share);
    let ask_price = fair * markup;

    let fill_rate = dynamic_fill_rate(book, ask_price, max_fill_rate);
    let filled = target_shares * fill_rate;
    let gross = filled * ask_price;

    WalkedFill {
        shares_filled: filled,
        gross_proceeds_usdc: gross,
        avg_price: ask_price,
        levels_consumed: 1,
        capped_by_depth: false,
    }
}

/// Walk the BID side of the book (we're selling), taking as many levels as
/// needed to fill `target_shares` or exhaust the book. Any level priced
/// below `min_price` is dropped BEFORE walking — this prevents a thin-tail
/// book (5 shares @ $0.30 + 100 @ $0.02) from diluting the realized VWAP
/// below the profitability threshold.
///
/// `min_price = 0.0` disables the filter (legacy behavior).
pub fn walk_bids_for_sell(book: &ClobBook, target_shares: f64, min_price: f64) -> WalkedFill {
    // Sort bids descending (best price first), drop anything below the floor.
    let mut levels: Vec<(f64, f64)> = book
        .bids
        .iter()
        .filter_map(|l| {
            let p = l.price.parse::<f64>().ok()?;
            let s = l.size.parse::<f64>().ok()?;
            if p >= min_price && s > 0.0 {
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
    dump_strategy: DumpStrategy,
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
        Self::new_with_strategy(
            clob_url,
            starting_bankroll,
            max_concurrent_events,
            mint_amount_usdc,
            min_dump_price,
            dump_fraction,
            log_path,
            DumpStrategy::default(),
        )
    }

    pub fn new_with_strategy(
        clob_url: String,
        starting_bankroll: f64,
        max_concurrent_events: usize,
        mint_amount_usdc: f64,
        min_dump_price: f64,
        dump_fraction: f64,
        log_path: PathBuf,
        dump_strategy: DumpStrategy,
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
            dump_strategy,
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

        // Mint-cost VWAP threshold: 1 / n_buckets is the imputed cost per
        // share when $X USDC mints X shares across N equal-weight outcomes
        // (matches gamma's avgPrice calculation for neg-risk conversions).
        // For $62 across 11 buckets: $62 / (62 * 11) = $0.091/share.
        //
        // Rule: if the VWAP we would realize by walking the bid side is
        // BELOW this threshold, we're dumping at a loss per share. Strictly
        // dominated by holding as a tail lottery ticket (worst-case P(win)=0
        // means we lose $0, whereas the sub-threshold dump crystallizes the
        // loss immediately). So skip.
        //
        // This is a strict upgrade over a simple `shares_filled == 0` check:
        // Paris-style empty books still get skipped, but so do markets with
        // thin bids at 1-2c that would return pennies on the dollar.
        let n_buckets = event.buckets.len().max(1) as f64;
        let mint_cost_per_share = self.mint_amount_usdc / (shares_per_bucket * n_buckets);

        let mut total_proceeds = 0.0;
        let mut total_fees = 0.0;
        let mut residual_legs: Vec<PaperPosition> = Vec::new();
        let mut skipped_below_cost_count: u64 = 0;
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
                        effective_cost_per_share: mint_cost_per_share,
                        minted_at: now_unix,
                        resolves_at,
                    });
                    continue;
                }
            };

            // Effective price floor: we will only consume levels priced at
            // or above this threshold. The MAX of the legacy dust floor
            // (min_dump_price) and the imputed mint cost per share — a
            // dump at price < cost-basis is strictly dominated by holding
            // the share as a tail lottery ticket.
            let price_floor = self.min_dump_price.max(mint_cost_per_share);

            // Branch on dump strategy. Taker walks the bid book with the
            // price floor; Maker simulates one ask post at a markup above
            // fair value and credits `fill_rate * target_dump` shares at
            // that ask price. Both paths produce a WalkedFill so the rest
            // of this loop is strategy-agnostic.
            let walked = match self.dump_strategy {
                DumpStrategy::Taker => {
                    walk_bids_for_sell(&book, target_dump, price_floor)
                }
                DumpStrategy::Maker { markup, fill_rate } => {
                    let w = simulate_maker_dump(
                        &book,
                        target_dump,
                        markup,
                        fill_rate,
                        mint_cost_per_share,
                    );
                    // Apply the price floor post-hoc for consistency with
                    // the taker path. Normally `fair * markup` is well
                    // above the floor (fair >= mint_cost, markup >= 1.0),
                    // but brand-new markets with sub-floor mids could slip.
                    if w.avg_price < price_floor {
                        WalkedFill {
                            shares_filled: 0.0,
                            gross_proceeds_usdc: 0.0,
                            avg_price: w.avg_price,
                            levels_consumed: 0,
                            capped_by_depth: false,
                        }
                    } else {
                        w
                    }
                }
            };

            if walked.shares_filled < 1.0 {
                // Nothing filled above the floor — either the book was
                // empty, or every bid was sub-cost. Hold as a tail.
                let reason = if book.bids.is_empty() {
                    "empty_book"
                } else {
                    "all_bids_below_floor"
                };
                if reason == "all_bids_below_floor" {
                    skipped_below_cost_count += 1;
                }
                self.log.append(
                    &event.event_slug,
                    "dump_skipped",
                    serde_json::json!({
                        "bucket": bucket.bucket_label,
                        "reason": reason,
                        "price_floor": price_floor,
                        "mint_cost_per_share": mint_cost_per_share,
                        "min_dump_price": self.min_dump_price,
                        "book_bids_count": book.bids.len(),
                    }),
                    -1.0,
                );
                residual_legs.push(PaperPosition {
                    event_slug: event.event_slug.clone(),
                    city: event.city.clone(),
                    resolution_date: event.resolution_date.clone(),
                    bucket_label: bucket.bucket_label.clone(),
                    token_id,
                    shares_held: shares_per_bucket,
                    effective_cost_per_share: mint_cost_per_share,
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
                    "skipped_below_cost": skipped_below_cost_count,
                    "mint_cost_per_share_threshold": mint_cost_per_share,
                }),
                acct.bankroll_usdc,
            );

            tracing::info!(
                "[PAPER] CYCLE {}: mint=-${:.2} gas=-${:.2} fees=-${:.2} proceeds=+${:.2} net={:+.2} | bankroll=${:.2} ROI={:.2}% skipped_below_cost={}",
                event.event_slug,
                mint_total,
                gas_mint,
                total_fees,
                total_proceeds,
                cycle_pnl,
                acct.bankroll_usdc,
                acct.roi_pct(),
                skipped_below_cost_count,
            );
        }

        Ok(())
    }

    pub async fn snapshot(&self) -> PaperAccount {
        self.account.lock().await.clone()
    }

    /// Sweep open positions for resolved events and settle them. For each
    /// unique event_slug in `open_positions`, query gamma for its
    /// `closed` state and `outcomePrices`. If closed:
    ///   - Move every bucket position for that event to `closed_positions`
    ///   - Credit `bankroll_usdc` with `shares * price` where price is the
    ///     on-chain outcome price ($1 for the winning bucket, $0 for the
    ///     losing ones — or the intermediate value if the oracle reported
    ///     a partial payout, which happens occasionally for ambiguous
    ///     temperature readings).
    ///
    /// For the long-running bot, call this on a `tokio::time::interval`
    /// (e.g. every 60s). For the replay harness, call it once at the end
    /// of the run — any event that's already resolved gets settled, the
    /// rest stay in `open_positions` and fall back to the CLOB-mid mark.
    ///
    /// Returns the USDC value credited to the bankroll from resolutions.
    pub async fn resolve_settled_events(&self, gamma_url: &str) -> Result<f64> {
        // Collect unique slugs from the open positions first to avoid
        // holding the account lock across network I/O.
        let slugs: std::collections::HashSet<String> = {
            let acct = self.account.lock().await;
            acct.open_positions.iter().map(|p| p.event_slug.clone()).collect()
        };
        tracing::info!(
            "[PAPER] resolve_settled_events: checking {} unique slugs",
            slugs.len()
        );

        let mut total_credit = 0.0;
        let mut resolved_events = 0usize;
        let mut resolved_positions = 0usize;

        for slug in slugs {
            let url = format!("{}/events/slug/{}", gamma_url, slug);
            let ev: serde_json::Value = match self
                .http
                .get(&url)
                .send()
                .await
                .and_then(|r| r.error_for_status())
            {
                Ok(r) => match r.json().await {
                    Ok(v) => v,
                    Err(_) => continue,
                },
                Err(_) => continue,
            };

            // Gamma's `closed` flag updates lazily (events 13h past
            // resolution still report closed:false). The reliable signal
            // is: endDate in the past AND at least one market has an
            // outcomePrices that's not the uniform prior.
            let closed = ev.get("closed").and_then(|c| c.as_bool()).unwrap_or(false);
            let end_unix = ev
                .get("endDate")
                .and_then(|d| d.as_str())
                .map(|s| {
                    chrono::DateTime::parse_from_rfc3339(s)
                        .map(|dt| dt.timestamp())
                        .unwrap_or(i64::MAX)
                })
                .unwrap_or(i64::MAX);
            let now_unix = chrono::Utc::now().timestamp();
            let past_end = end_unix <= now_unix;
            if !closed && !past_end {
                continue;
            }

            // Build a bucket_label -> winning bool map. Gamma exposes
            // outcomePrices per market as a JSON-encoded string. Index 0 is
            // YES, index 1 is NO. yes_price > 0.5 means this bucket wins.
            //
            // If NO market has a resolved outcomePrices yet (all still at
            // uniform priors), skip — the oracle hasn't reported.
            let mut winning_labels: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            let mut resolved_any = false;
            if let Some(markets) = ev.get("markets").and_then(|m| m.as_array()) {
                for m in markets {
                    let q = m.get("question").and_then(|q| q.as_str()).unwrap_or("");
                    let label = crate::scanner::extract_temp_label(q);
                    let prices_str = m
                        .get("outcomePrices")
                        .and_then(|p| p.as_str())
                        .unwrap_or("[]");
                    let prices: Vec<String> =
                        serde_json::from_str(prices_str).unwrap_or_default();
                    let yes_price = prices
                        .first()
                        .and_then(|s| s.parse::<f64>().ok())
                        .unwrap_or(-1.0);
                    if yes_price < 0.0 {
                        continue;
                    }
                    // Uniform prior = unresolved. A resolved market reports
                    // either "0" or "1" exactly.
                    if yes_price == 0.0 || yes_price == 1.0 {
                        resolved_any = true;
                    }
                    if yes_price > 0.5 {
                        winning_labels.insert(label);
                    }
                }
            }
            if !resolved_any {
                continue;
            }

            // Now take the account lock and settle every position for this
            // slug.
            let mut acct = self.account.lock().await;
            let (to_close, stay): (Vec<_>, Vec<_>) = acct
                .open_positions
                .drain(..)
                .partition(|p| p.event_slug == slug);
            acct.open_positions = stay;

            let event_credit: f64 = to_close
                .iter()
                .map(|p| {
                    if winning_labels.contains(&p.bucket_label) {
                        p.shares_held * 1.0
                    } else {
                        0.0
                    }
                })
                .sum();

            resolved_positions += to_close.len();
            total_credit += event_credit;
            resolved_events += 1;
            acct.bankroll_usdc += event_credit;
            acct.cum_realized_pnl += event_credit;
            acct.closed_positions.extend(to_close);
            acct.open_event_slugs.retain(|s| s != &slug);
            drop(acct);

            self.log.append(
                &slug,
                "resolution_settled",
                serde_json::json!({
                    "winners": winning_labels.iter().collect::<Vec<_>>(),
                    "credit_usdc": event_credit,
                }),
                -1.0,
            );
        }

        tracing::info!(
            "[PAPER] resolved {} events, {} positions settled, +${:.2} credited",
            resolved_events,
            resolved_positions,
            total_credit
        );
        Ok(total_credit)
    }

    /// Mark-to-market all open (tail) positions at the current CLOB
    /// midpoint. Fetches `/book` for each held token_id, uses
    /// `(best_bid + best_ask) / 2` as the share's implied win probability,
    /// and returns the total USDC value if every position were liquidated
    /// at that midpoint.
    ///
    /// Fallback if the book has no bids: use best_ask (we could sell to
    /// any taker at ≤ ask); if no asks: 0. If no quotes at all, the tail
    /// leg is implicitly worth 0 and excluded.
    ///
    /// This is NOT a resolution poll — it does not wait for the event's
    /// actual outcome. It's a snapshot estimate good enough to credit the
    /// skip rule against a plausible holding value. The proper resolution
    /// poller replaces this with the actual $1/$0 settlement in a later
    /// commit.
    pub async fn mark_to_market(&self) -> f64 {
        let acct = self.account.lock().await;
        let positions = acct.open_positions.clone();
        drop(acct);

        let mut total = 0.0;
        let mut priced = 0usize;
        let mut unpriced = 0usize;

        for pos in &positions {
            let mid = match fetch_book(&self.http, &self.clob_url, &pos.token_id).await {
                Ok(book) => book_midpoint(&book).unwrap_or(0.0),
                Err(_) => 0.0,
            };
            if mid > 0.0 {
                total += pos.shares_held * mid;
                priced += 1;
            } else {
                unpriced += 1;
            }
        }
        tracing::info!(
            "[PAPER] mark-to-market: {} positions priced, {} unpriced, total ${:.2}",
            priced,
            unpriced,
            total
        );
        total
    }
}

/// Midpoint of a CLOB book. Returns `None` if there's no sensible quote.
fn book_midpoint(book: &ClobBook) -> Option<f64> {
    let best_bid = book
        .bids
        .iter()
        .filter_map(|l| l.price.parse::<f64>().ok())
        .fold(f64::MIN, f64::max);
    let best_ask = book
        .asks
        .iter()
        .filter_map(|l| l.price.parse::<f64>().ok())
        .fold(f64::MAX, f64::min);
    if best_bid > f64::MIN && best_ask < f64::MAX {
        Some((best_bid + best_ask) / 2.0)
    } else if best_bid > f64::MIN {
        Some(best_bid)
    } else if best_ask < f64::MAX {
        Some(best_ask)
    } else {
        None
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
        let w = walk_bids_for_sell(&book, 62.0, 0.0);
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
        let w = walk_bids_for_sell(&book, 62.0, 0.0);
        assert_eq!(w.shares_filled, 5.0);
        assert!(w.capped_by_depth);
    }

    #[test]
    fn walks_empty_book_returns_zero() {
        let book = ClobBook {
            bids: vec![],
            asks: vec![],
        };
        let w = walk_bids_for_sell(&book, 62.0, 0.0);
        assert_eq!(w.shares_filled, 0.0);
        assert!(w.capped_by_depth);
    }

    #[test]
    fn price_floor_drops_thin_tail_from_walk() {
        // Book: 5 good shares, then a long tail of cheap shares that
        // would drag the VWAP below the floor if we didn't filter.
        let book = ClobBook {
            bids: vec![
                ClobLevel { price: "0.30".into(), size: "5".into() },
                ClobLevel { price: "0.05".into(), size: "100".into() },
                ClobLevel { price: "0.02".into(), size: "500".into() },
            ],
            asks: vec![],
        };

        // With floor=0.091, only the $0.30 level qualifies; walk fills 5.
        let w = walk_bids_for_sell(&book, 62.0, 0.091);
        assert_eq!(w.shares_filled, 5.0);
        assert!((w.avg_price - 0.30).abs() < 0.0001);
        assert_eq!(w.levels_consumed, 1);
        assert!(w.capped_by_depth);

        // Without a floor, we'd drain all 62 shares at a diluted VWAP.
        let w_legacy = walk_bids_for_sell(&book, 62.0, 0.0);
        assert_eq!(w_legacy.shares_filled, 62.0);
        assert!(w_legacy.avg_price < 0.091); // dilution confirmed
    }

    #[test]
    fn price_floor_empty_book_returns_zero() {
        let book = ClobBook {
            bids: vec![ClobLevel { price: "0.05".into(), size: "50".into() }],
            asks: vec![],
        };
        // Every level below the $0.091 floor → nothing filled.
        let w = walk_bids_for_sell(&book, 62.0, 0.091);
        assert_eq!(w.shares_filled, 0.0);
    }

    #[test]
    fn maker_sim_inside_wide_spread_gets_max_fill_rate() {
        // Wide spread: bid $0.20, ask $0.60 → mid $0.40. Markup 1.10 → ask $0.44.
        // $0.44 is WELL inside the [0.20, 0.60] spread → we become the new best
        // ask → full fill rate.
        let book = ClobBook {
            bids: vec![ClobLevel { price: "0.20".into(), size: "100".into() }],
            asks: vec![ClobLevel { price: "0.60".into(), size: "100".into() }],
        };
        let w = simulate_maker_dump(&book, 100.0, 1.10, 0.75, 0.091);
        assert!((w.avg_price - 0.44).abs() < 0.0001);
        assert!((w.shares_filled - 75.0).abs() < 0.0001); // 100 * 0.75
        assert!((w.gross_proceeds_usdc - 33.0).abs() < 0.0001); // 75 * 0.44
    }

    #[test]
    fn maker_sim_behind_wall_has_decayed_fill_rate() {
        // Tight spread: bid $0.38, ask $0.42 → mid $0.40. Markup 1.10 → ask $0.44.
        // $0.44 is 4.76% ABOVE best_ask $0.42 → behind the wall →
        // decay_t = 0.476 → fill_rate = 0.75*(1-0.476) + 0.375*0.476 ≈ 0.571
        let book = ClobBook {
            bids: vec![ClobLevel { price: "0.38".into(), size: "100".into() }],
            asks: vec![ClobLevel { price: "0.42".into(), size: "100".into() }],
        };
        let w = simulate_maker_dump(&book, 100.0, 1.10, 0.75, 0.091);
        assert!((w.avg_price - 0.44).abs() < 0.0001);
        assert!(w.shares_filled < 75.0 && w.shares_filled > 55.0);
        // Gross is less than the "inside-spread" case (33.0)
        assert!(w.gross_proceeds_usdc < 33.0);
    }

    #[test]
    fn maker_sim_far_behind_wall_floors_at_half_rate() {
        // Best ask $0.30, we post $0.48 → 60% above → deep in decay zone.
        // fill_rate should be at or below the floor (max_rate/2 = 0.375).
        let book = ClobBook {
            bids: vec![ClobLevel { price: "0.28".into(), size: "50".into() }],
            asks: vec![ClobLevel { price: "0.30".into(), size: "50".into() }],
        };
        // Force a high markup so we land far behind the wall.
        // mid = 0.29, markup 1.70 → ask 0.493 which is 64% above best ask.
        let w = simulate_maker_dump(&book, 100.0, 1.70, 0.75, 0.091);
        assert!((w.shares_filled - 37.5).abs() < 0.001); // 100 * 0.375
    }

    #[test]
    fn maker_sim_uses_mint_cost_floor_when_no_mid() {
        // Empty book → fair falls back to mint_cost_per_share ($0.091).
        // Ask = $0.091 × 1.10 = $0.1001. Empty asks → max fill rate.
        let book = ClobBook { bids: vec![], asks: vec![] };
        let w = simulate_maker_dump(&book, 100.0, 1.10, 0.75, 0.091);
        assert!((w.avg_price - 0.1001).abs() < 0.0001);
        assert!((w.shares_filled - 75.0).abs() < 0.0001);
    }

    #[test]
    fn maker_sim_uses_floor_when_mid_below_mint_cost() {
        // Mid = $0.05 < mint_cost_per_share $0.091 → use floor.
        // Our ask = 0.1001. Best ask in book is 0.06 → we're above →
        // fill rate decays. Verify ask price is correct regardless.
        let book = ClobBook {
            bids: vec![ClobLevel { price: "0.04".into(), size: "50".into() }],
            asks: vec![ClobLevel { price: "0.06".into(), size: "50".into() }],
        };
        let w = simulate_maker_dump(&book, 100.0, 1.10, 0.75, 0.091);
        assert!((w.avg_price - 0.1001).abs() < 0.0001);
    }

    #[test]
    fn dynamic_fill_rate_inside_spread() {
        let book = ClobBook {
            bids: vec![ClobLevel { price: "0.30".into(), size: "50".into() }],
            asks: vec![ClobLevel { price: "0.50".into(), size: "50".into() }],
        };
        // Our ask 0.44 is below best_ask 0.50 → full rate.
        assert!((dynamic_fill_rate(&book, 0.44, 0.80) - 0.80).abs() < 1e-9);
        // At the best ask → still full rate (boundary case).
        assert!((dynamic_fill_rate(&book, 0.50, 0.80) - 0.80).abs() < 1e-9);
    }

    #[test]
    fn dynamic_fill_rate_full_decay_at_10pct_above() {
        let book = ClobBook {
            bids: vec![],
            asks: vec![ClobLevel { price: "0.40".into(), size: "50".into() }],
        };
        // Ask at 0.44 → 10% above best ask → floor = 0.5 * max_rate.
        let r = dynamic_fill_rate(&book, 0.44, 0.80);
        assert!((r - 0.40).abs() < 1e-9);
        // Ask at 0.50 → 25% above → still clamped at floor.
        let r_far = dynamic_fill_rate(&book, 0.50, 0.80);
        assert!((r_far - 0.40).abs() < 1e-9);
    }

    #[test]
    fn paper_account_roi_math() {
        let mut acct = PaperAccount::new(1000.0);
        acct.bankroll_usdc = 1100.0;
        assert!((acct.net_pnl() - 100.0).abs() < 0.001);
        assert!((acct.roi_pct() - 10.0).abs() < 0.001);
    }
}
