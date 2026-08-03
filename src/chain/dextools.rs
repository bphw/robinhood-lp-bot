use anyhow::{Context, Result};
use serde::Deserialize;

/// DexTools API v2 client.
pub struct DexToolsClient {
    api_key: String,
    base_url: String,
    client: reqwest::Client,
}

/// A pool returned by the ranking/hotpools endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct DextoolsRankedPool {
    pub rank: u64,
    pub address: String,
    pub exchange_name: String,
    pub exchange_factory: String,
    pub creation_time: String,
    pub creation_block: u64,
    pub main_token: DextoolsToken,
    pub side_token: DextoolsToken,
    pub fee: f64,
}

/// Minimal token info inside a pool.
#[derive(Debug, Clone, Deserialize)]
pub struct DextoolsToken {
    pub address: String,
    pub symbol: String,
    pub name: String,
}

/// Pool score (dextScore object).
#[derive(Debug, Clone, Deserialize)]
pub struct DextoolsPoolScore {
    pub dext_score: DextScore,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DextScore {
    pub information: f64,
    pub holders: f64,
    pub pool: f64,
    pub transactions: f64,
    pub creation: f64,
    pub total: f64,
}

/// Token audit / security snapshot.
#[derive(Debug, Clone, Deserialize)]
pub struct DextoolsTokenAudit {
    pub is_open_source: String,
    pub is_honeypot: String,
    pub is_mintable: String,
    pub is_proxy: String,
    pub slippage_modifiable: String,
    pub is_blacklisted: String,
    pub sell_tax: DextoolsTax,
    pub buy_tax: DextoolsTax,
    pub is_contract_renounced: String,
    pub is_potentially_scam: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DextoolsTax {
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub status: String,
}

impl DexToolsClient {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: "https://api.dextools.io".to_string(),
            client: reqwest::Client::new(),
        }
    }

    fn auth_header(&self) -> reqwest::header::HeaderValue {
        reqwest::header::HeaderValue::from_str(&format!("Bearer {}", self.api_key))
            .unwrap_or_else(|_| reqwest::header::HeaderValue::from_static(""))
    }

    /// Fetch hot pools for a chain.
    pub async fn fetch_hotpools(&self, chain: &str) -> Result<Vec<DextoolsRankedPool>> {
        let url = format!("{}/v2/ranking/{}/hotpools", self.base_url, chain);
        let resp = self
            .client
            .get(&url)
            .header("x-api-key", self.auth_header())
            .send()
            .await
            .context("DexTools hotpools request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("DexTools hotpools returned {}: {}", status, body);
        }

        let data: Vec<DextoolsRankedPool> = resp
            .json()
            .await
            .context("DexTools hotpools JSON parse failed")?;
        Ok(data)
    }

    /// Fetch the DexScore for a pool.
    pub async fn fetch_pool_score(&self, chain: &str, address: &str) -> Result<DextoolsPoolScore> {
        let url = format!("{}/v2/pool/{}/{}/score", self.base_url, chain, address);
        let resp = self
            .client
            .get(&url)
            .header("x-api-key", self.auth_header())
            .send()
            .await
            .context("DexTools pool score request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("DexTools pool score returned {}: {}", status, body);
        }

        let data: DextoolsPoolScore = resp
            .json()
            .await
            .context("DexTools pool score JSON parse failed")?;
        Ok(data)
    }

    /// Fetch token audit/security info.
    pub async fn fetch_token_audit(&self, chain: &str, address: &str) -> Result<DextoolsTokenAudit> {
        let url = format!("{}/v2/token/{}/{}/audit", self.base_url, chain, address);
        let resp = self
            .client
            .get(&url)
            .header("x-api-key", self.auth_header())
            .send()
            .await
            .context("DexTools token audit request failed")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("DexTools token audit returned {}: {}", status, body);
        }

        let data: DextoolsTokenAudit = resp
            .json()
            .await
            .context("DexTools token audit JSON parse failed")?;
        Ok(data)
    }
}

/// Count "bad" security flags from a DexTools token audit.
/// Returns a number from 0 (clean) to 8 (very risky).
pub fn audit_issue_count(a: &DextoolsTokenAudit) -> u32 {
    let mut issues = 0;
    if a.is_honeypot.to_lowercase() == "yes" {
        issues += 1;
    }
    if a.is_mintable.to_lowercase() == "yes" {
        issues += 1;
    }
    if a.is_proxy.to_lowercase() == "yes" {
        issues += 1;
    }
    if a.slippage_modifiable.to_lowercase() == "yes" {
        issues += 1;
    }
    if a.is_blacklisted.to_lowercase() == "yes" {
        issues += 1;
    }
    if a.is_potentially_scam.to_lowercase() == "yes" {
        issues += 1;
    }
    if a.is_contract_renounced.to_lowercase() == "no" {
        issues += 1;
    }
    if a.is_open_source.to_lowercase() == "no" {
        issues += 1;
    }
    issues
}
