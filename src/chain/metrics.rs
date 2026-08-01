use super::abi::Erc20;
use super::pricing::{eth_price_usd, i256_to_f64, price_pool_tokens, token_info};
use super::safety::is_contract_verified;
use super::ChainClient;
use crate::config::AppConfig;
use crate::models::{PoolInfo, PoolMetrics};
use anyhow::{Context, Result};
use ethers::types::Address;
use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;

const LOG_CHUNK_SIZE: u64 = 5_000;

/// Result of scanning Swap events over a block range: total USD volume, plus
/// the distinct `recipient` addresses seen — an approximation of unique
/// traders (see `estimate_volume_usd` for the caveat on why `recipient`
/// rather than `sender` is used).
#[derive(Debug, Clone, Default)]
pub struct VolumeWindow {
    pub volume_usd: f64,
    pub unique_traders: u64,
}

/// Figures out which side of a pool is the "subject" token — the
/// non-reference one screening actually cares about — since WETH/USDG
/// themselves don't need honeypot/security checks. Returns None if both
/// sides are reference assets (nothing to check) or addresses don't parse.
fn subject_token(cfg: &AppConfig, pool: &PoolInfo) -> Option<Address> {
    let weth = Address::from_str(&cfg.chain.weth_address).ok()?;
    let usdg = Address::from_str(&cfg.chain.usdc_address).ok()?;
    if pool.token0 != weth && pool.token0 != usdg {
        Some(pool.token0)
    } else if pool.token1 != weth && pool.token1 != usdg {
        Some(pool.token1)
    } else {
        None
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

    let mut metrics = PoolMetrics {
        token0_symbol: sym0,
        token1_symbol: sym1,
        age_hours,
        token0_verified,
        token1_verified,
        ..Default::default()
    };

    if let Some((p0, p1)) = prices {
        let erc0 = Erc20::new(pool.token0, client.clone());
        let erc1 = Erc20::new(pool.token1, client.clone());
        let bal0 = erc0.balance_of(pool.address).call().await.unwrap_or_default();
        let bal1 = erc1.balance_of(pool.address).call().await.unwrap_or_default();

        let bal0_h = bal0.as_u128() as f64 / 10f64.powi(dec0 as i32);
        let bal1_h = bal1.as_u128() as f64 / 10f64.powi(dec1 as i32);
        let tvl = bal0_h * p0 + bal1_h * p1;

        let lookback = cfg.screening.lookback_blocks_for_volume;
        let from_block = current_block.saturating_sub(lookback).max(pool.created_block);
        let window = estimate_volume_usd(client.clone(), pool.address, from_block, current_block, dec0, dec1, p0, p1)
            .await
            .unwrap_or_default();

        let fee_fraction = pool.fee as f64 / 1_000_000.0;
        let apr = if tvl > 0.0 { (window.volume_usd * fee_fraction / tvl) * 365.0 * 100.0 } else { 0.0 };

        metrics.tvl_usd = Some(tvl);
        metrics.volume_24h_usd = Some(window.volume_usd);
        metrics.apr_percent = Some(apr);
        metrics.unique_traders_24h = Some(window.unique_traders);

        let (sellable, loss_pct) = run_honeypot_check(client.clone(), cfg, pool, dec0, dec1, p0, p1).await;
        metrics.honeypot_sellable = sellable;
        metrics.honeypot_round_trip_loss_percent = loss_pct;

        if let Some(subject) = subject_token(cfg, pool) {
            let (subject_price, subject_decimals) =
                if subject == pool.token0 { (p0, dec0) } else { (p1, dec1) };
            if subject_price > 0.0 {
                let subject_erc20 = Erc20::new(subject, client.clone());
                if let Ok(total_supply) = subject_erc20.total_supply().call().await {
                    let supply_h = total_supply.as_u128() as f64 / 10f64.powi(subject_decimals as i32);
                    metrics.market_cap_usd = Some(supply_h * subject_price);
                }
            }

            // --- Hybrid security fetch: GeckoTerminal primary, GoPlus fallback ---
            let gecko_network = cfg.screening.gecko_network_id.trim();
            let mut gecko_fetched = false;
            if !gecko_network.is_empty() && !cfg.screening.hide_geckoterminal {
                match super::geckoterminal::fetch_token_info(gecko_network, subject).await {
                    Ok(Some(gsec)) => {
                        gecko_fetched = true;
                        metrics.gt_score = gsec.gt_score;
                        metrics.gt_score_pool = gsec.gt_score_pool;
                        metrics.gt_score_transaction = gsec.gt_score_transaction;
                        metrics.gt_score_creation = gsec.gt_score_creation;
                        metrics.gt_score_info = gsec.gt_score_info;
                        metrics.gt_score_holders = gsec.gt_score_holders;
                        metrics.gt_verified = gsec.gt_verified;
                        metrics.gecko_is_honeypot = gsec.is_honeypot;
                        metrics.gecko_developer_holding_pct = gsec.developer_holding_pct;
                        metrics.gecko_holder_count = gsec.holder_count;
                        metrics.gecko_top10_holder_pct = gsec.top10_holder_pct;

                        // Fill GoPlus-style fallback fields from GeckoTerminal data
                        // so the screener works with either source seamlessly.
                        metrics.holder_count = gsec.holder_count;
                        metrics.top10_holder_pct = gsec.top10_holder_pct;
                        metrics.dev_holding_pct = gsec.developer_holding_pct;
                        // GeckoTerminal doesn't expose buy/sell tax directly —
                        // leave those as None so GoPlus can fill them if available.
                        metrics.is_honeypot_goplus = gsec.is_honeypot;
                        // mint_authority == None → not mintable; Some → mintable
                        metrics.is_mintable = gsec.mint_authority.as_ref().map(|a| !a.is_empty() && a != "0x0000000000000000000000000000000000000000");
                        // freeze_authority == None → not blacklistable (best EVM proxy)
                        metrics.is_blacklistable = gsec.freeze_authority.as_ref().map(|a| !a.is_empty() && a != "0x0000000000000000000000000000000000000000");
                        // ownership_renounced: true if no developer address or empty/zero
                        metrics.ownership_renounced = gsec.developer_address.as_ref().map(|a| a.is_empty() || a == "0x0000000000000000000000000000000000000000");
                        // Now move the String fields into metrics.
                        metrics.gecko_mint_authority = gsec.mint_authority;
                        metrics.gecko_freeze_authority = gsec.freeze_authority;
                        metrics.gecko_developer_address = gsec.developer_address;
                        log::info!(
                            "GeckoTerminal gt_score={:.1} for token {:?} on network {}",
                            gsec.gt_score.unwrap_or(0.0),
                            subject,
                            gecko_network
                        );
                    }
                    Ok(None) => {
                        log::info!("GeckoTerminal has no data for token {:?} on network {}", subject, gecko_network);
                    }
                    Err(e) => log::warn!("GeckoTerminal lookup failed for {:?}: {e:?}", subject),
                }
            }

            // GoPlus fallback: only fetch fields GeckoTerminal didn't provide,
            // or fetch everything if GeckoTerminal wasn't queried / had no data.
            if !cfg.screening.hide_goplus {
                match super::goplus::fetch_token_security(cfg.chain.chain_id, subject).await {
                    Ok(Some(sec)) => {
                        if !gecko_fetched {
                            // No GeckoTerminal data at all — populate all fields from GoPlus.
                            metrics.holder_count = sec.holder_count;
                            metrics.top10_holder_pct = sec.top10_holder_pct;
                            metrics.dev_holding_pct = sec.dev_holding_pct;
                            metrics.buy_tax_percent = sec.buy_tax_percent;
                            metrics.sell_tax_percent = sec.sell_tax_percent;
                            metrics.is_mintable = sec.is_mintable;
                            metrics.ownership_renounced = sec.ownership_renounced;
                            metrics.is_honeypot_goplus = sec.is_honeypot;
                            metrics.is_blacklistable = sec.is_blacklistable;
                            metrics.transfer_pausable = sec.transfer_pausable;
                            metrics.lp_locked_pct = sec.lp_locked_pct;
                        } else {
                            // GeckoTerminal provided some fields; fill only the ones
                            // GeckoTerminal doesn't expose (buy/sell tax, LP lock).
                            metrics.buy_tax_percent = metrics.buy_tax_percent.or(sec.buy_tax_percent);
                            metrics.sell_tax_percent = metrics.sell_tax_percent.or(sec.sell_tax_percent);
                            metrics.lp_locked_pct = metrics.lp_locked_pct.or(sec.lp_locked_pct);
                            // For bool fields, prefer GeckoTerminal when it has data,
                            // but fall back to GoPlus if GeckoTerminal inferred them
                            // from mint_authority/freeze_authority and GoPlus has actual
                            // contract analysis.
                            metrics.is_mintable = metrics.is_mintable.or(sec.is_mintable);
                            metrics.is_blacklistable = metrics.is_blacklistable.or(sec.is_blacklistable);
                            metrics.transfer_pausable = metrics.transfer_pausable.or(sec.transfer_pausable);
                        }
                    }
                    Ok(None) => {
                        log::info!("No GoPlus data yet for token {subject:?} — leaving security fields unknown");
                    }
                    Err(e) => log::warn!("GoPlus lookup failed for {subject:?}: {e:?}"),
                }
            }
        }
    }

    Ok(metrics)
}

/// Runs the buy-then-sell round trip simulation on whichever side of the
/// pool is the non-reference token. Returns (None, None) if both sides are
/// reference assets — nothing to test.
async fn run_honeypot_check(
    client: Arc<ChainClient>,
    cfg: &AppConfig,
    pool: &PoolInfo,
    dec0: u8,
    dec1: u8,
    p0: f64,
    p1: f64,
) -> (Option<bool>, Option<f64>) {
    use ethers::types::U256;

    let Ok(weth) = Address::from_str(&cfg.chain.weth_address) else { return (None, None) };
    let Ok(usdg) = Address::from_str(&cfg.chain.usdc_address) else { return (None, None) };

    let (test_token, reference, ref_price, ref_decimals) = if pool.token0 != weth && pool.token0 != usdg {
        (pool.token0, pool.token1, p1, dec1)
    } else if pool.token1 != weth && pool.token1 != usdg {
        (pool.token1, pool.token0, p0, dec0)
    } else {
        return (None, None); // both sides are reference assets, nothing to test
    };

    if ref_price <= 0.0 {
        return (None, None);
    }
    let test_amount_human = cfg.screening.honeypot_test_amount_usd / ref_price;
    let test_amount_raw = U256::from((test_amount_human * 10f64.powi(ref_decimals as i32)) as u128);
    if test_amount_raw.is_zero() {
        return (None, None);
    }

    match super::honeypot::check_honeypot(client, cfg, test_token, reference, pool.fee, test_amount_raw).await {
        Ok(check) => (Some(check.sellable), check.round_trip_loss_percent),
        Err(e) => {
            log::warn!("Honeypot check failed for pool {:?}: {e:?}", pool.address);
            (None, None)
        }
    }
}

/// Sums swap volume in USD over an arbitrary block range `[from_block,
/// to_block]`, using whichever side of the pool has a known price, and
/// counts distinct `recipient` addresses as an approximation of unique
/// traders. Shared by the 24h-volume calculation here and by the
/// volume-spike detector in `spike.rs`, which calls this twice with two
/// different windows (and ignores the trader count).
///
/// `recipient` is used rather than `sender` because for swaps routed
/// through SwapRouter02 (as this bot's own swaps are), `sender` is the
/// router contract itself — nearly every trade would show the same
/// "sender" and the count would be meaninglessly low. `recipient` is
/// usually the actual end-user wallet, though multi-hop routes can still
/// undercount (an intermediate hop's recipient may be a router, not the
/// trader) — this is an approximation, not an exact count.
pub async fn estimate_volume_usd(
    client: Arc<ChainClient>,
    pool_address: Address,
    from_block: u64,
    to_block: u64,
    dec0: u8,
    dec1: u8,
    p0: f64,
    p1: f64,
) -> Result<VolumeWindow> {
    let pool_contract = super::abi::UniswapV3Pool::new(pool_address, client.clone());

    let mut total_usd = 0.0f64;
    let mut traders: HashSet<Address> = HashSet::new();
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
            traders.insert(ev.recipient);
        }

        start = end + 1;
    }

    Ok(VolumeWindow { volume_usd: total_usd, unique_traders: traders.len() as u64 })
}
