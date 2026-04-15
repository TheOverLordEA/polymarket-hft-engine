use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum PositionStatus {
    /// Bought, waiting for price to hit sell target or resolution
    Active,
    /// Sell order placed, waiting for confirmation
    PendingSell,
    /// Sold for profit
    SoldForProfit,
    /// Expired / resolved worthless
    Expired,
    /// Resolved at $1 (winner!)
    ResolvedWinner,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    /// Polymarket condition token ID
    pub token_id: String,
    /// Market slug (e.g. "highest-temperature-in-lucknow-on-april-15-2026")
    pub market_slug: String,
    /// City name
    pub city: String,
    /// Outcome label (e.g. "45°C")
    pub outcome_label: String,
    /// Resolution date (YYYY-MM-DD)
    pub resolution_date: String,
    /// Entry price per share
    pub entry_price: f64,
    /// Number of shares bought
    pub shares: f64,
    /// Total cost (entry_price * shares)
    pub cost_basis: f64,
    /// Current market price
    pub current_price: f64,
    /// Position status
    pub status: PositionStatus,
    /// Timestamp of purchase (unix)
    pub bought_at: i64,
    /// Realized PnL if sold/resolved
    pub realized_pnl: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BotState {
    /// All positions (active + historical)
    pub positions: Vec<Position>,
    /// Markets already scanned (slug -> timestamp), to avoid re-buying
    pub known_markets: HashMap<String, i64>,
    /// Running USDC balance (for simulation)
    pub sim_balance: f64,
    /// Cumulative realized PnL
    pub cumulative_pnl: f64,
    /// Total trades executed
    pub total_trades: u64,
    /// Total winners
    pub total_winners: u64,
    /// USDC minted today (safety rail against runaway mint loops).
    /// Reset every 24h by `check_and_reset_daily_cap`.
    #[serde(default)]
    pub daily_minted_usdc: f64,
    /// Unix seconds when the daily-cap window started.
    #[serde(default)]
    pub daily_cap_reset_ts: i64,
}

impl Default for BotState {
    fn default() -> Self {
        Self {
            positions: Vec::new(),
            known_markets: HashMap::new(),
            sim_balance: 100.0,
            cumulative_pnl: 0.0,
            total_trades: 0,
            total_winners: 0,
            daily_minted_usdc: 0.0,
            daily_cap_reset_ts: 0,
        }
    }
}

impl BotState {
    /// Zero the daily-mint counter if more than 24h have elapsed since the
    /// last reset. Safe to call on every event — cheap and idempotent.
    pub fn check_and_reset_daily_cap(&mut self) {
        let now = chrono::Utc::now().timestamp();
        if now - self.daily_cap_reset_ts >= 86_400 {
            if self.daily_minted_usdc > 0.0 {
                tracing::info!(
                    "[DAILY-CAP] rolling window reset: was ${:.2}",
                    self.daily_minted_usdc
                );
            }
            self.daily_minted_usdc = 0.0;
            self.daily_cap_reset_ts = now;
        }
    }
}

impl BotState {
    pub fn load() -> Self {
        if Path::new("bot_state.json").exists() {
            if let Ok(data) = fs::read_to_string("bot_state.json") {
                if let Ok(state) = serde_json::from_str(&data) {
                    tracing::info!("[STATE] Loaded persistent state from bot_state.json");
                    return state;
                }
            }
        }
        tracing::info!("[STATE] No previous state found. Starting fresh.");
        Self::default()
    }

    pub fn save(&self) {
        if let Ok(json) = serde_json::to_string_pretty(self) {
            if let Err(e) = fs::write("bot_state.json", json) {
                tracing::error!("[STATE] Failed to save: {}", e);
            }
        }
    }

    pub fn active_positions(&self) -> Vec<&Position> {
        self.positions
            .iter()
            .filter(|p| p.status == PositionStatus::Active || p.status == PositionStatus::PendingSell)
            .collect()
    }

    pub fn has_position_in_market(&self, market_slug: &str) -> bool {
        self.positions
            .iter()
            .any(|p| p.market_slug == market_slug && p.status == PositionStatus::Active)
    }

    pub fn record_buy(&mut self, position: Position) {
        self.total_trades += 1;
        self.positions.push(position);
        self.save();
    }

    pub fn record_sell(&mut self, token_id: &str, sell_price: f64) {
        if let Some(pos) = self.positions.iter_mut().find(|p| p.token_id == token_id && p.status == PositionStatus::Active) {
            let pnl = (sell_price - pos.entry_price) * pos.shares;
            pos.realized_pnl = pnl;
            pos.status = PositionStatus::SoldForProfit;
            self.cumulative_pnl += pnl;
            if pnl > 0.0 {
                self.total_winners += 1;
            }
            self.save();
        }
    }

    pub fn record_resolution(&mut self, token_id: &str, resolved_yes: bool) {
        if let Some(pos) = self.positions.iter_mut().find(|p| p.token_id == token_id && p.status == PositionStatus::Active) {
            if resolved_yes {
                let pnl = (1.0 - pos.entry_price) * pos.shares;
                pos.realized_pnl = pnl;
                pos.status = PositionStatus::ResolvedWinner;
                self.cumulative_pnl += pnl;
                self.total_winners += 1;
            } else {
                pos.realized_pnl = -pos.cost_basis;
                pos.status = PositionStatus::Expired;
                self.cumulative_pnl -= pos.cost_basis;
            }
            self.save();
        }
    }

    pub fn summary(&self) -> String {
        let active = self.active_positions();
        let total_cost: f64 = active.iter().map(|p| p.cost_basis).sum();
        let total_current: f64 = active.iter().map(|p| p.current_price * p.shares).sum();
        format!(
            "Positions: {} active | Deployed: ${:.2} | Current Value: ${:.2} | Realized PnL: ${:.2} | Trades: {} | Winners: {} | Win Rate: {:.1}%",
            active.len(),
            total_cost,
            total_current,
            self.cumulative_pnl,
            self.total_trades,
            self.total_winners,
            if self.total_trades > 0 { (self.total_winners as f64 / self.total_trades as f64) * 100.0 } else { 0.0 }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_position(token_id: &str, city: &str, label: &str, entry_price: f64, shares: f64) -> Position {
        Position {
            token_id: token_id.to_string(),
            market_slug: format!("test-market-{}", city),
            city: city.to_string(),
            outcome_label: label.to_string(),
            resolution_date: "2026-04-15".to_string(),
            entry_price,
            shares,
            cost_basis: entry_price * shares,
            current_price: entry_price,
            status: PositionStatus::Active,
            bought_at: 1000000,
            realized_pnl: 0.0,
        }
    }

    #[test]
    fn test_default_state() {
        let state = BotState::default();
        assert!(state.positions.is_empty());
        assert!(state.known_markets.is_empty());
        assert_eq!(state.sim_balance, 100.0);
        assert_eq!(state.cumulative_pnl, 0.0);
        assert_eq!(state.total_trades, 0);
        assert_eq!(state.total_winners, 0);
    }

    #[test]
    fn test_record_buy() {
        let mut state = BotState::default();
        let pos = make_position("tok1", "lucknow", "45°C", 0.05, 100.0);
        state.record_buy(pos);

        assert_eq!(state.positions.len(), 1);
        assert_eq!(state.total_trades, 1);
        assert_eq!(state.positions[0].token_id, "tok1");
        assert_eq!(state.positions[0].entry_price, 0.05);
        assert_eq!(state.positions[0].shares, 100.0);
        assert_eq!(state.positions[0].cost_basis, 5.0);
    }

    #[test]
    fn test_record_sell_profit() {
        let mut state = BotState::default();
        let pos = make_position("tok1", "lucknow", "45°C", 0.05, 100.0);
        state.record_buy(pos);

        state.record_sell("tok1", 0.10);

        assert_eq!(state.positions[0].status, PositionStatus::SoldForProfit);
        assert!((state.positions[0].realized_pnl - 5.0).abs() < 0.001); // (0.10 - 0.05) * 100
        assert_eq!(state.total_winners, 1);
        assert!((state.cumulative_pnl - 5.0).abs() < 0.001);
    }

    #[test]
    fn test_record_sell_no_match() {
        let mut state = BotState::default();
        let pos = make_position("tok1", "lucknow", "45°C", 0.05, 100.0);
        state.record_buy(pos);

        // Selling a non-existent token should do nothing
        state.record_sell("tok_nonexistent", 0.10);
        assert_eq!(state.positions[0].status, PositionStatus::Active);
        assert_eq!(state.total_winners, 0);
    }

    #[test]
    fn test_record_resolution_winner() {
        let mut state = BotState::default();
        let pos = make_position("tok1", "lucknow", "45°C", 0.05, 100.0);
        state.record_buy(pos);

        state.record_resolution("tok1", true);

        assert_eq!(state.positions[0].status, PositionStatus::ResolvedWinner);
        // PnL = (1.0 - 0.05) * 100 = 95.0
        assert!((state.positions[0].realized_pnl - 95.0).abs() < 0.001);
        assert_eq!(state.total_winners, 1);
        assert!((state.cumulative_pnl - 95.0).abs() < 0.001);
    }

    #[test]
    fn test_record_resolution_loser() {
        let mut state = BotState::default();
        let pos = make_position("tok1", "lucknow", "45°C", 0.05, 100.0);
        state.record_buy(pos);

        state.record_resolution("tok1", false);

        assert_eq!(state.positions[0].status, PositionStatus::Expired);
        assert!((state.positions[0].realized_pnl - (-5.0)).abs() < 0.001);
        assert!((state.cumulative_pnl - (-5.0)).abs() < 0.001);
        assert_eq!(state.total_winners, 0);
    }

    #[test]
    fn test_active_positions() {
        let mut state = BotState::default();
        state.record_buy(make_position("tok1", "lucknow", "45°C", 0.05, 100.0));
        state.record_buy(make_position("tok2", "lucknow", "47°C", 0.03, 150.0));
        state.record_buy(make_position("tok3", "tokyo", "35°C", 0.04, 120.0));

        assert_eq!(state.active_positions().len(), 3);

        // Sell one
        state.record_sell("tok1", 0.10);
        assert_eq!(state.active_positions().len(), 2);

        // Resolve one
        state.record_resolution("tok2", false);
        assert_eq!(state.active_positions().len(), 1);
    }

    #[test]
    fn test_has_position_in_market() {
        let mut state = BotState::default();
        let pos = make_position("tok1", "lucknow", "45°C", 0.05, 100.0);
        state.record_buy(pos);

        assert!(state.has_position_in_market("test-market-lucknow"));
        assert!(!state.has_position_in_market("test-market-tokyo"));
    }

    #[test]
    fn test_multiple_trades_win_rate() {
        let mut state = BotState::default();

        // Buy 4 positions
        state.record_buy(make_position("tok1", "a", "45°C", 0.05, 100.0));
        state.record_buy(make_position("tok2", "b", "46°C", 0.04, 100.0));
        state.record_buy(make_position("tok3", "c", "47°C", 0.03, 100.0));
        state.record_buy(make_position("tok4", "d", "48°C", 0.02, 100.0));

        // 1 winner, 3 losers
        state.record_resolution("tok1", true);
        state.record_resolution("tok2", false);
        state.record_resolution("tok3", false);
        state.record_resolution("tok4", false);

        assert_eq!(state.total_trades, 4);
        assert_eq!(state.total_winners, 1);

        let summary = state.summary();
        assert!(summary.contains("Win Rate: 25.0%"));
    }

    #[test]
    fn test_summary_empty() {
        let state = BotState::default();
        let summary = state.summary();
        assert!(summary.contains("0 active"));
        assert!(summary.contains("Win Rate: 0.0%"));
    }

    #[test]
    fn test_state_serialization_roundtrip() {
        let mut state = BotState::default();
        state.record_buy(make_position("tok1", "lucknow", "45°C", 0.05, 100.0));
        state.known_markets.insert("test-slug".to_string(), 123456);

        let json = serde_json::to_string(&state).unwrap();
        let deserialized: BotState = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.positions.len(), 1);
        assert_eq!(deserialized.total_trades, 1);
        assert!(deserialized.known_markets.contains_key("test-slug"));
    }
}
