use super::abi::{Erc20, UniswapV3Factory, UniswapV3Pool};
use super::safety::is_contract_verified;
use super::ChainClient;
use crate::config::AppConfig;
use crate::models::{PoolInfo, PoolMetrics};
use anyhow::{Context, Result};
use ethers::types::{Address, U256};
use std::str::FromStr;
use std::sync::Arc;

const LOG_CHUNK_SIZE: u64 = 5_000;
const STABLE_DECIMALS_FALLBACK: u8 = 6;

async fn eth_price_usd(client: Arc<ChainClient>, cfg: &AppConfig) -> Result<f64> {
    let factory_addr = Address::from_str(&cfg.chain.uniswap_v3_factory)?;
    let factory = UniswapV3Factory::new(factory_addr, client.clone());
    let weth = Address::from_str(&cfg.chain.weth_address)?;
    let usdc = Address::from_str(&cfg.chain.usdc_address)?;

    // Try the common fee tiers in order until one has a deployed pool.
    for fee in [500u32, 3000, 10000, 100] {
        let pool_addr = factory.get_pool(weth, usdc, fee).call().await.unwrap_or_default();
        if pool_addr == Address::zero() {
            continue;
        }
        let pool = UniswapV3Pool::new(pool_addr, client.clone());
        let (t0, _t1) = (pool.token_0().call().await?, pool.token_1().call().await?);
        let slot0 = pool.slot_0().call().await?;
        let sqrt_price_x96 = slot0.0;

        let (weth_decimals, usdc_decimals) = (18u8, STABLE_DECIMALS_FALLBACK);
        let price_1_per_0 = sqrt_price_x96_to_price(sqrt_price_x96, weth_decimals, usdc_decimals, t0 == weth);

        // price_1_per_0 here is defined as "USD per WETH" once we account for
        // which token is which — see sqrt_price_x96_to_price for the convention.
        return Ok(price_1_per_0);
    }

    anyhow::bail!("could not find a WETH/USDC pool to price ETH from")
}

/// Converts a Uniswap V3 slot0 sqrtPriceX96 into a human-readable price of
/// "USD-like token per ETH-like token", assuming `weth_is_token0` tells us
/// which side of the pool WETH sits on. decimals0/1 must match token0/token1
/// respectively as returned by the pool contract.
fn sqrt_price_x96_to_price(sqrt_price_x96: U256, decimals0: u8, decimals1: u8, weth_is_token0: bool) -> f64 {
    let sqrt_price = sqrt_price_x96.as_u128() as f64 / (2f64.powi(96));
    let raw_price_1_per_0 = sqrt_price * sqrt_price; // token1 per token0, raw units
    let adj = 10f64.powi(decimals0 as i32 - decimals1 as i32);
    let price_1_per_0_human = raw_price_1_per_0 * adj; // human token1 per human token0

    if weth_is_token0 {
        // price_1_per_0_human = USDC per WETH = USD per ETH directly.
        price_1_per_0_human
    } else {
        // token0 is USDC, token1 is WETH: price_1_per_0_human = WETH per USDC.
        // Invert to get USD per ETH.
        1.0 / price_1_per_0_human
    }
}

async fn token_info(client: Arc<ChainClient>, token: Address) -> Result<(String, u8)> {
    let erc20 = Erc20::new(token, client);
    let symbol = erc20.symbol().call().await.unwrap_or_else(|_| "???".to_string());
    let decimals = erc20.decimals().call().await.unwrap_or(18);
    Ok((symbol, decimals))
}

/// Derives the USD price of `token`, given the pool's current price and a
/// known USD price for the *other* side of the pool. Returns None if neither
/// side of the pool is a token we know how to price (WETH or USDC).
async fn price_pool_tokens(
    client: Arc<ChainClient>,
    cfg: &AppConfig,
    pool_addr: Address,
    token0: Address,
    token1: Address,
    decimals0: u8,
    decimals1: u8,
    eth_usd: f64,
) -> Option<(f64, f64)> {
    let weth = Address::from_str(&cfg.chain.weth_address).ok()?;
    let usdc = Address::from_str(&cfg.chain.usdc_address).ok()?;

    let (known_side_is_0, known_price_usd) = if token0 == weth {
        (true, eth_usd)
    } else if token1 == weth {
        (false, eth_usd)
    } else if token0 == usdc {
        (true, 1.0)
    } else if token1 == usdc {
        (false, 1.0)
    } else {
        return None;
    };

    let pool = UniswapV3Pool::new(pool_addr, client);
    let slot0 = pool.slot_0().call().await.ok()?;
    let sqrt_price_x96 = slot0.0;
    let sqrt_price = sqrt_price_x96.as_u128() as f64 / 2f64.powi(96);
    let raw_price_1_per_0 = sqrt_price * sqrt_price;
    let price_1_per_0_human = raw_price_1_per_0 * 10f64.powi(decimals0 as i32 - decimals1 as i32);

    if known_side_is_0 {
        // token0 price known; token1 price = price0 / (token1 per token0)... derive:
        // 1 token0 = price_1_per_0_human token1  =>  price1 = price0 / price_1_per_0_human
        let price1 = known_price_usd / price_1_per_0_human;
        Some((known_price_usd, price1))
    } else {
        // token1 price known; price0 = price1 * price_1_per_0_human
        let price0 = known_price_usd * price_1_per_0_human;
        Some((price0, known_price_usd))
    }
}

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

        let volume = estimate_volume_usd(
            client.clone(),
            cfg,
            pool,
            current_block,
            dec0,
            dec1,
            p0,
            p1,
        )
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

/// Sums swap volume over `lookback_blocks_for_volume` blocks and converts it
/// to USD using whichever side of the pool has a known price. This scans raw
/// Swap event logs directly from the pool contract (no subgraph dependency).
async fn estimate_volume_usd(
    client: Arc<ChainClient>,
    cfg: &AppConfig,
    pool: &PoolInfo,
    current_block: u64,
    dec0: u8,
    dec1: u8,
    p0: f64,
    p1: f64,
) -> Result<f64> {
    let pool_contract = UniswapV3Pool::new(pool.address, client.clone());
    let lookback = cfg.screening.lookback_blocks_for_volume;
    let from_block = current_block.saturating_sub(lookback).max(pool.created_block);

    let mut total_usd = 0.0f64;
    let mut start = from_block;

    while start <= current_block {
        let end = (start + LOG_CHUNK_SIZE).min(current_block);
        let events = pool_contract
            .event::<super::abi::SwapFilter>()
            .from_block(start)
            .to_block(end)
            .query()
            .await
            .with_context(|| format!("querying Swap logs {start}-{end} for {:?}", pool.address))?;

        for ev in events {
            let amt0 = i256_to_f64(ev.amount_0) / 10f64.powi(dec0 as i32);
            let amt1 = i256_to_f64(ev.amount_1) / 10f64.powi(dec1 as i32);
            // Both sides represent the same trade value; use whichever price is
            // more likely to be accurate to cross-check, then average.
            let usd_from_0 = amt0.abs() * p0;
            let usd_from_1 = amt1.abs() * p1;
            total_usd += (usd_from_0 + usd_from_1) / 2.0;
        }

        start = end + 1;
    }

    Ok(total_usd)
}

fn i256_to_f64(v: ethers::types::I256) -> f64 {
    // I256 -> f64 via string round-trip keeps this simple and safe against
    // overflow; precision loss is acceptable for a volume estimate.
    v.to_string().parse::<f64>().unwrap_or(0.0)
}
