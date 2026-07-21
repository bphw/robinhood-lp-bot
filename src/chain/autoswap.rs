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

/// Swaps both legs of a just-closed position into USDG (the reference
/// stable asset configured as `chain.usdc_address`). Any leg that's already
/// USDG is left alone. Any leg that's WETH swaps directly to USDG. Any other
/// token routes through the pool it was just paired with — first trying a
/// direct pool to USDG, then falling back to a two-hop WETH bridge — since
/// screening already guarantees that pairing exists (that's the pool this
/// bot just exited).
///
/// Returns one `SwapResult` per swap actually performed (0, 1, or 2 — a
/// position with a WETH or USDG leg needs at most one swap per non-stable
/// side; a two-hop bridge produces two).
pub async fn swap_proceeds_to_usdg(
    client: Arc<ChainClient>,
    cfg: &AppConfig,
    token0: Address,
    amount0: U256,
    token1: Address,
    amount1: U256,
    position_fee: u32,
) -> Result<Vec<SwapResult>> {
    let usdg = Address::from_str(&cfg.chain.usdc_address)?;
    let weth = Address::from_str(&cfg.chain.weth_address)?;
    let mut results = Vec::new();

    for (token, amount, other_token) in [(token0, amount0, token1), (token1, amount1, token0)] {
        if amount.is_zero() || token == usdg {
            continue; // nothing to do, or already the target asset
        }

        if token == weth {
            // Direct WETH -> USDG leg.
            match find_pool_fee(client.clone(), cfg, weth, usdg).await? {
                Some(fee) => results.push(swap_single(client.clone(), cfg, token, usdg, fee, amount).await?),
                None => anyhow::bail!("no WETH/USDG pool found to swap proceeds through"),
            }
            continue;
        }

        // Non-reference token: it was paired with `other_token` in the pool
        // we just closed, which screening guarantees is WETH or USDG.
        if other_token == usdg {
            results.push(swap_single(client.clone(), cfg, token, usdg, position_fee, amount).await?);
        } else if other_token == weth {
            // Two-hop: token -> WETH (via the pool we just exited, so this
            // route is guaranteed to exist) -> USDG.
            let hop1 = swap_single(client.clone(), cfg, token, weth, position_fee, amount).await?;
            let weth_out = hop1.amount_out;
            results.push(hop1);
            match find_pool_fee(client.clone(), cfg, weth, usdg).await? {
                Some(fee) => results.push(swap_single(client.clone(), cfg, weth, usdg, fee, weth_out).await?),
                None => anyhow::bail!("no WETH/USDG pool found to complete the two-hop swap"),
            }
        } else {
            anyhow::bail!(
                "position pool doesn't pair with WETH or USDG on either side — can't auto-route to USDG"
            );
        }
    }

    Ok(results)
}
