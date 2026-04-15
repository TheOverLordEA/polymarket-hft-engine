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

            polygon_wss_url: "wss://polygon-rpc.com".to_string(),
            polygon_rpc_urls: vec!["https://polygon-rpc.com".to_string()],
            private_key: String::new(),
            funder_address: None,

            mint_amount_usdc: 10.0,
            min_dump_price: 0.08,
            dump_fraction: 0.95,
            daily_cap_usdc: 200.0,
            priority_fee_multiplier: 3.0,
            tx_gas_limit: 500_000,

            simulation: true,
            telegram_bot_token: None,
            telegram_chat_id: None,
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
    fn default_mint_size_is_ten() {
        assert_eq!(Config::default().mint_amount_usdc, 10.0);
    }

    #[test]
    fn default_daily_cap_is_two_hundred() {
        assert_eq!(Config::default().daily_cap_usdc, 200.0);
    }

    #[test]
    fn default_polygon_wss_is_public() {
        assert_eq!(Config::default().polygon_wss_url, "wss://polygon-rpc.com");
    }

    #[test]
    fn default_mint_to_dump_sanity() {
        let c = Config::default();
        assert!(c.dump_fraction > 0.0 && c.dump_fraction <= 1.0);
        assert!(c.min_dump_price > 0.0 && c.min_dump_price < 1.0);
    }
}
