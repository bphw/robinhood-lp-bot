use super::abi::{
    CollectFilter, CollectParams, DecreaseLiquidityParams, Erc20, IncreaseLiquidityFilter, MintParams,
    NonfungiblePositionManager, UniswapV3Pool,
};
use super::autoswap::{swap_proceeds_to_usdg, AutoSwapOutcome, FailedLeg, SwapResult};
use super::position::{amounts_for_liquidity, tick_to_sqrt_price};
use super::ChainClient;
use crate::config::AppConfig;
use anyhow::{Context, Result};
use ethers::abi::RawLog;
use ethers::contract::EthLogDecode;
use ethers::types::{Address, U256};
use std::str::FromStr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct LpResult {
    pub tx_hash: String,
    pub explorer_tx_url: String,
    pub token_id: Option<u64>,
    pub token0: Address,
    pub token1: Address,
    pub fee: u32,
    pub tick_lower: i32,
    pub tick_upper: i32,
}

pub struct CloseResult {
    pub tx_hash: String,
    pub explorer_tx_url: String,
    /// Swaps performed to route proceeds into USDG, if any (empty if the
    /// position was already 100% USDG on both legs, or one leg had zero
    /// balance to begin with).
    pub swaps: Vec<SwapResult>,
    /// Legs that couldn't be swapped to USDG (e.g. a token that turned into
    /// a honeypot after being screened) — left in the wallet as whatever
    /// token they ended up as. The close itself still succeeds; this is
    /// reported so you know to check on it.
    pub failed_legs: Vec<FailedLeg>,
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

fn deadline_in(secs: u64) -> U256 {
    U256::from(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() + secs)
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
    // absolute terms (that lives in pricing.rs) — instead we size relative to
    // the pool's own price ratio so the two amounts are balanced for the
    // current tick, then scale by usd_amount as a rough total-value proxy.
    // NOTE: this assumes `usd_amount` is denominated such that "half in each
    // token, in the pool's price ratio" is a reasonable approximation; for
    // precise USD sizing, wire in chain::pricing::price_pool_tokens here too.
    let half = usd_amount / 2.0;
    let amount0_desired_human = half;
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
    let deadline = deadline_in(600);

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

    // Pull the tokenId out of the IncreaseLiquidity event emitted during
    // mint, rather than trusting log ordering/indices.
    let token_id = receipt.as_ref().and_then(|r| {
        r.logs.iter().find_map(|log| {
            if log.address != pm_address {
                return None;
            }
            let raw = RawLog { topics: log.topics.clone(), data: log.data.to_vec() };
            IncreaseLiquidityFilter::decode_log(&raw).ok().map(|ev| ev.token_id.as_u64())
        })
    });

    Ok(LpResult {
        explorer_tx_url: format!("{}/tx/{}", cfg.chain.explorer_base_url.trim_end_matches('/'), tx_hash),
        tx_hash,
        token_id,
        token0,
        token1,
        fee,
        tick_lower,
        tick_upper,
    })
}

/// Fully closes an open position: removes all liquidity and collects both
/// principal and any uncollected fees back to the wallet, then attempts to
/// burn the now-empty NFT (best-effort; failure to burn doesn't fail the
/// close — the funds are already out).
///
/// `pool_address` is needed to read the current price so we can compute an
/// expected payout and apply `wallet.slippage_bps` as a floor — without
/// this, a zero-minimum close is an open invitation for a sandwich attack
/// (see the module-level note in the README).
#[allow(unused_assignments)]
pub async fn close_position(
    client: Arc<ChainClient>,
    cfg: &AppConfig,
    token_id: u64,
    pool_address: Address,
) -> Result<CloseResult> {
    let pm_address = Address::from_str(&cfg.chain.position_manager)?;
    let pm = NonfungiblePositionManager::new(pm_address, client.clone());

    let info = pm.positions(U256::from(token_id)).call().await.context("fetching position before close")?;
    let (token0, token1, position_fee, tick_lower, tick_upper, liquidity) = (info.2, info.3, info.4, info.5, info.6, info.7);
    // (nonce, operator, token0, token1, fee, tickLower, tickUpper, liquidity, ...)

    let mut last_tx_hash = String::new();

    if liquidity > 0 {
        // Compute the expected payout right now, at the current on-chain
        // price, then require the actual execution to be within
        // `slippage_bps` of it. If the price has moved (or someone tries to
        // sandwich this transaction) beyond that tolerance by the time it
        // lands, decreaseLiquidity reverts instead of paying out at a worse
        // price.
        let pool = UniswapV3Pool::new(pool_address, client.clone());
        let slot0 = pool.slot_0().call().await.context("fetching pool slot0 for close")?;
        let sqrt_p = slot0.0.as_u128() as f64 / 2f64.powi(96);
        let sqrt_pa = tick_to_sqrt_price(tick_lower);
        let sqrt_pb = tick_to_sqrt_price(tick_upper);
        let (expected0_raw, expected1_raw) = amounts_for_liquidity(liquidity, sqrt_p, sqrt_pa, sqrt_pb);

        let slippage = cfg.wallet.slippage_bps as f64 / 10_000.0;
        let amount0_min = U256::from((expected0_raw * (1.0 - slippage)).max(0.0) as u128);
        let amount1_min = U256::from((expected1_raw * (1.0 - slippage)).max(0.0) as u128);

        let decrease_params = DecreaseLiquidityParams {
            token_id: U256::from(token_id),
            liquidity,
            amount_0_min: amount0_min,
            amount_1_min: amount1_min,
            deadline: deadline_in(600),
        };
        let call = pm.decrease_liquidity(decrease_params);
        let pending = call.send().await.context("sending decreaseLiquidity tx")?;
        last_tx_hash = format!("{:#x}", pending.tx_hash());
        pending.await.context("waiting for decreaseLiquidity receipt")?;
    }

    let collect_params = CollectParams {
        token_id: U256::from(token_id),
        recipient: client.address(),
        amount_0_max: u128::MAX,
        amount_1_max: u128::MAX,
    };
    let call = pm.collect(collect_params);
    let pending = call.send().await.context("sending collect tx")?;
    last_tx_hash = format!("{:#x}", pending.tx_hash());
    let collect_receipt = pending.await.context("waiting for collect receipt")?;

    // Decode the actual amounts transferred out (principal + fees combined)
    // from the Collect event, rather than trusting our own pre-computed
    // estimate — this is what really landed in the wallet.
    let (collected0, collected1) = collect_receipt
        .as_ref()
        .and_then(|r| {
            r.logs.iter().find_map(|log| {
                if log.address != pm_address {
                    return None;
                }
                let raw = RawLog { topics: log.topics.clone(), data: log.data.to_vec() };
                CollectFilter::decode_log(&raw).ok().map(|ev| (ev.amount_0, ev.amount_1))
            })
        })
        .unwrap_or((U256::zero(), U256::zero()));

    let outcome: AutoSwapOutcome = if collected0.is_zero() && collected1.is_zero() {
        AutoSwapOutcome { swaps: Vec::new(), failed_legs: Vec::new() }
    } else {
        // Note: swap_proceeds_to_usdg already isolates per-leg failures
        // internally (see its doc comment) — it only returns Err for
        // something structural like a bad config address, not for an
        // individual honeypot leg. So this ? is intentional: a genuine
        // error here means something is actually wrong, not just "one leg
        // was a honeypot."
        swap_proceeds_to_usdg(client.clone(), cfg, token0, collected0, token1, collected1, position_fee)
            .await
            .context("auto-swapping proceeds to USDG")?
    };

    // Best-effort cleanup; a failed burn doesn't matter, the funds are safe.
    let _ = pm.burn(U256::from(token_id)).send().await;

    Ok(CloseResult {
        explorer_tx_url: format!("{}/tx/{}", cfg.chain.explorer_base_url.trim_end_matches('/'), last_tx_hash),
        tx_hash: last_tx_hash,
        swaps: outcome.swaps,
        failed_legs: outcome.failed_legs,
    })
}

async fn approve_if_needed(erc20: &Erc20<ChainClient>, cfg: &AppConfig, amount: U256) -> Result<()> {
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
