mod chain;
mod config;
mod models;
mod screener;
mod storage;
mod telegram;

use crate::config::AppConfig;
use crate::models::PoolCandidate;
use anyhow::Result;
use ethers::middleware::Middleware;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use teloxide::prelude::*;
use tokio::sync::Mutex;

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let cfg = Arc::new(AppConfig::load("config.toml")?);
    let client = chain::build_client(&cfg).await?;
    let bot = Bot::new(cfg.telegram.bot_token.clone());
    let chat_id = ChatId(cfg.telegram.chat_id);

    let storage = Arc::new(Mutex::new(storage::Storage::load_or_default("state.json")?));
    let screening_enabled = Arc::new(AtomicBool::new(cfg.screening.enabled));

    // Task 1: listen for Telegram button taps ("Add LP now" / "Close
    // position") and the /positions command.
    let bot_cfg = cfg.clone();
    let bot_client = client.clone();
    let bot_storage = storage.clone();
    let bot_screening = screening_enabled.clone();
    let bot_for_listener = bot.clone();
    tokio::spawn(async move {
        if let Err(e) = telegram::run_bot(bot_for_listener, bot_cfg, bot_client, bot_storage, bot_screening).await {
            log::error!("Telegram bot stopped: {e:?}");
        }
    });

    // Task 2: pool discovery + screening loop (skips when screening is disabled).
    let scan_cfg = cfg.clone();
    let scan_client = client.clone();
    let scan_bot = bot.clone();
    let scan_storage = storage.clone();
    let scan_screening = screening_enabled.clone();
    tokio::spawn(async move {
        log::info!(
            "Starting screening loop (poll interval: {}s)",
            scan_cfg.screening.poll_interval_secs
        );
        loop {
            if scan_screening.load(Ordering::Relaxed) {
                if let Err(e) = run_scan_cycle(&scan_cfg, scan_client.clone(), &scan_bot, chat_id, scan_storage.clone()).await {
                    log::error!("Scan cycle failed: {e:?}");
                }
            } else {
                log::info!("Auto-screening is disabled — skipping scan cycle. Use /toggle_screening to re-enable.");
            }
            tokio::time::sleep(Duration::from_secs(scan_cfg.screening.poll_interval_secs)).await;
        }
    });

    // Task 3: position monitoring — PnL / take-profit / stop-loss / volume
    // spikes, for pools where this bot holds an open LP position.
    log::info!(
        "Starting position monitoring loop (check interval: {}s)",
        cfg.monitoring.position_check_interval_secs
    );
    loop {
        if let Err(e) = run_monitoring_cycle(&cfg, client.clone(), &bot, chat_id, storage.clone()).await {
            log::error!("Monitoring cycle failed: {e:?}");
        }
        tokio::time::sleep(Duration::from_secs(cfg.monitoring.position_check_interval_secs)).await;
    }
}

async fn run_scan_cycle(
    cfg: &Arc<AppConfig>,
    client: Arc<chain::ChainClient>,
    bot: &Bot,
    chat_id: ChatId,
    storage: Arc<Mutex<storage::Storage>>,
) -> Result<()> {
    let from_block = {
        let s = storage.lock().await;
        let last = s.last_scanned_block();
        if last == 0 {
            cfg.chain.factory_deployment_block
        } else {
            last + 1
        }
    };

    let (new_pools, latest_block) =
        chain::pools::discover_new_pools(client.clone(), &cfg.chain.uniswap_v3_factory, from_block).await?;

    log::info!("Discovered {} new pool(s) up to block {latest_block}", new_pools.len());

    let now_ts = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();

    for pool in new_pools {
        let already = {
            let s = storage.lock().await;
            s.already_alerted(pool.address)
        };
        if already {
            continue;
        }

        let metrics = match chain::metrics::compute_metrics(client.clone(), cfg, &pool, latest_block, now_ts).await {
            Ok(m) => m,
            Err(e) => {
                log::warn!("Failed to compute metrics for pool {:?}: {e:?}", pool.address);
                continue;
            }
        };

        let candidate = PoolCandidate { info: pool.clone(), metrics };
        let result = screener::screen(candidate, &cfg.screening);

        if result.passed {
            log::info!("Pool {:?} PASSED screening", pool.address);
            if let Err(e) = telegram::send_alert(bot, chat_id, &result).await {
                log::error!("Failed to send Telegram alert: {e:?}");
                continue; // retry next cycle rather than marking as alerted
            }
            let mut s = storage.lock().await;
            s.mark_alerted(pool.address)?;
        } else {
            log::info!("Pool {:?} did not pass: {:?}", pool.address, result.reasons);
            let mut s = storage.lock().await;
            s.mark_alerted(pool.address)?;
        }
    }

    {
        let mut s = storage.lock().await;
        s.set_last_scanned_block(latest_block)?;
    }

    Ok(())
}

/// For every open position: compute fresh PnL and alert (with a close
/// button) if take-profit or stop-loss is hit; also check for a volume
/// spike on that position's pool.
async fn run_monitoring_cycle(
    cfg: &Arc<AppConfig>,
    client: Arc<chain::ChainClient>,
    bot: &Bot,
    chat_id: ChatId,
    storage: Arc<Mutex<storage::Storage>>,
) -> Result<()> {
    let open_positions = { storage.lock().await.open_positions() };
    if open_positions.is_empty() {
        return Ok(());
    }

    let current_block = client.get_block_number().await?.as_u64();

    for position in open_positions {
        // --- PnL / take-profit / stop-loss ---
        let already_tpsl_alerted = { storage.lock().await.already_tpsl_alerted(position.token_id) };
        if !already_tpsl_alerted {
            match chain::position::compute_pnl(client.clone(), cfg, &position).await {
                Ok(pnl) => {
                    let hit = if pnl.pnl_percent >= cfg.monitoring.take_profit_percent {
                        Some("Take-profit")
                    } else if pnl.pnl_percent <= -cfg.monitoring.stop_loss_percent {
                        Some("Stop-loss")
                    } else {
                        None
                    };
                    if let Some(hit) = hit {
                        if let Err(e) = telegram::send_tp_sl_alert(bot, chat_id, &position, pnl.pnl_percent, hit).await {
                            log::error!("Failed to send TP/SL alert for position {}: {e:?}", position.token_id);
                        } else {
                            let mut s = storage.lock().await;
                            s.mark_tpsl_alerted(position.token_id)?;
                        }
                    }
                }
                Err(e) => log::warn!("PnL check failed for position {}: {e:?}", position.token_id),
            }
        }

        // --- Volume spike, scoped to this position's pool ---
        let cooldown_blocks =
            (cfg.monitoring.volume_spike_window_hours * cfg.monitoring.approx_blocks_per_hour as f64) as u64;
        let on_cooldown = {
            storage.lock().await.spike_alert_on_cooldown(position.pool_address, current_block, cooldown_blocks)
        };
        if !on_cooldown {
            let pool_info = models::PoolInfo {
                address: position.pool_address,
                token0: position.token0,
                token1: position.token1,
                fee: position.fee,
                created_block: position.pool_created_block,
                created_timestamp: 0,
            };
            match chain::spike::check_volume_spike(client.clone(), cfg, &pool_info, current_block).await {
                Ok(Some(spike)) => {
                    if let Err(e) = telegram::send_spike_alert(bot, chat_id, &position, &spike).await {
                        log::error!("Failed to send spike alert for position {}: {e:?}", position.token_id);
                    } else {
                        let mut s = storage.lock().await;
                        s.mark_spike_alerted(position.pool_address, current_block)?;
                    }
                }
                Ok(None) => {}
                Err(e) => log::warn!("Volume spike check failed for pool {:?}: {e:?}", position.pool_address),
            }
        }
    }

    Ok(())
}
