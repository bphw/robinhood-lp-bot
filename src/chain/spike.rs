use super::metrics::estimate_volume_usd;
use super::pricing::{eth_price_usd, price_pool_tokens, token_info};
use super::ChainClient;
use crate::config::AppConfig;
use crate::models::{PoolInfo, VolumeSpike};
use anyhow::Result;
use std::sync::Arc;

/// A recent-window volume below this floor is treated as noise — a jump
/// from "$5 of volume" to "$50 of volume" is technically a 10x spike but
/// isn't meaningful. Reuses the screening min-volume threshold, scaled down
/// to an hourly-ish figure, as a reasonable floor.
fn min_meaningful_volume_usd(cfg: &AppConfig) -> f64 {
    (cfg.screening.min_volume_24h_usd / 24.0).max(50.0)
}

/// Compares volume in the most recent window against the window immediately
/// before it. Returns None if there isn't enough history yet (pool younger
/// than two full windows) or if the pool isn't priceable.
pub async fn check_volume_spike(
    client: Arc<ChainClient>,
    cfg: &AppConfig,
    pool: &PoolInfo,
    current_block: u64,
) -> Result<Option<VolumeSpike>> {
    let window_blocks = (cfg.monitoring.volume_spike_window_hours * cfg.monitoring.approx_blocks_per_hour as f64) as u64;
    if window_blocks == 0 {
        return Ok(None);
    }

    let recent_from = current_block.saturating_sub(window_blocks);
    let previous_from = current_block.saturating_sub(window_blocks * 2);

    if previous_from < pool.created_block {
        // Pool isn't old enough yet to have a full "previous window" to
        // compare against.
        return Ok(None);
    }

    let (sym0, dec0) = token_info(client.clone(), pool.token0).await?;
    let _ = sym0;
    let (_sym1, dec1) = token_info(client.clone(), pool.token1).await?;

    let eth_usd = eth_price_usd(client.clone(), cfg).await.unwrap_or(0.0);
    let Some((p0, p1)) = price_pool_tokens(client.clone(), cfg, pool.address, pool.token0, pool.token1, dec0, dec1, eth_usd).await else {
        return Ok(None);
    };

    let recent_volume_usd =
        estimate_volume_usd(client.clone(), pool.address, recent_from, current_block, dec0, dec1, p0, p1)
            .await?
            .volume_usd;
    let previous_volume_usd =
        estimate_volume_usd(client.clone(), pool.address, previous_from, recent_from.saturating_sub(1), dec0, dec1, p0, p1)
            .await?
            .volume_usd;

    if recent_volume_usd < min_meaningful_volume_usd(cfg) {
        return Ok(None);
    }

    let ratio = if previous_volume_usd > 0.0 {
        recent_volume_usd / previous_volume_usd
    } else {
        // No volume at all in the previous window but meaningful volume now
        // — treat as a spike (ratio reported as the recent/floor for display).
        recent_volume_usd / min_meaningful_volume_usd(cfg)
    };

    if ratio >= cfg.monitoring.volume_spike_multiplier {
        Ok(Some(VolumeSpike { recent_volume_usd, previous_volume_usd, ratio }))
    } else {
        Ok(None)
    }
}
