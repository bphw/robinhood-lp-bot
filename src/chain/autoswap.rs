use super::abi::{Erc20, ExactInputSingleParams, QuoteExactInputSingleParams, QuoterV2, SwapRouter02};
use super::pricing::find_pool_fee;
use super::ChainClient;
use crate::config::AppConfig;
use anyhow::{Context, Result};
use ethers::types::{Address, U256};
use std::str::FromStr;
use std::sync::Arc;

pub struct SwapResult {
    pub tx_hash: String,
    pub token_in: Address,
    pub amount_in: U256,
    pub amount_out: U256,
}

/// A leg that couldn't be converted to USDG — left in the wallet as
/// whatever token it already was, rather than blocking the rest of the
/// close. `stuck_token`/`stuck_amount` is what's actually sitting in the
/// wallet as a result (which may be the original token, or WETH if a
/// two-hop swap got halfway before failing).
pub struct FailedLeg {
    pub stuck_token: Address,
    pub stuck_amount: U256,
    pub reason: String,
}

pub struct AutoSwapOutcome {
    pub swaps: Vec<SwapResult>,
    pub failed_legs: Vec<FailedLeg>,
}

/// Swaps `amount_in` of `token_in` into `token_out` at `fee`, using a
/// QuoterV2 quote to compute a real slippage-protected minimum (rather than
/// accepting any output, which — same as the close-position fix — would be
/// an open door for a sandwich attack).
async fn swap_single(
    client: Arc<ChainClient>,
    cfg: &AppConfig,
    token_in: Address,
    token_out: Address,
    fee: u32,
    amount_in: U256,
) -> Result<SwapResult> {
    let quoter_addr = Address::from_str(&cfg.chain.quoter_v2)?;
    let quoter = QuoterV2::new(quoter_addr, client.clone());

    let quote = quoter
        .quote_exact_input_single(QuoteExactInputSingleParams {
            token_in,
            token_out,
            amount_in,
            fee,
            sqrt_price_limit_x96: U256::zero(),
        })
        .call()
        .await
        .context("quoting swap")?;
    let expected_out = quote.0;

    let slippage = cfg.wallet.slippage_bps as f64 / 10_000.0;
    let min_out = U256::from(((expected_out.as_u128() as f64) * (1.0 - slippage)) as u128);

    let erc20 = Erc20::new(token_in, client.clone());
    let router_addr = Address::from_str(&cfg.chain.swap_router)?;
    let owner = client.address();
    let current_allowance = erc20.allowance(owner, router_addr).call().await.unwrap_or_default();
    if current_allowance < amount_in {
        let call = erc20.approve(router_addr, amount_in);
        let pending = call.send().await.context("sending swap approve tx")?;
        pending.await.context("waiting for swap approve receipt")?;
    }

    let router = SwapRouter02::new(router_addr, client.clone());
    let params = ExactInputSingleParams {
        token_in,
        token_out,
        fee,
        recipient: owner,
        amount_in,
        amount_out_minimum: min_out,
        sqrt_price_limit_x96: U256::zero(),
    };
    let call = router.exact_input_single(params);
    let pending = call.send().await.context("sending swap tx")?;
    let tx_hash = format!("{:#x}", pending.tx_hash());
    pending.await.context("waiting for swap receipt")?;

    Ok(SwapResult { tx_hash, token_in, amount_in, amount_out: expected_out })
}

/// Swaps both legs of a just-closed position into WETH (the reference
/// volatile asset). Any leg that's already WETH is left alone. Any leg that's
/// USDG swaps directly to WETH. Any other token routes through the pool it
/// was just paired with — first trying a direct pool to WETH, then falling
/// back to a two-hop USDG bridge.
pub async fn swap_proceeds_to_weth(
    client: Arc<ChainClient>,
    cfg: &AppConfig,
    token0: Address,
    amount0: U256,
    token1: Address,
    amount1: U256,
    position_fee: u32,
) -> Result<AutoSwapOutcome> {
    let usdg = Address::from_str(&cfg.chain.usdc_address)?;
    let weth = Address::from_str(&cfg.chain.weth_address)?;
    let mut swaps = Vec::new();
    let mut failed_legs = Vec::new();

    for (token, amount, other_token) in [(token0, amount0, token1), (token1, amount1, token0)] {
        if amount.is_zero() || token == weth {
            continue; // nothing to do, or already the target asset
        }

        if token == usdg {
            // Direct USDG -> WETH leg.
            match find_pool_fee(client.clone(), cfg, usdg, weth).await {
                Ok(Some(fee)) => match swap_single(client.clone(), cfg, token, weth, fee, amount).await {
                    Ok(r) => swaps.push(r),
                    Err(e) => failed_legs.push(FailedLeg { stuck_token: usdg, stuck_amount: amount, reason: e.to_string() }),
                },
                Ok(None) => failed_legs.push(FailedLeg {
                    stuck_token: usdg,
                    stuck_amount: amount,
                    reason: "no USDG/WETH pool found".to_string(),
                }),
                Err(e) => failed_legs.push(FailedLeg { stuck_token: usdg, stuck_amount: amount, reason: e.to_string() }),
            }
            continue;
        }

        // Non-reference token: it was paired with `other_token` in the pool
        // we just closed.
        if other_token == weth {
            match swap_single(client.clone(), cfg, token, weth, position_fee, amount).await {
                Ok(r) => swaps.push(r),
                Err(e) => failed_legs.push(FailedLeg { stuck_token: token, stuck_amount: amount, reason: e.to_string() }),
            }
        } else if other_token == usdg {
            // Two-hop: token -> USDG (via the pool we just exited) -> WETH.
            match swap_single(client.clone(), cfg, token, usdg, position_fee, amount).await {
                Ok(hop1) => {
                    let usdg_out = hop1.amount_out;
                    swaps.push(hop1);
                    match find_pool_fee(client.clone(), cfg, usdg, weth).await {
                        Ok(Some(fee)) => match swap_single(client.clone(), cfg, usdg, weth, fee, usdg_out).await {
                            Ok(r) => swaps.push(r),
                            Err(e) => failed_legs.push(FailedLeg { stuck_token: usdg, stuck_amount: usdg_out, reason: e.to_string() }),
                        },
                        Ok(None) => failed_legs.push(FailedLeg {
                            stuck_token: usdg,
                            stuck_amount: usdg_out,
                            reason: "no USDG/WETH pool found to complete the two-hop swap".to_string(),
                        }),
                        Err(e) => failed_legs.push(FailedLeg { stuck_token: usdg, stuck_amount: usdg_out, reason: e.to_string() }),
                    }
                }
                Err(e) => failed_legs.push(FailedLeg { stuck_token: token, stuck_amount: amount, reason: e.to_string() }),
            }
        } else {
            failed_legs.push(FailedLeg {
                stuck_token: token,
                stuck_amount: amount,
                reason: "pool doesn't pair with WETH or USDG on either side — no auto-route available".to_string(),
            });
        }
    }

    Ok(AutoSwapOutcome { swaps, failed_legs })
}

/// Swaps both legs of a just-closed position into USDG (the reference
/// stable asset configured as `chain.usdc_address`). Any leg that's already
/// USDG is left alone. Any leg that's WETH swaps directly to USDG. Any other
/// token routes through the pool it was just paired with — first trying a
/// direct pool to USDG, then falling back to a two-hop WETH bridge — since
/// screening already guarantees that pairing exists (that's the pool this
/// bot just exited).
///
/// **Force-close behavior**: if a token has turned into a honeypot (or
/// otherwise can't be swapped) since it was screened, that leg's failure is
/// isolated — it's recorded in `failed_legs` and left in the wallet as-is,
/// rather than causing the whole close to error out and potentially leave
/// funds stuck mid-transaction. The other leg (typically WETH/USDG, which is
/// where almost all the real value usually is) still gets swapped normally.
pub async fn swap_proceeds_to_usdg(
    client: Arc<ChainClient>,
    cfg: &AppConfig,
    token0: Address,
    amount0: U256,
    token1: Address,
    amount1: U256,
    position_fee: u32,
) -> Result<AutoSwapOutcome> {
    let usdg = Address::from_str(&cfg.chain.usdc_address)?;
    let weth = Address::from_str(&cfg.chain.weth_address)?;
    let mut swaps = Vec::new();
    let mut failed_legs = Vec::new();

    for (token, amount, other_token) in [(token0, amount0, token1), (token1, amount1, token0)] {
        if amount.is_zero() || token == usdg {
            continue; // nothing to do, or already the target asset
        }

        if token == weth {
            // Direct WETH -> USDG leg.
            match find_pool_fee(client.clone(), cfg, weth, usdg).await {
                Ok(Some(fee)) => match swap_single(client.clone(), cfg, token, usdg, fee, amount).await {
                    Ok(r) => swaps.push(r),
                    Err(e) => failed_legs.push(FailedLeg { stuck_token: weth, stuck_amount: amount, reason: e.to_string() }),
                },
                Ok(None) => failed_legs.push(FailedLeg {
                    stuck_token: weth,
                    stuck_amount: amount,
                    reason: "no WETH/USDG pool found".to_string(),
                }),
                Err(e) => failed_legs.push(FailedLeg { stuck_token: weth, stuck_amount: amount, reason: e.to_string() }),
            }
            continue;
        }

        // Non-reference token: it was paired with `other_token` in the pool
        // we just closed, which screening guarantees is WETH or USDG.
        if other_token == usdg {
            match swap_single(client.clone(), cfg, token, usdg, position_fee, amount).await {
                Ok(r) => swaps.push(r),
                Err(e) => failed_legs.push(FailedLeg { stuck_token: token, stuck_amount: amount, reason: e.to_string() }),
            }
        } else if other_token == weth {
            // Two-hop: token -> WETH (via the pool we just exited, so this
            // route is guaranteed to exist) -> USDG. If the first hop fails,
            // the original token is what's stuck. If the first hop succeeds
            // but the second fails, WETH is what's stuck — still a fine
            // outcome, since WETH is trivially liquid later.
            match swap_single(client.clone(), cfg, token, weth, position_fee, amount).await {
                Ok(hop1) => {
                    let weth_out = hop1.amount_out;
                    swaps.push(hop1);
                    match find_pool_fee(client.clone(), cfg, weth, usdg).await {
                        Ok(Some(fee)) => match swap_single(client.clone(), cfg, weth, usdg, fee, weth_out).await {
                            Ok(r) => swaps.push(r),
                            Err(e) => failed_legs.push(FailedLeg { stuck_token: weth, stuck_amount: weth_out, reason: e.to_string() }),
                        },
                        Ok(None) => failed_legs.push(FailedLeg {
                            stuck_token: weth,
                            stuck_amount: weth_out,
                            reason: "no WETH/USDG pool found to complete the two-hop swap".to_string(),
                        }),
                        Err(e) => failed_legs.push(FailedLeg { stuck_token: weth, stuck_amount: weth_out, reason: e.to_string() }),
                    }
                }
                Err(e) => failed_legs.push(FailedLeg { stuck_token: token, stuck_amount: amount, reason: e.to_string() }),
            }
        } else {
            failed_legs.push(FailedLeg {
                stuck_token: token,
                stuck_amount: amount,
                reason: "pool doesn't pair with WETH or USDG on either side — no auto-route available".to_string(),
            });
        }
    }

    Ok(AutoSwapOutcome { swaps, failed_legs })
}
