use anyhow::Result;
use ethers::types::Address;
use serde::Deserialize;

/// Parsed, typed view of GeckoTerminal's token security data from the
/// `/tokens/{address}/info` endpoint. GeckoTerminal provides a weighted
/// `gt_score` (0-100) plus component scores for pool, transaction, creation,
/// info, and holders — plus security flags like honeypot, mint authority,
/// freeze authority, and developer holding percentage.
///
/// A field is None if GeckoTerminal has no data for this token (common for
/// very new tokens, or chains not yet indexed by GeckoTerminal). Treat None
/// as "unknown", not "safe".
#[derive(Debug, Clone, Default)]
pub struct GeckoSecurity {
    /// Overall GeckoTerminal trust score, 0-100. Higher is safer/more
    /// established.
    pub gt_score: Option<f64>,
    /// Component scores that make up gt_score.
    pub gt_score_pool: Option<f64>,
    pub gt_score_transaction: Option<f64>,
    pub gt_score_creation: Option<f64>,
    pub gt_score_info: Option<f64>,
    pub gt_score_holders: Option<f64>,
    /// GeckoTerminal verified flag (team/project submitted info).
    pub gt_verified: Option<bool>,
    /// GeckoTerminal's own honeypot verdict.
    pub is_honeypot: Option<bool>,
    /// Mint authority address, if any. None = no mint authority found.
    pub mint_authority: Option<String>,
    /// Freeze authority address, if any. None = no freeze authority found.
    pub freeze_authority: Option<String>,
    /// Developer/creator wallet address.
    pub developer_address: Option<String>,
    /// Developer holding as percent of supply.
    pub developer_holding_pct: Option<f64>,
    /// Total holder count.
    pub holder_count: Option<u64>,
    /// Top-10 holder concentration as a percent string (e.g. "45.23").
    pub top10_holder_pct: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct RawResponse {
    data: RawTokenData,
}

#[derive(Debug, Deserialize)]
struct RawTokenData {
    attributes: RawTokenAttributes,
}

#[derive(Debug, Deserialize)]
struct RawTokenAttributes {
    #[serde(default)]
    gt_score: Option<f64>,
    #[serde(default)]
    gt_score_details: Option<RawScoreDetails>,
    #[serde(default)]
    gt_verified: Option<bool>,
    #[serde(default)]
    is_honeypot: Option<bool>,
    #[serde(default)]
    mint_authority: Option<String>,
    #[serde(default)]
    freeze_authority: Option<String>,
    #[serde(default)]
    developer_address: Option<String>,
    #[serde(default)]
    developer_holding_percentage: Option<String>,
    #[serde(default)]
    holders: Option<RawHolders>,
}

#[derive(Debug, Deserialize, Default)]
struct RawScoreDetails {
    #[serde(default)]
    pool: Option<f64>,
    #[serde(default)]
    transaction: Option<f64>,
    #[serde(default)]
    creation: Option<f64>,
    #[serde(default)]
    info: Option<f64>,
    #[serde(default)]
    holders: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct RawHolders {
    #[serde(default)]
    count: Option<u64>,
    #[serde(default)]
    distribution_percentage: Option<RawDistribution>,
}

#[derive(Debug, Deserialize)]
struct RawDistribution {
    #[serde(default)]
    top_10: Option<String>,
}

fn parse_pct(s: &Option<String>) -> Option<f64> {
    s.as_ref()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| v.is_finite())
}

/// Fetches GeckoTerminal token info for a single token on a given network.
/// Returns Ok(None) if GeckoTerminal has no data (network not supported,
/// token too new, rate-limited, etc.). Callers should treat None as
/// "unknown" and fall back to another security source.
pub async fn fetch_token_info(network_id: &str, token: Address) -> Result<Option<GeckoSecurity>> {
    let addr_lower = format!("{token:#x}");
    let url = format!(
        "https://api.geckoterminal.com/api/v2/networks/{network_id}/tokens/{addr_lower}/info"
    );

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()?;

    let resp = match client.get(&url).send().await {
        Ok(r) => r,
        Err(e) => {
            log::warn!("GeckoTerminal request failed: {e:?}");
            return Ok(None);
        }
    };

    if resp.status().as_u16() == 429 {
        log::warn!("GeckoTerminal rate limited");
        return Ok(None);
    }
    if !resp.status().is_success() {
        log::warn!(
            "GeckoTerminal returned {} for token {} on network {}",
            resp.status(),
            addr_lower,
            network_id
        );
        return Ok(None);
    }

    let body: RawResponse = match resp.json().await {
        Ok(b) => b,
        Err(e) => {
            log::warn!("GeckoTerminal response didn't parse: {e:?}");
            return Ok(None);
        }
    };

    let raw = body.data.attributes;

    let top10 = raw
        .holders
        .as_ref()
        .and_then(|h| h.distribution_percentage.as_ref())
        .and_then(|d| parse_pct(&d.top_10));

    let dev_pct = parse_pct(&raw.developer_holding_percentage);

    let details = raw.gt_score_details.unwrap_or_default();

    Ok(Some(GeckoSecurity {
        gt_score: raw.gt_score,
        gt_score_pool: details.pool,
        gt_score_transaction: details.transaction,
        gt_score_creation: details.creation,
        gt_score_info: details.info,
        gt_score_holders: details.holders,
        gt_verified: raw.gt_verified,
        is_honeypot: raw.is_honeypot,
        mint_authority: raw.mint_authority,
        freeze_authority: raw.freeze_authority,
        developer_address: raw.developer_address,
        developer_holding_pct: dev_pct,
        holder_count: raw.holders.as_ref().and_then(|h| h.count),
        top10_holder_pct: top10,
    }))
}
