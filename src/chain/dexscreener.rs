use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DexScreenerPair {
    pub chain_id: String,
    pub pair_address: String,
    pub base_token: TokenInfo,
    pub quote_token: TokenInfo,
    pub volume: Volume,
    pub liquidity: Liquidity,
    pub pair_created_at: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenInfo {
    pub address: String,
    pub name: String,
    pub symbol: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Volume {
    #[serde(rename = "h24")]
    pub h24: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Liquidity {
    pub usd: f64,
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
