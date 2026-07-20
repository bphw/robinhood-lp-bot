use super::abi::Erc20;
use super::pricing::{eth_price_usd, i256_to_f64, price_pool_tokens, token_info};
use super::safety::is_contract_verified;
use super::ChainClient;
use crate::config::AppConfig;
use crate::models::{PoolInfo, PoolMetrics};
use anyhow::{Context, Result};
use std::sync::Arc;

const LOG_CHUNK_SIZE: u64 = 5_000;

pub async fn compute_metrics(
    client: Arc<ChainClient>,
    cfg: &AppConfig,
    pool: &PoolInfo,
    current_block: u64,
    current_timestamp: u64,
) -> Result<PoolMetrics> {
    let (sym0, dec0) = token_info(client.clone(), pool.token0).await?;
    let (sym1, dec1) = token_info(client.clone(), pool.token1).await?;

    let age_hours = if pool.created_timestamp > 0 && current_timestamp > pool.created_timestamp {
        (current_timestamp - pool.created_timestamp) as f64 / 3600.0
    } else {
        0.0
    };

    let token0_verified = is_contract_verified(&cfg.chain.blockscout_api_base, pool.token0).await;
    let token1_verified = is_contract_verified(&cfg.chain.blockscout_api_base, pool.token1).await;

    let eth_usd = eth_price_usd(client.clone(), cfg).await.unwrap_or(0.0);

    let prices = price_pool_tokens(
        client.clone(),
        cfg,
        pool.address,
        pool.token0,
        pool.token1,
        dec0,
        dec1,
        eth_usd,
    )
    .await;

    let (tvl_usd, volume_24h_usd, apr_percent) = if let Some((p0, p1)) = prices {
        let erc0 = Erc20::new(pool.token0, client.clone());
        let erc1 = Erc20::new(pool.token1, client.clone());
        let bal0 = erc0.balance_of(pool.address).call().await.unwrap_or_default();
        let bal1 = erc1.balance_of(pool.address).call().await.unwrap_or_default();

        let bal0_h = bal0.as_u128() as f64 / 10f64.powi(dec0 as i32);
        let bal1_h = bal1.as_u128() as f64 / 10f64.powi(dec1 as i32);
        let tvl = bal0_h * p0 + bal1_h * p1;

        let lookback = cfg.screening.lookback_blocks_for_volume;
        let from_block = current_block.saturating_sub(lookback).max(pool.created_block);
        let volume = estimate_volume_usd(client.clone(), pool.address, from_block, current_block, dec0, dec1, p0, p1)
            .await
            .unwrap_or(0.0);

        let fee_fraction = pool.fee as f64 / 1_000_000.0;
        let apr = if tvl > 0.0 {
            (volume * fee_fraction / tvl) * 365.0 * 100.0
        } else {
            0.0
        };

        (Some(tvl), Some(volume), Some(apr))
    } else {
        (None, None, None)
    };

    Ok(PoolMetrics {
        token0_symbol: sym0,
        token1_symbol: sym1,
        tvl_usd,
        volume_24h_usd,
        apr_percent,
        age_hours,
        token0_verified,
        token1_verified,
    })
}

/// Sums swap volume in USD over an arbitrary block range `[from_block,
/// to_block]`, using whichever side of the pool has a known price. Shared by
/// the 24h-volume calculation here and by the volume-spike detector in
/// `spike.rs`, which calls this twice with two different windows.
pub async fn estimate_volume_usd(
    client: Arc<ChainClient>,
    pool_address: ethers::types::Address,
    from_block: u64,
    to_block: u64,
    dec0: u8,
    dec1: u8,
    p0: f64,
    p1: f64,
) -> Result<f64> {
    let pool_contract = super::abi::UniswapV3Pool::new(pool_address, client.clone());

    let mut total_usd = 0.0f64;
    let mut start = from_block;

    while start <= to_block {
        let end = (start + LOG_CHUNK_SIZE).min(to_block);
        let events = pool_contract
            .event::<super::abi::SwapFilter>()
            .from_block(start)
            .to_block(end)
            .query()
            .await
            .with_context(|| format!("querying Swap logs {start}-{end} for {:?}", pool_address))?;

        for ev in events {
            let amt0 = i256_to_f64(ev.amount_0) / 10f64.powi(dec0 as i32);
            let amt1 = i256_to_f64(ev.amount_1) / 10f64.powi(dec1 as i32);
            let usd_from_0 = amt0.abs() * p0;
            let usd_from_1 = amt1.abs() * p1;
            total_usd += (usd_from_0 + usd_from_1) / 2.0;
        }

        start = end + 1;
    }

    Ok(total_usd)
}
