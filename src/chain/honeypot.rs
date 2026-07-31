use super::abi::{QuoteExactInputSingleParams, QuoterV2};
use super::ChainClient;
use crate::config::AppConfig;
use anyhow::Result;
use ethers::types::{Address, U256};
use std::str::FromStr;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct HoneypotCheck {
    /// False if the simulated sell reverted outright — the strongest signal
    /// something is wrong (a transfer-blocking or sell-disabled token).
    pub sellable: bool,
    /// Round-trip loss from simulating "buy `test_amount_in` of the
    /// reference asset worth of token, then immediately sell it back" — a
    /// legitimate token with a normal 0.3%-ish fee tier loses a few percent
    /// to pool fees and slippage; a token with a large hidden sell tax loses
    /// far more. None if `sellable` is false (no meaningful number to give).
    pub round_trip_loss_percent: Option<f64>,
}

/// Simulates a small buy-then-sell round trip for `token` (paired with
/// `reference`, e.g. WETH or USDG, at `fee`) using QuoterV2 — the same
/// underlying idea as a manual "buy 0.01 ETH, try to sell it back" honeypot
/// test, done via simulation rather than a real transaction.
///
/// This is a real, meaningful check but not a full guarantee: it exercises
/// the *router's* swap path, which is exactly what this bot's own
/// auto-swap-on-close relies on — but a sufficiently adversarial token could
/// in principle behave differently for a simulated call vs. a real one (e.g.
/// gating on `tx.origin`, or on which addresses have transacted before). A
/// fully rigorous check would require state-override `eth_call` simulation
/// against a specific sender address, which isn't implemented here.
pub async fn check_honeypot(
    client: Arc<ChainClient>,
    cfg: &AppConfig,
    token: Address,
    reference: Address,
    fee: u32,
    test_amount_in: U256,
) -> Result<HoneypotCheck> {
    let quoter_addr = Address::from_str(&cfg.chain.quoter_v2)?;
    let quoter = QuoterV2::new(quoter_addr, client.clone());

    let buy_quote = quoter
        .quote_exact_input_single(QuoteExactInputSingleParams {
            token_in: reference,
            token_out: token,
            amount_in: test_amount_in,
            fee,
            sqrt_price_limit_x96: U256::zero(),
        })
        .call()
        .await;

    let Ok(buy_quote) = buy_quote else {
        // Can't even simulate buying it — treat conservatively as not
        // sellable rather than assuming it's fine.
        return Ok(HoneypotCheck { sellable: false, round_trip_loss_percent: None });
    };
    let token_amount = buy_quote.0;
    if token_amount.is_zero() {
        return Ok(HoneypotCheck { sellable: false, round_trip_loss_percent: None });
    }

    let sell_quote = quoter
        .quote_exact_input_single(QuoteExactInputSingleParams {
            token_in: token,
            token_out: reference,
            amount_in: token_amount,
            fee,
            sqrt_price_limit_x96: U256::zero(),
        })
        .call()
        .await;

    let Ok(sell_quote) = sell_quote else {
        // Buying simulates fine but selling reverts — the classic honeypot
        // signature.
        return Ok(HoneypotCheck { sellable: false, round_trip_loss_percent: None });
    };
    let reference_back = sell_quote.0;

    let in_h = test_amount_in.as_u128() as f64;
    let back_h = reference_back.as_u128() as f64;
    let loss_percent = if in_h > 0.0 { ((in_h - back_h) / in_h * 100.0).max(0.0) } else { 0.0 };

    Ok(HoneypotCheck { sellable: true, round_trip_loss_percent: Some(loss_percent) })
}
