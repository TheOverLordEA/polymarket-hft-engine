# PolyHFT: Institutional-Grade Polymarket Execution Engine

![Rust](https://img.shields.io/badge/Built_With-Rust-orange?style=flat-square&logo=rust)
![License](https://img.shields.io/badge/License-MIT-blue?style=flat-square)
![Status](https://img.shields.io/badge/Status-Production_Ready-green?style=flat-square)

**PolyHFT** is a high-frequency, asynchronous trading engine designed for Polymarket's 5-minute crypto markets.

This engine handles the "plumbing" so you can focus on the math. It solves the problems of market microstructure, state persistence, and RPC latency management out of the box.

---

## ⚡ Why This Engine Exists

Ready to use polymarket bot built with rust. I wasn't able to find any open source polymarket bot built with rust, so I decided to build one for myself. It works with polymarket CLOB and uses binance and coinbase for price feeds. It also has a built in risk management system that prevents you from losing money. The bot was built for 5 minute crypto markets and is optimized for speed and accuracy.

### 1. The Ultra-Low Latency Stack
This engine is built to maximize the speed of the **Rust + AWS `eu-west-1` + Alchemy + Binance/Coinbase** pipeline. 
- **Concurrent Execution:** Processes Binance, Coinbase, and Polymarket WebSocket streams simultaneously without thread blocking.
- **Tick-to-Trade Speed:** Evaluates entry matrices in microseconds using `libm::erf`. The entire loop from price tick to Polygon signature executes in under 5ms.
- **Memory Safety:** No garbage collection pauses.

### 2. Private RPC & Colocation Ready
- **Dedicated Nodes:** Native support for private Polygon RPC endpoints (Alchemy/QuickNode) via `.env` configuration. 
- **AWS Optimized:** Designed to run headless via `tmux` on `eu-west-1` (Ireland) or `us-east-1` (N. Virginia), minimizing network hops to Polymarket's matching engine and centralized exchange APIs.

### 3. Institutional Risk Management
- **De-Peg Killswitch:** Automatically halts trading if Binance and Coinbase prices diverge by >0.15% (protects against oracle failure).
- **Velocity Lockout:** Dynamic volatility monitoring prevents execution during "flash crash" candles where slippage is infinite.
- **Volatility Desert:** Automatically idles during low-volume regimes to prevent fee churn.
- **Drift Reconciliation:** A background loop that syncs local state with on-chain shares every 15s.

### 4. Telemetry & Live Monitoring
- **Real-Time Telegram Integration:** Built-in, asynchronous Telegram alerts. The bot fires off execution receipts, final PnL settlements, and killswitch warnings directly to your phone without blocking the main trading thread.

---

## 🛠 Architecture

The system is split into two binary crates for isolation:

1.  **`btc-5min-bot` / `eth-5min-bot`**: The specialized strategy runners.
2.  **`core_shared`**: The shared library handling WebSocket parsing, signing, and math.

### The "Clean Room" Strategy
This repository contains the **Execution Engine**. The proprietary entry/exit logic (Tiers 1, 2, and 3) has been scrubbed to provide a clean canvas for your strategy.

**You are responsible for implementing the logic inside:**
`src/strategy.rs` -> `execute_tick()`

```rust
// EXAMPLE SLOT FOR YOUR STRATEGY
// The engine provides:
// - gap: divergence from strike
// - time_to_expiry: seconds left
// - active_dvol: current volatility

if gap > (binance_price * 0.005) && time_to_expiry <= 20 {
    // The engine handles the signing, nonce management, and connection
    self.execute_buy(market, mkt_price).await;
}
```

🚀 Quick Start
Prerequisites

    Rust (Cargo)

    A Polygon RPC URL (Alchemy Private Node recommended)

    A Polymarket Proxy Wallet (Relayer)

Setup

    Clone the Repo
    Bash

    git clone https://github.com/TheOverLordEA/poly-hft-engine.git
    cd poly-hft-engine

    Configure Environment
    Create a .env file in the bot directory:
    Bash

    PRIVATE_KEY=your_polygon_private_key
    POLYGON_RPC_URL=https://polygon-mainnet.g.alchemy.com/v2/YOUR_KEY
    TELEGRAM_BOT_TOKEN=optional_for_alerts
    TELEGRAM_CHAT_ID=optional_for_alerts

    Safe Testing Logic
    The engine comes with built-in testing modes so you don't burn capital while tuning your strategy:

        Ghost Test: Run run_ghost_test() to verify your API keys and proxy wallet connection by firing a $0.10 order and instantly cancelling it.

        Simulation Mode: Set TRADING_ENABLED=false when starting the bot. It will track real-time WebSocket data, simulate fills, and output simulated PnL to the terminal and Telegram without signing live transactions.

    Build & Run
    Bash

    cargo build --release
    TRADING_ENABLED=true ./target/release/eth-5min-bot

💼 Consulting & Custom Integration

"I have the strategy, but I don't know Rust."

I built this engine. I know every line of the async loop, the WebSocket parsers, and the chain reconciliation logic.

If want a custom implementation with your strategy and vps:

   I can implement your proprietary strategy into this engine securely.

   I can help you with deploy & colocate your bot on AWS eu-west-1 or us-east-1 for maximum speed.

   I can help you with optimize the fee/spread math for your specific capital size

[Contact Me via DM] or open an Issue titled "Consulting Request".