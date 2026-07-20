mod chain;
mod config;
mod models;
mod screener;
mod storage;
mod telegram;

use crate::config::AppConfig;
use crate::models::PoolCandidate;
use anyhow::Result;
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

    // Task 1: listen for Telegram button taps and execute LP adds.
    let listener_cfg = cfg.clone();
    let listener_client = client.clone();
    let listener_bot = bot.clone();
    tokio::spawn(async move {
        if let Err(e) = telegram::run_callback_listener(listener_bot, listener_cfg, listener_client).await {
            log::error!("Telegram callback listener stopped: {e:?}");
        }
    });

    log::info!(
        "Starting screening loop (poll interval: {}s)",
        cfg.screening.poll_interval_secs
    );

    loop {
        if let Err(e) = run_scan_cycle(&cfg, client.clone(), &bot, chat_id, storage.clone()).await {
            log::error!("Scan cycle failed: {e:?}");
        }
        tokio::time::sleep(Duration::from_secs(cfg.screening.poll_interval_secs)).await;
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
