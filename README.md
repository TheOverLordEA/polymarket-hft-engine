# PolyHFT Weather: Event-Driven Polymarket Weather-Market Bot

![Rust](https://img.shields.io/badge/Built_With-Rust-orange?style=flat-square&logo=rust)
![License](https://img.shields.io/badge/License-MIT-blue?style=flat-square)
![Status](https://img.shields.io/badge/Status-Sim_Mode-yellow?style=flat-square)

A high-frequency, event-driven Rust bot for Polymarket weather temperature markets. Detects newly-deployed markets on-chain, pre-signs dump orders before the mint tx is even broadcast, and flushes them the instant the mint lands.

The original PolyHFT codebase (5-minute BTC/ETH bot) has been replaced by this weather-specific implementation. See [git history](#history) for the pivot story.

---

## ⚡ Why this bot exists

Weather-market minting (buy $X of complete sets → dump profitable legs → hold tail legs) is currently the most profitable structural trade on Polymarket's daily temperature events. A deep-dive on the dominant competitor (`@IWantYourMoney`) showed:

- ~$600/day net on ~$5,000 working capital over an 8-day window
- ~90 events/day × ~$124 mint size ($62 SPLIT + $62 CONVERT)
- Median mint→dump lag ≈ 92 seconds, 10th percentile 22 seconds
- **Zero real CLOB buys** at the "$0.05" price points — the cost basis is a bookkeeping artifact of neg-risk conversion

Chasing those fills on the CLOB orderbook doesn't work — the liquidity isn't there. You have to mint and dump on the **same event** he's minting, as fast as possible. That's what this bot does.

---

## 🏗 Architecture

Three WebSocket streams feed a single `tokio::select!` loop. No polling anywhere on the hot path.

```
┌──────────────────────┐
│ Polygon WSS          │──┐
│  logs sub:           │  │   ┌─────────────┐
│  NegRiskAdapter      │  ├──▶│ WeatherEvent│──┐
│  MarketPrepared +    │  │   └─────────────┘  │
│  QuestionPrepared    │  │                    │
└──────────────────────┘  │                    ▼
┌──────────────────────┐  │   ┌─────────────┐  ┌──────────────────┐
│ Polygon WSS          │──┤──▶│ MintReceipt │─▶│ tokio::select!   │
│  newPendingTransactions  │   └─────────────┘  │   main event loop│
│  (own address filter)│  │                    │                  │
└──────────────────────┘  │                    │  • presign dumps │
┌──────────────────────┐  │                    │  • broadcast mint│
│ Polymarket CLOB WS   │──┘   ┌─────────────┐  │  • flush orders  │
│  market channel      │────▶│ ClobReady    │─▶│  • redeem        │
│  custom_feature=true │      └─────────────┘  └──────────────────┘
└──────────────────────┘
```

### Hot-path timeline for a new weather event

1. `NegRiskAdapter.MarketPrepared` fires on Polygon → `[onchain]` watcher reads log (~10–50 ms budget from WSS)
2. `QuestionPrepared` fires N times (once per bucket) → buckets accumulate keyed by `marketId`
3. Once the bucket set is complete, `ctf_math::neg_risk_bucket_token_ids` derives every ERC-1155 positionId **off-chain** from the `marketId` + question index (no RPC needed, verified byte-for-byte against live Lucknow data)
4. `WeatherEvent` is pushed to the main loop
5. In parallel: `presigner.presign_for_event()` builds a sell order per bucket; `mint_executor.mint_event()` broadcasts the `splitPosition` tx to every configured RPC
6. When `[mempool]` watcher sees our own tx hash (or `[clob_ws]` sees `new_market`), the `[dump]` path flushes every cached order via `POST /order`
7. Tail legs held through resolution and redeemed via `NegRiskAdapter.redeemPositions`

---

## 🔒 Safety rails

- **Simulation mode is the default.** The bot refuses to go live unless `SIMULATION=false` AND `PRIVATE_KEY` is set. Sim mode runs the full pipeline (WSS subs, presigning, tx construction) and logs what would happen, without broadcasting anything.
- **Daily USDC cap.** `DAILY_CAP_USDC` enforces a hard 24h limit. Every mint checks against `state.daily_minted_usdc` before firing; breach → skip + log.
- **Weather-only filter.** Two gates: slug regex (`highest-temperature-in-{city}-on-{month}-{day}-{year}`) and oracle allowlist (UmaCtfAdapter + NegRiskUmaCtfAdapter). Non-weather markets are dropped at the watcher before the executor ever sees them.
- **Graceful shutdown.** `ctrl-c` persists state to `bot_state.json` before exit.

---

## 🧮 NegRiskIdLib port — the tricky part

The bot derives Polymarket's ERC-1155 `positionId` for every bucket **off-chain**, so pre-signing can happen the instant an on-chain event fires (no eth_call round-trips). Gnosis CTF's `getCollectionId` is not a plain keccak — it maps `(conditionId, indexSet)` onto the alt_bn128 curve, encoding point compression in the top bits. The port lives in [`weather-bot/src/ctf_math.rs`](weather-bot/src/ctf_math.rs).

Verification tests in `ctf_math::tests` pin the port against live gamma data for the Lucknow April 15 2026 event (buckets 0, 1, 3, 9 — both `conditionId` and YES/NO `positionId` match byte-for-byte). **If you edit the math and those tests fail, the bot will compute wrong token IDs — fix the code, don't weaken the tests.**

---

## 🚀 Quick start (sim mode)

```bash
git clone https://github.com/TheOverLordEA/polymarket-hft-engine.git
cd polymarket-hft-engine
cp weather-bot/.env.example weather-bot/.env   # edit as needed
cargo run -p weather-bot
```

Default `.env` is enough for sim mode — no private key required. You'll see:

```
[CONFIG] mode=SIMULATION mint=$10.00 daily_cap=$200 min_dump=$0.080 dump_frac=0.95
[CONFIG] polygon_wss=wss://polygon-rpc.com
[CONFIG] clob_ws=wss://ws-subscriptions-clob.polymarket.com/ws/market
[onchain] connecting to wss://polygon-rpc.com
[onchain] subscribed to NegRiskAdapter MarketPrepared + QuestionPrepared
[MAIN] event loop ready — waiting for on-chain weather events
```

## 📋 Going live (checklist)

The live mint path is scaffolded but not wired. To complete it:

1. **Finish `mint_executor::mint_event_live`** — build `TxEip1559` via `alloy-consensus`, sign with `LocalSigner`, RLP-encode, POST `eth_sendRawTransaction` to all configured RPCs in parallel. ~150 lines.
2. **Wire gamma slug lookup in `watchers/onchain.rs::emit_weather_event`** — currently emits events with placeholder `city` / `resolution_date` / `bucket_label`. The weather filter depends on the slug being populated.
3. **Replace `watchers/mempool.rs` with Alchemy-specific `alchemy_pendingTransactions`** — public Polygon nodes don't filter pending txs server-side, so the current implementation would drown in ~500 tx/s of noise.
4. **Cache EIP-712 signed bytes** in `presigner.rs` — currently the signing happens at flush time via the SDK's `limit_order().build()` path. Ideal: sign at presign time so flush is a pure POST.
5. **Pair mint receipts back to conditionIds** — `main.rs` receives `MintReceipt` but doesn't yet use it to key the dump flush. Needs a slug→condition_ids map populated at mint time.

Each is a bounded follow-up. Baseline: 46/46 tests passing, full stack compiles clean.

---

## 🛠 Repo layout

```
core-shared/           — shared types (OrderBook helpers, Trade, Market enum)
weather-bot/
├── Cargo.toml
├── .env.example
└── src/
    ├── main.rs              — tokio::select! event loop
    ├── config.rs            — env + config.json loader
    ├── state.rs             — persistent bot state + daily cap
    ├── types.rs             — WeatherEvent / BucketInfo / MintReceipt / channel aliases
    ├── ctf_math.rs          — NegRiskIdLib + alt_bn128 getCollectionId (verified vs gamma)
    ├── weather_filter.rs    — slug regex + oracle allowlist
    ├── scanner.rs           — slug parsing utilities
    ├── executor.rs          — Polymarket CLOB auth + signed limit orders
    ├── mint_executor.rs     — NegRiskAdapter splitPosition tx builder
    ├── presigner.rs         — per-event signed-order cache
    ├── alerts.rs            — Telegram push
    └── watchers/
        ├── mod.rs
        ├── onchain.rs       — Polygon WSS eth_subscribe("logs", …)
        ├── clob_ws.rs       — Polymarket CLOB WSS with custom_feature_enabled
        └── mempool.rs       — own pending-tx subscription
```

---

## ⚙️ Configuration

All config is env-driven. See [`weather-bot/.env.example`](weather-bot/.env.example) for the full list. Key knobs:

| Var | Default | Notes |
|---|---|---|
| `SIMULATION` | `true` | **Must be explicitly set to `false` to go live.** |
| `PRIVATE_KEY` | _(unset)_ | Hex EOA key. CLOB L2 creds are derived from this at startup. |
| `FUNDER_ADDRESS` | _(unset)_ | Set only if you use a Gnosis Safe proxy wallet. |
| `POLYGON_WSS_URL` | `wss://polygon-rpc.com` | Public is fine for sim. Use Alchemy/QuickNode for live HFT. |
| `POLYGON_RPC_URLS` | `polygon-rpc.com,llamarpc.com` | Comma-separated HTTPS RPCs for parallel tx broadcast. |
| `CLOB_ANCHOR_ASSET_ID` | _(unset)_ | Stable token ID to piggyback `custom_feature_enabled`. Disabled if unset. |
| `MINT_AMOUNT_USDC` | `10.0` | Per-event mint size. |
| `MIN_DUMP_PRICE` | `0.08` | Skip dumping legs below this. |
| `DUMP_FRACTION` | `0.95` | Fraction of minted shares to dump (keep 5% as tail). |
| `DAILY_CAP_USDC` | `200.0` | Hard 24h safety rail. |
| `TELEGRAM_BOT_TOKEN` | _(unset)_ | Optional alerts. |
| `TELEGRAM_CHAT_ID` | _(unset)_ | Optional alerts. |

---

## 🧪 Tests

```bash
cargo test -p weather-bot
```

Expected: **46 passed**. If the `ctf_math::tests::position_ids_match_gamma_*` cases break, the NegRiskIdLib port is wrong — do not proceed. Fix the math, rerun.

---

## 📜 History

This repo originally shipped as the BTC/ETH 5-minute PolyHFT engine. After a competitor deep-dive showed weather markets offered better risk-adjusted return for small capital, the 5-minute bots were removed and replaced with this event-driven weather pipeline. The old `btc-5min-bot` / `eth-5min-bot` crates are available in git history before commit `4b913cb`.

## ⚖️ Disclaimer

Experimental. Not financial advice. Don't run live without the follow-ups above and without sim-mode validation against real traffic. The author is not responsible for lost funds, failed trades, or RPC bills.
