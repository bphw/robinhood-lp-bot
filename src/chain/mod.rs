pub mod abi;
pub mod lp;
pub mod metrics;
pub mod pools;
pub mod position;
pub mod pricing;
pub mod safety;
pub mod spike;

use crate::config::AppConfig;
use anyhow::{Context, Result};
use ethers::middleware::SignerMiddleware;
use ethers::providers::{Http, Provider};
use ethers::signers::{LocalWallet, Signer};
use std::sync::Arc;

pub type ChainClient = SignerMiddleware<Provider<Http>, LocalWallet>;

/// Build the provider + wallet signer used everywhere else in the app.
pub async fn build_client(cfg: &AppConfig) -> Result<Arc<ChainClient>> {
    let provider = Provider::<Http>::try_from(cfg.chain.rpc_url.as_str())
        .context("failed to build JSON-RPC provider")?;

    let wallet: LocalWallet = cfg
        .wallet
        .private_key
        .parse::<LocalWallet>()
        .context("failed to parse wallet private key")?
        .with_chain_id(cfg.chain.chain_id);

    log::info!("Wallet address for LP execution: {:?}", wallet.address());

    let client = SignerMiddleware::new(provider, wallet);
    Ok(Arc::new(client))
}
