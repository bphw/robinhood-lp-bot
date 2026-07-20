use ethers::types::Address;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct BlockscoutContractResp {
    #[serde(default)]
    is_verified: Option<bool>,
}

/// Checks whether a contract's source is verified on the chain's Blockscout
/// instance. This is a real, meaningful signal (unverified = you can't read
/// what the token actually does) but it is NOT a full honeypot/rug check.
///
/// True honeypot detection (simulating a buy-then-sell to see if selling is
/// blocked, checking for hidden transfer taxes/blacklists, mint functions,
/// owner-only pause switches, etc.) requires either:
///   - a tx-simulation service for this chain (e.g. Tenderly, or a
///     honeypot-checker API once one exists for Robinhood Chain), or
///   - running your own simulation against a local fork.
/// Neither is wired up here. Treat `require_verified_tokens` as a first
/// filter, not a safety guarantee, and review pools manually before sizing
/// up beyond your default LP amount.
pub async fn is_contract_verified(api_base: &str, address: Address) -> Option<bool> {
    let url = format!("{}/smart-contracts/{:#x}", api_base.trim_end_matches('/'), address);

    let resp = reqwest::get(&url).await.ok()?;
    if resp.status() == reqwest::StatusCode::NOT_FOUND {
        // Blockscout returns 404 for unverified/unknown contracts on some versions.
        return Some(false);
    }
    if !resp.status().is_success() {
        return None;
    }

    let body: BlockscoutContractResp = resp.json().await.ok()?;
    Some(body.is_verified.unwrap_or(false))
}
