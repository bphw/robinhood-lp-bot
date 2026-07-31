use anyhow::Result;
use ethers::types::Address;
use serde::Deserialize;
use std::collections::HashMap;

/// Parsed, typed view of the GoPlus fields this bot actually uses. GoPlus
/// returns everything as strings (even booleans, as "0"/"1"), so all the
/// messy parsing lives in this module — callers get clean Option<T>s.
///
/// A field is None either because GoPlus didn't return it, or the value
/// wasn't parseable (e.g. "NaN", which GoPlus does return for some ratios
/// when the denominator is zero). Treat None as "unknown", not "safe".
#[derive(Debug, Clone, Default)]
pub struct GoPlusSecurity {
    pub holder_count: Option<u64>,
    /// Sum of the top-10 holders' share of supply, as a percent (0-100).
    /// NOTE: assumes GoPlus's per-holder `percent` field is a fraction of 1
    /// (consistent with buy_tax/sell_tax, where e.g. "1" = 100% tax) — this
    /// project hasn't been able to verify that against a live response for
    /// a real Robinhood Chain meme token; sanity-check the first few pools
    /// you see against https://gopluslabs.io by hand.
    pub top10_holder_pct: Option<f64>,
    /// Creator/deployer's holdings as a percent of supply.
    pub dev_holding_pct: Option<f64>,
    pub buy_tax_percent: Option<f64>,
    pub sell_tax_percent: Option<f64>,
    pub is_open_source: Option<bool>,
    /// True means the contract has a mint function reachable by some
    /// privileged caller — the closest EVM equivalent to Solana's "mint
    /// authority not revoked". True is a red flag.
    pub is_mintable: Option<bool>,
    /// True means the contract's owner is the zero address (or the
    /// contract has no privileged owner at all) — the closest EVM
    /// equivalent to a revoked authority. False means someone still holds
    /// owner-level control.
    pub ownership_renounced: Option<bool>,
    /// GoPlus's own honeypot verdict — independent of, and a useful
    /// cross-check against, this bot's own on-chain honeypot simulation in
    /// chain::honeypot.
    pub is_honeypot: Option<bool>,
    /// True means the contract can blacklist addresses from
    /// transferring/selling — the closest EVM equivalent to a "freeze
    /// authority".
    pub is_blacklistable: Option<bool>,
    pub transfer_pausable: Option<bool>,
    /// Percent of LP tokens held in addresses GoPlus has identified as
    /// locked (a known locker contract, a timelock, or a burn address).
    /// Only meaningful for Uniswap v2-style fungible LP tokens — Uniswap v3
    /// positions are NFTs and GoPlus generally won't have lock data for
    /// them, so this will often be None for v3 pools even when GoPlus has
    /// data on the token itself. Treat None as "couldn't verify," not "not
    /// locked."
    pub lp_locked_pct: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct RawResponse {
    code: i32,
    #[serde(default)]
    result: HashMap<String, RawTokenSecurity>,
}

#[derive(Debug, Deserialize, Default)]
struct RawHolder {
    #[serde(default)]
    percent: Option<String>,
    #[serde(default)]
    is_locked: Option<i32>,
}

#[derive(Debug, Deserialize, Default)]
struct RawTokenSecurity {
    #[serde(default)]
    holder_count: Option<String>,
    #[serde(default)]
    holders: Option<Vec<RawHolder>>,
    #[serde(default)]
    creator_percent: Option<String>,
    #[serde(default)]
    owner_percent: Option<String>,
    #[serde(default)]
    buy_tax: Option<String>,
    #[serde(default)]
    sell_tax: Option<String>,
    #[serde(default)]
    is_open_source: Option<String>,
    #[serde(default)]
    is_mintable: Option<String>,
    #[serde(default)]
    owner_address: Option<String>,
    #[serde(default)]
    is_honeypot: Option<String>,
    #[serde(default)]
    is_blacklisted: Option<String>,
    #[serde(default)]
    transfer_pausable: Option<String>,
    #[serde(default)]
    lp_holders: Option<Vec<RawHolder>>,
}

fn parse_bool(s: &Option<String>) -> Option<bool> {
    match s.as_deref() {
        Some("1") => Some(true),
        Some("0") => Some(false),
        _ => None,
    }
}

fn parse_pct_fraction(s: &Option<String>) -> Option<f64> {
    s.as_ref()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| v.is_finite())
        .map(|v| v * 100.0)
}

/// Fetches GoPlus's token security data for a single token. Returns Ok(None)
/// — not an error — if GoPlus has no data on this token yet (common for a
/// token minutes old on a chain this new) or the request otherwise doesn't
/// come back cleanly; callers should treat that as "unknown," not fail the
/// whole screening pipeline over an external API being unavailable.
pub async fn fetch_token_security(chain_id: u64, token: Address) -> Result<Option<GoPlusSecurity>> {
    let addr_lower = format!("{token:#x}");
    let url = format!("https://api.gopluslabs.io/api/v1/token_security/{chain_id}?contract_addresses={addr_lower}");

    let resp = match reqwest::get(&url).await {
        Ok(r) => r,
        Err(e) => {
            log::warn!("GoPlus request failed: {e:?}");
            return Ok(None);
        }
    };
    if !resp.status().is_success() {
        return Ok(None);
    }
    let body: RawResponse = match resp.json().await {
        Ok(b) => b,
        Err(e) => {
            log::warn!("GoPlus response didn't parse as expected: {e:?}");
            return Ok(None);
        }
    };
    if body.code != 1 {
        return Ok(None);
    }
    let Some(raw) = body.result.get(&addr_lower) else {
        return Ok(None);
    };

    let top10_holder_pct = raw
        .holders
        .as_ref()
        .map(|holders| holders.iter().take(10).filter_map(|h| parse_pct_fraction(&h.percent)).sum::<f64>());

    let dev_holding_pct = parse_pct_fraction(&raw.creator_percent).or_else(|| parse_pct_fraction(&raw.owner_percent));

    let ownership_renounced = raw.owner_address.as_ref().map(|a| {
        let a = a.to_lowercase();
        a.is_empty() || a == "0x0000000000000000000000000000000000000000"
    });

    let lp_locked_pct = raw.lp_holders.as_ref().map(|holders| {
        holders
            .iter()
            .filter(|h| h.is_locked == Some(1))
            .filter_map(|h| parse_pct_fraction(&h.percent))
            .sum::<f64>()
    });

    Ok(Some(GoPlusSecurity {
        holder_count: raw.holder_count.as_ref().and_then(|v| v.parse().ok()),
        top10_holder_pct,
        dev_holding_pct,
        buy_tax_percent: parse_pct_fraction(&raw.buy_tax),
        sell_tax_percent: parse_pct_fraction(&raw.sell_tax),
        is_open_source: parse_bool(&raw.is_open_source),
        is_mintable: parse_bool(&raw.is_mintable),
        ownership_renounced,
        is_honeypot: parse_bool(&raw.is_honeypot),
        is_blacklistable: parse_bool(&raw.is_blacklisted),
        transfer_pausable: parse_bool(&raw.transfer_pausable),
        lp_locked_pct,
    }))
}
