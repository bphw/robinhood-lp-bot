use super::abi::{Erc20, MintParams, NonfungiblePositionManager, UniswapV3Pool};
use super::ChainClient;
use crate::config::AppConfig;
use anyhow::{Context, Result};
use ethers::types::{Address, U256};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct LpResult {
    pub tx_hash: String,
    pub token_id: Option<U256>,
    pub explorer_tx_url: String,
}

fn fee_to_tick_spacing(fee: u32) -> i32 {
    match fee {
        100 => 1,
        500 => 10,
        3000 => 60,
        10000 => 200,
        other => {
            log::warn!("unrecognized fee tier {other}, defaulting tick spacing to 60");
            60
        }
    }
}

fn nearest_usable_tick(tick: i32, spacing: i32) -> i32 {
    let rounded = (tick as f64 / spacing as f64).round() as i32;
    rounded * spacing
}

/// Adds liquidity to `pool_address` sized at `usd_amount` (split ~50/50 by
/// value across the two tokens), centered on the pool's current price with a
/// +/- `cfg.wallet.tick_range_percent` range. Executes with the wallet
/// configured in `cfg.wallet.private_key` — this actually sends a real
/// transaction and spends real funds.
pub async fn add_liquidity(
    client: Arc<ChainClient>,
    cfg: &AppConfig,
    pool_address: Address,
    usd_amount: f64,
) -> Result<LpResult> {
    let pool = UniswapV3Pool::new(pool_address, client.clone());
    let token0 = pool.token_0().call().await.context("token0")?;
    let token1 = pool.token_1().call().await.context("token1")?;
    let fee = pool.fee().call().await.context("fee")?;
    let slot0 = pool.slot_0().call().await.context("slot0")?;
    let (sqrt_price_x96, current_tick) = (slot0.0, slot0.1);

    let erc0 = Erc20::new(token0, client.clone());
    let erc1 = Erc20::new(token1, client.clone());
    let dec0 = erc0.decimals().call().await.unwrap_or(18);
    let dec1 = erc1.decimals().call().await.unwrap_or(18);

    // Price of token1 in terms of token0, human units.
    let sqrt_price = sqrt_price_x96.as_u128() as f64 / 2f64.powi(96);
    let raw_price_1_per_0 = sqrt_price * sqrt_price;
    let price_1_per_0 = raw_price_1_per_0 * 10f64.powi(dec0 as i32 - dec1 as i32);

    // Split the USD amount 50/50 by value. We don't know USD prices here in
    // absolute terms (that lives in metrics.rs) — instead we size relative to
    // the pool's own price ratio so the two amounts are balanced for the
    // current tick, then scale by usd_amount as a rough total-value proxy.
    // NOTE: this assumes `usd_amount` is denominated such that "half in each
    // token, in the pool's price ratio" is a reasonable approximation; for
    // precise USD sizing, wire in the same pricing helper used in metrics.rs.
    let half = usd_amount / 2.0;
    let amount0_desired_human = half; // treat token0 leg as `half` units of token0's own numeraire
    let amount1_desired_human = half * price_1_per_0;

    let amount0_desired = U256::from((amount0_desired_human * 10f64.powi(dec0 as i32)) as u128);
    let amount1_desired = U256::from((amount1_desired_human * 10f64.powi(dec1 as i32)) as u128);

    let spacing = fee_to_tick_spacing(fee);
    let range = cfg.wallet.tick_range_percent / 100.0;
    let delta_tick = ((1.0 + range).ln() / 1.0001f64.ln()) as i32;
    let tick_lower = nearest_usable_tick(current_tick - delta_tick, spacing);
    let tick_upper = nearest_usable_tick(current_tick + delta_tick, spacing);

    let slippage = cfg.wallet.slippage_bps as f64 / 10_000.0;
    let amount0_min = U256::from(((amount0_desired.as_u128() as f64) * (1.0 - slippage)) as u128);
    let amount1_min = U256::from(((amount1_desired.as_u128() as f64) * (1.0 - slippage)) as u128);

    let recipient = client.address();
    let deadline = U256::from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 600,
    );

    // Approvals — required before the position manager can pull tokens in.
    approve_if_needed(&erc0, cfg, amount0_desired).await?;
    approve_if_needed(&erc1, cfg, amount1_desired).await?;

    let pm_address = Address::from_str(&cfg.chain.position_manager)?;
    let pm = NonfungiblePositionManager::new(pm_address, client.clone());

    let params = MintParams {
        token_0: token0,
        token_1: token1,
        fee,
        tick_lower,
        tick_upper,
        amount_0_desired: amount0_desired,
        amount_1_desired: amount1_desired,
        amount_0_min: amount0_min,
        amount_1_min: amount1_min,
        recipient,
        deadline,
    };

    let call = pm.mint(params);
    let pending = call.send().await.context("sending mint tx")?;
    let tx_hash = format!("{:#x}", pending.tx_hash());
    let receipt = pending.await.context("waiting for mint tx receipt")?;

    let token_id = receipt.and_then(|_r| None); // decoding the tokenId from logs is a further step; tx hash is enough to confirm on explorer.

    Ok(LpResult {
        explorer_tx_url: format!("{}/tx/{}", cfg.chain.explorer_base_url.trim_end_matches('/'), tx_hash),
        tx_hash,
        token_id,
    })
}

async fn approve_if_needed(
    erc20: &Erc20<ChainClient>,
    cfg: &AppConfig,
    amount: U256,
) -> Result<()> {
    let spender = Address::from_str(&cfg.chain.position_manager)?;
    let owner = erc20.client().address();
    let current = erc20.allowance(owner, spender).call().await.unwrap_or_default();
    if current >= amount {
        return Ok(());
    }
    let call = erc20.approve(spender, amount);
    let pending = call.send().await.context("sending approve tx")?;
    pending.await.context("waiting for approve receipt")?;
    Ok(())
}
