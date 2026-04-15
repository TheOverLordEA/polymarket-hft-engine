mod alerts;
mod config;
mod ctf_math;
mod executor;
mod mint_executor;
mod paper;
mod presigner;
mod scanner;
mod state;
mod types;
mod watchers;
mod weather_filter;

use crate::alerts::send_alert;
use crate::config::Config;
use crate::executor::Executor;
use crate::mint_executor::MintExecutor;
use crate::paper::PaperEngine;
use crate::presigner::{OrderTemplate, Presigner};
use crate::state::BotState;
use crate::types::{
    ClobMarketReady, MintReceipt, WeatherEvent,
};
use alloy_primitives::Address;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::signal;
use tokio::sync::{mpsc, Mutex};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("weather_bot=info")
        .init();

    tracing::info!("===============================================");
    tracing::info!("  Polymarket Weather HFT — Mint + Dump Pipeline ");
    tracing::info!("===============================================");

    let config = Arc::new(Config::load());
    let state = Arc::new(Mutex::new(BotState::load()));

    if config.private_key.is_empty() && !config.simulation {
        tracing::error!("PRIVATE_KEY not set and SIMULATION=false — refusing to start");
        std::process::exit(1);
    }

    let mode_label = if config.paper_mode {
        "PAPER"
    } else if config.simulation {
        "SIMULATION"
    } else {
        "LIVE"
    };
    tracing::info!(
        "[CONFIG] mode={} mint=${:.2} daily_cap=${:.0} min_dump=${:.3} dump_frac={:.2}",
        mode_label,
        config.mint_amount_usdc,
        config.daily_cap_usdc,
        config.min_dump_price,
        config.dump_fraction,
    );
    tracing::info!("[CONFIG] polygon_wss={}", config.polygon_wss_url);
    tracing::info!("[CONFIG] clob_ws={}", config.clob_ws_market_url);
    if config.paper_mode {
        tracing::info!(
            "[CONFIG] paper: bankroll=${:.0} max_concurrent={} log={}",
            config.paper_starting_bankroll,
            config.paper_max_concurrent_events,
            config.paper_log_path,
        );
    }

    // --- Executors ---
    let executor = Arc::new(
        Executor::new((*config).clone())
            .await
            .map_err(|e| {
                tracing::error!("[EXEC] init failed: {}", e);
                e
            })?,
    );
    let mint_exec = Arc::new(MintExecutor::new(config.clone()));
    let presigner = Arc::new(Presigner::new(
        executor.clone(),
        OrderTemplate {
            min_dump_price: config.min_dump_price,
            dump_fraction: config.dump_fraction,
            mint_amount_usdc: config.mint_amount_usdc,
        },
    ));
    // Paper engine is only instantiated when paper_mode is on. When enabled
    // it shadows mint_exec + presigner entirely — we take the PaperEngine
    // branch in the main select arm below and skip the live executors.
    let paper_engine: Option<Arc<PaperEngine>> = if config.paper_mode {
        Some(Arc::new(PaperEngine::new(
            config.clob_api_url.clone(),
            config.paper_starting_bankroll,
            config.paper_max_concurrent_events,
            config.mint_amount_usdc,
            config.min_dump_price,
            config.dump_fraction,
            PathBuf::from(&config.paper_log_path),
        )))
    } else {
        None
    };

    send_alert(&config, "🌡️ Weather HFT bot started").await;

    // --- Event channels ---
    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<WeatherEvent>();
    let (mint_tx, mut mint_rx) = mpsc::unbounded_channel::<MintReceipt>();
    let (ready_tx, mut ready_rx) = mpsc::unbounded_channel::<ClobMarketReady>();

    // --- Spawn watchers ---
    let wss = config.polygon_wss_url.clone();
    let event_tx_cl = event_tx.clone();
    tokio::spawn(async move {
        if let Err(e) = watchers::onchain::run_onchain_watcher(wss, event_tx_cl).await {
            tracing::error!("[onchain] fatal: {}", e);
        }
    });

    let clob_wss = config.clob_ws_market_url.clone();
    let anchor = config.clob_anchor_asset_id.clone();
    let ready_tx_cl = ready_tx.clone();
    tokio::spawn(async move {
        if let Err(e) =
            watchers::clob_ws::run_clob_market_watcher(clob_wss, anchor, ready_tx_cl).await
        {
            tracing::error!("[clob_ws] fatal: {}", e);
        }
    });

    // Mempool watcher (live only — skip in sim to avoid noisy public-node traffic)
    if !config.simulation {
        let wss = config.polygon_wss_url.clone();
        let addr = derive_signer_address(&config.private_key).unwrap_or(Address::ZERO);
        let mint_tx_cl = mint_tx.clone();
        tokio::spawn(async move {
            if let Err(e) = watchers::mempool::run_mempool_watcher(wss, addr, mint_tx_cl).await {
                tracing::error!("[mempool] fatal: {}", e);
            }
        });
    }

    tracing::info!("[MAIN] event loop ready — waiting for on-chain weather events");

    loop {
        tokio::select! {
            Some(event) = event_rx.recv() => {
                tracing::info!(
                    "[EVENT] {} buckets={} detected_at_ns={}",
                    event.event_slug,
                    event.buckets.len(),
                    event.detected_at_ns,
                );

                // ==== PAPER MODE SHORT-CIRCUIT ====
                // When paper_mode is on, the PaperEngine owns the whole
                // event lifecycle (bankroll check, virtual mint, real
                // orderbook walk, fee-adjusted P&L, CSV logging) in ONE
                // async call. No mint_exec / presigner involvement.
                if let Some(paper) = paper_engine.clone() {
                    let event_for_paper = event.clone();
                    tokio::spawn(async move {
                        let now_unix = chrono::Utc::now().timestamp();
                        if let Err(e) = paper.run_event(&event_for_paper, now_unix).await {
                            tracing::error!("[PAPER] {}: {}", event_for_paper.event_slug, e);
                        }
                    });
                    continue;
                }

                // ==== LIVE / SIMULATION (non-paper) PATH ====
                // 1) Daily cap check
                {
                    let mut s = state.lock().await;
                    s.check_and_reset_daily_cap();
                    if s.daily_minted_usdc + config.mint_amount_usdc > config.daily_cap_usdc {
                        tracing::warn!(
                            "[DAILY-CAP] {:.2} + {:.2} > {:.2} — skipping {}",
                            s.daily_minted_usdc,
                            config.mint_amount_usdc,
                            config.daily_cap_usdc,
                            event.event_slug
                        );
                        continue;
                    }
                }

                // 2) Pre-sign dump orders (hot path, fire-and-forget)
                let pre = presigner.clone();
                let event_for_pre = event.clone();
                tokio::spawn(async move {
                    pre.presign_for_event(&event_for_pre).await;
                });

                // 3) Mint (also fire-and-forget — the mint channel is watched separately)
                let mint = mint_exec.clone();
                let event_for_mint = event.clone();
                let state_cl = state.clone();
                let mint_tx_cl = mint_tx.clone();
                let amount = config.mint_amount_usdc;
                tokio::spawn(async move {
                    match mint.mint_event(&event_for_mint, amount).await {
                        Ok(receipts) => {
                            let mut s = state_cl.lock().await;
                            s.daily_minted_usdc += amount;
                            // Forward every receipt to the mint channel so the dump
                            // path flushes the corresponding presigned orders.
                            for r in receipts {
                                let _ = mint_tx_cl.send(r);
                            }
                        }
                        Err(e) => {
                            tracing::error!("[MINT] {}: {}", event_for_mint.event_slug, e);
                        }
                    }
                });
            }

            Some(receipt) = mint_rx.recv() => {
                tracing::info!(
                    "[MINT-RECEIPT] {} tx=0x{} (lag from detect: n/a)",
                    receipt.event_slug,
                    hex::encode(&receipt.tx_hash.as_slice()[..8]),
                );
                // TODO: pair receipt.event_slug back to the condition_ids for that
                // event so we can flush each bucket's presigned orders. For now,
                // flush by slug is a no-op until the presigner cache is keyed
                // on slug as well as condition_id.
            }

            Some(ready) = ready_rx.recv() => {
                let cid_hex = hex::encode(ready.condition_id.as_slice());
                let pre = presigner.clone();
                tokio::spawn(async move {
                    let n = pre.flush_condition(&cid_hex).await;
                    if n > 0 {
                        tracing::info!("[CLOB-READY] flushed {} orders for cid=0x{}", n, cid_hex);
                    }
                });
            }

            _ = signal::ctrl_c() => {
                tracing::info!("[SHUTDOWN] ctrl-c — saving state");
                state.lock().await.save();
                break;
            }
        }
    }

    Ok(())
}

/// Derive the EOA address from a hex private key. Returns `None` if the key
/// is invalid or empty. Used only to seed the mempool watcher's filter.
fn derive_signer_address(private_key: &str) -> Option<Address> {
    use std::str::FromStr;
    if private_key.is_empty() {
        return None;
    }
    let signer = alloy_signer_local::PrivateKeySigner::from_str(private_key).ok()?;
    Some(alloy_signer::Signer::address(&signer))
}
