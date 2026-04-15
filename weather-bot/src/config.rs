use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    // -------- Polymarket endpoints --------
    pub clob_api_url: String,
    pub gamma_api_url: String,
    /// `wss://ws-subscriptions-clob.polymarket.com/ws/market`
    pub clob_ws_market_url: String,
    /// `wss://ws-subscriptions-clob.polymarket.com/ws/user`
    pub clob_ws_user_url: String,
    /// A stable token ID to piggyback `custom_feature_enabled: true` on, so we
    /// receive the globally-broadcast `new_market` messages. Any long-running
    /// active token works.
    pub clob_anchor_asset_id: String,

    // -------- Polygon on-chain --------
    /// Polygon WSS URL for `eth_subscribe("logs", ...)` and `newPendingTransactions`.
    /// Default is the public `wss://polygon-rpc.com` — fine for sim/dev, switch
    /// to Alchemy/QuickNode for live HFT.
    pub polygon_wss_url: String,
    /// HTTP RPCs for parallel tx broadcast. Hitting >1 endpoint improves
    /// propagation to block producers.
    pub polygon_rpc_urls: Vec<String>,
    /// Hex private key, NO `0x` prefix. Loaded from `PRIVATE_KEY` env var.
    pub private_key: String,
    /// Proxy wallet address for Gnosis Safe signature type (optional).
    pub funder_address: Option<String>,

    // -------- HFT strategy params --------
    /// USDC per mint. Default 10.0 (user's calibrated size).
    pub mint_amount_usdc: f64,
    /// Skip dumping any leg whose estimated bid is below this.
    pub min_dump_price: f64,
    /// Fraction of minted shares to dump (0.95 = keep 5% as tail lottery).
    pub dump_fraction: f64,
    /// Daily safety rail — total USDC minted per 24h.
    pub daily_cap_usdc: f64,
    /// Priority-fee multiplier over base fee. 3.0 is aggressive.
    pub priority_fee_multiplier: f64,
    /// Mint transaction gas limit.
    pub tx_gas_limit: u64,

    // -------- Mode + telemetry --------
    pub simulation: bool,
    pub telegram_bot_token: Option<String>,
    pub telegram_chat_id: Option<String>,

    // -------- Paper-trading mode --------
    /// When true, the bot runs through the PaperEngine instead of the
    /// live mint executor: real orderbooks, real fees, virtual bankroll.
    /// Can only be true if `simulation == true` (paper implies sim).
    pub paper_mode: bool,
    /// Virtual USDC bankroll to start the paper run with.
    pub paper_starting_bankroll: f64,
    /// Max simultaneously-open events — paper run skips new mints once full.
    pub paper_max_concurrent_events: usize,
    /// Where to append the structured CSV log.
    pub paper_log_path: String,
    /// "taker" (walk bid book) or "maker" (simulate ask at markup).
    /// Default "taker" — the pessimistic floor.
    pub paper_dump_strategy: String,
    /// When `paper_dump_strategy == "maker"`, the ask is priced at
    /// `max(clob_mid, mint_cost) * maker_markup`. 1.10 = 10% above fair.
    pub paper_maker_markup: f64,
    /// When `paper_dump_strategy == "maker"`, fraction of the target
    /// shares we assume the maker ask fills before expiry. 0.75 is a
    /// loose calibration; v2 should replace with an empirical model.
    pub paper_maker_fill_rate: f64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            clob_api_url: "https://clob.polymarket.com".to_string(),
            gamma_api_url: "https://gamma-api.polymarket.com".to_string(),
            clob_ws_market_url: "wss://ws-subscriptions-clob.polymarket.com/ws/market"
                .to_string(),
            clob_ws_user_url: "wss://ws-subscriptions-clob.polymarket.com/ws/user".to_string(),
            clob_anchor_asset_id: String::new(),

            // wss://polygon-rpc.com does NOT serve WebSocket (returns HTML
            // on upgrade). wss://1rpc.io/matic responds with HTTP 101 and
            // honors eth_subscribe — fine for paper/dev. Live HFT should
            // use Alchemy or QuickNode.
            polygon_wss_url: "wss://1rpc.io/matic".to_string(),
            polygon_rpc_urls: vec!["https://polygon-rpc.com".to_string()],
            private_key: String::new(),
            funder_address: None,

            // $62 matches the competitor's production sizing: above the
            // rewardsMinSize=50 floor (24% buffer), and with 8 concurrent
            // events against a $1000 bankroll it's $62 * 2 ops * 8 = $992
            // working capital, a tight but viable fit. Was $10 as a
            // placeholder from an earlier scoping conversation; that value
            // is BELOW the rewards floor and produces self-consistent but
            // off-strategy paper numbers. Do not lower unless you
            // explicitly want pre-rewards edge measurements.
            mint_amount_usdc: 62.0,
            min_dump_price: 0.08,
            dump_fraction: 0.95,
            daily_cap_usdc: 200.0,
            priority_fee_multiplier: 3.0,
            tx_gas_limit: 500_000,

            simulation: true,
            telegram_bot_token: None,
            telegram_chat_id: None,

            paper_mode: true,
            paper_starting_bankroll: 1000.0,
            paper_max_concurrent_events: 8,
            paper_log_path: "paper_run.log".to_string(),
            paper_dump_strategy: "taker".to_string(),
            paper_maker_markup: 1.10,
            paper_maker_fill_rate: 0.75,
        }
    }
}

impl Config {
    pub fn load() -> Self {
        dotenv::dotenv().ok();

        let mut cfg = if Path::new("config.json").exists() {
            fs::read_to_string("config.json")
                .ok()
                .and_then(|d| serde_json::from_str(&d).ok())
                .unwrap_or_default()
        } else {
            Self::default()
        };

        // --- required creds ---
        if let Ok(key) = std::env::var("PRIVATE_KEY") {
            cfg.private_key = key.trim_start_matches("0x").to_string();
        }
        if let Ok(funder) = std::env::var("FUNDER_ADDRESS") {
            cfg.funder_address = Some(funder);
        }

        // --- on-chain endpoints ---
        if let Ok(wss) = std::env::var("POLYGON_WSS_URL") {
            cfg.polygon_wss_url = wss;
        }
        if let Ok(rpcs) = std::env::var("POLYGON_RPC_URLS") {
            cfg.polygon_rpc_urls = rpcs.split(',').map(|s| s.trim().to_string()).collect();
        } else if let Ok(rpc) = std::env::var("POLYGON_RPC_URL") {
            cfg.polygon_rpc_urls = vec![rpc];
        }

        // --- CLOB WS anchor ---
        if let Ok(anchor) = std::env::var("CLOB_ANCHOR_ASSET_ID") {
            cfg.clob_anchor_asset_id = anchor;
        }

        // --- strategy knobs ---
        if let Ok(v) = std::env::var("MINT_AMOUNT_USDC") {
            if let Ok(f) = v.parse() {
                cfg.mint_amount_usdc = f;
            }
        }
        if let Ok(v) = std::env::var("MIN_DUMP_PRICE") {
            if let Ok(f) = v.parse() {
                cfg.min_dump_price = f;
            }
        }
        if let Ok(v) = std::env::var("DUMP_FRACTION") {
            if let Ok(f) = v.parse() {
                cfg.dump_fraction = f;
            }
        }
        if let Ok(v) = std::env::var("DAILY_CAP_USDC") {
            if let Ok(f) = v.parse() {
                cfg.daily_cap_usdc = f;
            }
        }

        // --- telemetry ---
        if let Ok(token) = std::env::var("TELEGRAM_BOT_TOKEN") {
            cfg.telegram_bot_token = Some(token);
        }
        if let Ok(chat) = std::env::var("TELEGRAM_CHAT_ID") {
            cfg.telegram_chat_id = Some(chat);
        }
        if let Ok(sim) = std::env::var("SIMULATION") {
            cfg.simulation = sim == "true" || sim == "1";
        }

        // --- paper mode ---
        if let Ok(v) = std::env::var("PAPER_MODE") {
            cfg.paper_mode = v == "true" || v == "1";
        }
        if let Ok(v) = std::env::var("PAPER_STARTING_BANKROLL") {
            if let Ok(f) = v.parse() {
                cfg.paper_starting_bankroll = f;
            }
        }
        if let Ok(v) = std::env::var("PAPER_MAX_CONCURRENT_EVENTS") {
            if let Ok(n) = v.parse() {
                cfg.paper_max_concurrent_events = n;
            }
        }
        if let Ok(v) = std::env::var("PAPER_LOG_PATH") {
            cfg.paper_log_path = v;
        }
        if let Ok(v) = std::env::var("PAPER_DUMP_STRATEGY") {
            let lower = v.to_lowercase();
            if lower == "taker" || lower == "maker" {
                cfg.paper_dump_strategy = lower;
            }
        }
        if let Ok(v) = std::env::var("PAPER_MAKER_MARKUP") {
            if let Ok(f) = v.parse() {
                cfg.paper_maker_markup = f;
            }
        }
        if let Ok(v) = std::env::var("PAPER_MAKER_FILL_RATE") {
            if let Ok(f) = v.parse() {
                cfg.paper_maker_fill_rate = f;
            }
        }

        // Invariant: paper_mode forces simulation
        if cfg.paper_mode {
            cfg.simulation = true;
        }

        cfg
    }

    pub fn save(&self) {
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = fs::write("config.json", json);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_sim_mode_on() {
        let c = Config::default();
        assert!(c.simulation);
    }

    #[test]
    fn default_mint_size_matches_competitor() {
        // $62 is @IWantYourMoney's production sizing — above the
        // rewardsMinSize=50 eligibility floor.
        assert_eq!(Config::default().mint_amount_usdc, 62.0);
    }

    #[test]
    fn default_daily_cap_is_two_hundred() {
        assert_eq!(Config::default().daily_cap_usdc, 200.0);
    }

    #[test]
    fn default_polygon_wss_supports_ws_upgrade() {
        // wss://polygon-rpc.com returns HTML on WS upgrade (not a real
        // WSS endpoint). 1rpc.io/matic does the handshake but rejects
        // eth_subscribe — good enough for sim/dev. Live HFT needs
        // Alchemy or QuickNode.
        let url = Config::default().polygon_wss_url;
        assert!(url.starts_with("wss://"));
        assert!(url != "wss://polygon-rpc.com");
    }

    #[test]
    fn default_mint_to_dump_sanity() {
        let c = Config::default();
        assert!(c.dump_fraction > 0.0 && c.dump_fraction <= 1.0);
        assert!(c.min_dump_price > 0.0 && c.min_dump_price < 1.0);
    }
}
