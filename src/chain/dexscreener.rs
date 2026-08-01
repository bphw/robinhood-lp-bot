use crate::models::PoolMetrics;
use crate::config::AppConfig;
use crate::chain::ChainClient;
use anyhow::{Context, Result};
use ethers::types::Address;
use serde::Deserialize;
use std::sync::Arc;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DexScreenerPair {
    pub chain_id: String,
    pub dex_id: String,
    pub url: String,
    pub pair_address: String,
    pub labels: Option<Vec<String>>,
    pub base_token: TokenInfo,
    pub quote_token: TokenInfo,
    pub price_native: String,
    pub price_usd: String,
    pub txns: Txns,
    pub volume: Volume,
    pub price_change: PriceChange,
    pub liquidity: Liquidity,
    pub fdv: Option<f64>,
    pub market_cap: Option<f64>,
    pub pair_created_at: Option<u64>,
    pub info: Option<TokenInfoExtra>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenInfo {
    pub address: String,
    pub name: String,
    pub symbol: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenInfoExtra {
    pub image_url: Option<String>,
    pub websites: Option<Vec<String>>,
    pub socials: Option<Vec<SocialLink>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SocialLink {
    pub r#type: String,
    pub url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Txns {
    pub m5: TxnCounts,
    pub h1: TxnCounts,
    pub h6: TxnCounts,
    pub h24: TxnCounts,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TxnCounts {
    pub buys: u64,
    pub sells: u64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Volume {
    #[serde(rename = "h24")]
    pub h24: f64,
    #[serde(rename = "h6")]
    pub h6: f64,
    #[serde(rename = "h1")]
    pub h1: f64,
    #[serde(rename = "m5")]
    pub m5: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PriceChange {
    #[serde(rename = "h24")]
    pub h24: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Liquidity {
    pub usd: f64,
    pub base: Option<f64>,
    pub quote: Option<f64>,
}

/// DexScreener top-level response wrapper.
/// The API returns either a `"pair"` object or a `"pairs"` array depending on the endpoint variant.
#[derive(Debug, Clone, Deserialize)]
pub struct DexScreenerResponse {
    pub pairs: Option<Vec<DexScreenerPair>>,
    pub pair: Option<DexScreenerPair>,
}

impl DexScreenerResponse {
    pub fn first_pair(self) -> Option<DexScreenerPair> {
        self.pair.or_else(|| self.pairs.and_then(|v| v.into_iter().next()))
    }
}

/// Fetch a single pair from DexScreener by chain slug and pair address.
///
/// Robinhood Chain slug is `"robinhood"`.
pub async fn fetch_pair(chain_id: &str, pair_address: &str) -> Result<Option<DexScreenerPair>> {
    let url = format!(
        "https://api.dexscreener.com/latest/dex/pairs/{}/{}",
        chain_id,
        pair_address
    );
    let resp = reqwest::get(&url)
        .await
        .context("DexScreener request failed")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("DexScreener returned {}: {}", status, body);
    }

    let data: DexScreenerResponse = resp
        .json()
        .await
        .context("DexScreener JSON parse failed")?;

    Ok(data.first_pair())
}

/// Search DexScreener by token name, symbol, or address.
/// Returns all matching pairs (usually 1-10 results).
pub async fn fetch_search(query: &str) -> Result<Vec<DexScreenerPair>> {
    let client = reqwest::Client::new();
    let resp = client
        .get("https://api.dexscreener.com/latest/dex/search")
        .query(&[("q", query)])
        .send()
        .await
        .context("DexScreener search request failed")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("DexScreener search returned {}: {}", status, body);
    }

    let data: DexScreenerResponse = resp
        .json()
        .await
        .context("DexScreener search JSON parse failed")?;

    Ok(data.pairs.unwrap_or_default())
}

/// Try DexScreener first for a pool's TVL / volume / APR / age, then fall
/// back to on-chain metrics.  Even when DexScreener has data we still
/// re-run the on-chain path to fill honeypot + security fields, then merge
/// them into the DexScreener-derived metrics without overwriting TVL / volume.
///
/// This is the same logic used by `/check` and the auto-screener so both
/// paths behave identically.
pub async fn compute_metrics_with_fallback(
    client: Arc<ChainClient>,
    cfg: &AppConfig,
    pool: &crate::models::PoolInfo,
    current_block: u64,
    current_timestamp: u64,
) -> Result<PoolMetrics> {
    let pool_hex = format!("{:#x}", pool.address);

    let mut metrics = match fetch_pair("robinhood", &pool_hex).await {
        Ok(Some(ds)) => {
            let age_hours = ds
                .pair_created_at
                .map(|ms| (current_timestamp as f64 - (ms as f64 / 1000.0)) / 3600.0)
                .unwrap_or(0.0);
            let fee_pct = pool.fee as f64 / 10_000.0;
            let apr = if ds.liquidity.usd > 0.0 {
                Some((ds.volume.h24 * fee_pct / 100.0) / ds.liquidity.usd * 365.0 * 100.0)
            } else {
                None
            };
            let base_addr = ds
                .base_token
                .address
                .parse::<Address>()
                .unwrap_or_default();
            let (token0_sym, token1_sym) = if pool.token0 == base_addr {
                (ds.base_token.symbol.clone(), ds.quote_token.symbol.clone())
            } else {
                (ds.quote_token.symbol.clone(), ds.base_token.symbol.clone())
            };
            PoolMetrics {
                token0_symbol: token0_sym,
                token1_symbol: token1_sym,
                tvl_usd: Some(ds.liquidity.usd),
                volume_24h_usd: Some(ds.volume.h24),
                apr_percent: apr,
                age_hours,
                token0_verified: Some(true),
                token1_verified: Some(true),
                ..Default::default()
            }
        }
        Ok(None) | Err(_) => {
            crate::chain::metrics::compute_metrics(
                client.clone(),
                cfg,
                pool,
                current_block,
                current_timestamp,
            )
            .await?
        }
    };

    // Always merge on-chain security / honeypot fields.
    match crate::chain::metrics::compute_metrics(
        client.clone(),
        cfg,
        pool,
        current_block,
        current_timestamp,
    )
    .await
    {
        Ok(on_chain) => {
            metrics.honeypot_sellable = on_chain.honeypot_sellable;
            metrics.honeypot_round_trip_loss_percent = on_chain.honeypot_round_trip_loss_percent;
            metrics.market_cap_usd = on_chain.market_cap_usd.or(metrics.market_cap_usd);
            metrics.holder_count = on_chain.holder_count.or(metrics.holder_count);
            metrics.top10_holder_pct = on_chain.top10_holder_pct.or(metrics.top10_holder_pct);
            metrics.dev_holding_pct = on_chain.dev_holding_pct.or(metrics.dev_holding_pct);
            metrics.buy_tax_percent = on_chain.buy_tax_percent.or(metrics.buy_tax_percent);
            metrics.sell_tax_percent = on_chain.sell_tax_percent.or(metrics.sell_tax_percent);
            metrics.is_mintable = on_chain.is_mintable.or(metrics.is_mintable);
            metrics.ownership_renounced = on_chain.ownership_renounced.or(metrics.ownership_renounced);
            metrics.is_honeypot_goplus = on_chain.is_honeypot_goplus.or(metrics.is_honeypot_goplus);
            metrics.is_blacklistable = on_chain.is_blacklistable.or(metrics.is_blacklistable);
            metrics.transfer_pausable = on_chain.transfer_pausable.or(metrics.transfer_pausable);
            metrics.lp_locked_pct = on_chain.lp_locked_pct.or(metrics.lp_locked_pct);
            metrics.gt_score = on_chain.gt_score.or(metrics.gt_score);
            metrics.gt_verified = on_chain.gt_verified.or(metrics.gt_verified);
            metrics.gecko_is_honeypot = on_chain.gecko_is_honeypot.or(metrics.gecko_is_honeypot);
            metrics.unique_traders_24h = on_chain.unique_traders_24h.or(metrics.unique_traders_24h);
            metrics.token0_verified = on_chain.token0_verified.or(metrics.token0_verified);
            metrics.token1_verified = on_chain.token1_verified.or(metrics.token1_verified);
            metrics.gecko_mint_authority = on_chain.gecko_mint_authority.or(metrics.gecko_mint_authority);
            metrics.gecko_freeze_authority = on_chain.gecko_freeze_authority.or(metrics.gecko_freeze_authority);
            metrics.gecko_developer_address = on_chain.gecko_developer_address.or(metrics.gecko_developer_address);
            metrics.gecko_developer_holding_pct = on_chain.gecko_developer_holding_pct.or(metrics.gecko_developer_holding_pct);
            metrics.gecko_holder_count = on_chain.gecko_holder_count.or(metrics.gecko_holder_count);
            metrics.gecko_top10_holder_pct = on_chain.gecko_top10_holder_pct.or(metrics.gecko_top10_holder_pct);
            metrics.gt_score_pool = on_chain.gt_score_pool.or(metrics.gt_score_pool);
            metrics.gt_score_transaction = on_chain.gt_score_transaction.or(metrics.gt_score_transaction);
            metrics.gt_score_creation = on_chain.gt_score_creation.or(metrics.gt_score_creation);
            metrics.gt_score_info = on_chain.gt_score_info.or(metrics.gt_score_info);
            metrics.gt_score_holders = on_chain.gt_score_holders.or(metrics.gt_score_holders);
        }
        Err(e) => log::warn!(
            "On-chain metrics fallback failed for pool {}: {e:?}",
            pool_hex
        ),
    }

    Ok(metrics)
}
