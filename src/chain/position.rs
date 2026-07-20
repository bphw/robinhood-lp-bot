use super::abi::{NonfungiblePositionManager, UniswapV3Pool};
use super::pricing::{eth_price_usd, price_pool_tokens, token_info};
use super::ChainClient;
use crate::config::AppConfig;
use crate::models::{Position, PositionPnl};
use anyhow::{Context, Result};
use ethers::types::{Address, U256};
use std::str::FromStr;
use std::sync::Arc;

/// sqrt(1.0001^tick) — the raw (non-decimal-adjusted) sqrt price at a given
/// tick, in the same units as sqrtPriceX96/2^96. This is the standard
/// Uniswap V3 tick-to-price relationship.
fn tick_to_sqrt_price(tick: i32) -> f64 {
    1.0001f64.powf(tick as f64 / 2.0)
}

/// Given liquidity and the current/lower/upper sqrt prices (all in raw,
/// non-decimal-adjusted units), returns (amount0_raw, amount1_raw) — the
/// token amounts represented by that liquidity, in each token's smallest
/// unit (e.g. wei). This is the standard Uniswap V3 formula used to convert
/// an NFT position's `liquidity` into actual token amounts.
fn amounts_for_liquidity(liquidity: u128, sqrt_p: f64, sqrt_pa: f64, sqrt_pb: f64) -> (f64, f64) {
    let l = liquidity as f64;
    let (sqrt_pa, sqrt_pb) = if sqrt_pa <= sqrt_pb { (sqrt_pa, sqrt_pb) } else { (sqrt_pb, sqrt_pa) };

    if sqrt_p <= sqrt_pa {
        // Current price below range: position is 100% token0.
        let amount0 = l * (1.0 / sqrt_pa - 1.0 / sqrt_pb);
        (amount0, 0.0)
    } else if sqrt_p >= sqrt_pb {
        // Current price above range: position is 100% token1.
        let amount1 = l * (sqrt_pb - sqrt_pa);
        (0.0, amount1)
    } else {
        let amount0 = l * (1.0 / sqrt_p - 1.0 / sqrt_pb);
        let amount1 = l * (sqrt_p - sqrt_pa);
        (amount0, amount1)
    }
}

/// Computes a fresh PnL snapshot for an open position: current value of the
/// underlying liquidity plus uncollected fees, minus what you originally put
/// in (`entry_cost_usd`).
///
/// This intentionally does NOT try to separate "impermanent loss" from
/// "fees earned" as distinct line items — `pnl_usd` is simply
/// current-value-including-fees minus entry cost, which is what actually
/// matters for a take-profit/stop-loss decision.
pub async fn compute_pnl(client: Arc<ChainClient>, cfg: &AppConfig, position: &Position) -> Result<PositionPnl> {
    let pm_address = Address::from_str(&cfg.chain.position_manager)?;
    let pm = NonfungiblePositionManager::new(pm_address, client.clone());

    let info = pm
        .positions(U256::from(position.token_id))
        .call()
        .await
        .context("fetching position info")?;
    // positions() returns: (nonce, operator, token0, token1, fee, tickLower,
    // tickUpper, liquidity, feeGrowthInside0LastX128, feeGrowthInside1LastX128,
    // tokensOwed0, tokensOwed1)
    let (_, _, token0, token1, _fee, tick_lower, tick_upper, liquidity, _, _, owed0, owed1) = info;

    let pool = UniswapV3Pool::new(position.pool_address, client.clone());
    let slot0 = pool.slot_0().call().await.context("fetching pool slot0")?;
    let (sqrt_price_x96, current_tick) = (slot0.0, slot0.1);

    let (_sym0, dec0) = token_info(client.clone(), token0).await?;
    let (_sym1, dec1) = token_info(client.clone(), token1).await?;

    let sqrt_p = sqrt_price_x96.as_u128() as f64 / 2f64.powi(96);
    let sqrt_pa = tick_to_sqrt_price(tick_lower);
    let sqrt_pb = tick_to_sqrt_price(tick_upper);

    let (amount0_raw, amount1_raw) = amounts_for_liquidity(liquidity, sqrt_p, sqrt_pa, sqrt_pb);
    let amount0_h = amount0_raw / 10f64.powi(dec0 as i32);
    let amount1_h = amount1_raw / 10f64.powi(dec1 as i32);
    let owed0_h = owed0 as f64 / 10f64.powi(dec0 as i32);
    let owed1_h = owed1 as f64 / 10f64.powi(dec1 as i32);

    let eth_usd = eth_price_usd(client.clone(), cfg).await.unwrap_or(0.0);
    let prices = price_pool_tokens(client.clone(), cfg, position.pool_address, token0, token1, dec0, dec1, eth_usd)
        .await
        .context("pool no longer priceable (neither side is WETH/USDG) — cannot compute PnL")?;
    let (p0, p1) = prices;

    let principal_value_usd = (amount0_h + owed0_h) * p0 + (amount1_h + owed1_h) * p1;
    let uncollected_fees_usd = owed0_h * p0 + owed1_h * p1;
    let pnl_usd = principal_value_usd - position.entry_cost_usd;
    let pnl_percent = if position.entry_cost_usd > 0.0 {
        pnl_usd / position.entry_cost_usd * 100.0
    } else {
        0.0
    };
    let in_range = current_tick >= tick_lower && current_tick < tick_upper;

    Ok(PositionPnl {
        current_value_usd: principal_value_usd,
        uncollected_fees_usd,
        pnl_usd,
        pnl_percent,
        in_range,
    })
}
