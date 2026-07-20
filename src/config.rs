use anyhow::{Context, Result};
use ethers::types::Address;
use serde::Deserialize;
use std::str::FromStr;

#[derive(Debug, Deserialize, Clone)]
pub struct ChainConfig {
    pub rpc_url: String,
    pub chain_id: u64,
    /// Uniswap V3 (or the chain's primary AMM) factory contract address.
    /// Find this from the Robinhood Chain developer docs / Uniswap deployments page,
    /// or by looking up the AMM's factory on the Blockscout explorer.
    pub uniswap_v3_factory: String,
    /// NonfungiblePositionManager contract address for the same AMM deployment.
    pub position_manager: String,
    pub weth_address: String,
    /// Robinhood Chain's native stablecoin is USDG (6 decimals), not USDC —
    /// this field just holds whichever "$1 reference asset" address you want
    /// to price pools against. Keep the field name generic in your head.
    pub usdc_address: String,
    /// Block the factory was deployed at, so we don't scan from genesis.
    pub factory_deployment_block: u64,
    pub blockscout_api_base: String,
    /// Human-facing explorer base URL, used to build tx links in Telegram
    /// messages, e.g. "https://robinhoodchain.blockscout.com"
    pub explorer_base_url: String,
}

#[derive(Debug, Deserialize, Clone)]
pub struct WalletConfig {
    /// Private key for a DEDICATED hot wallet used only for LP entries.
    /// Do NOT put your main wallet's key here. Fund it only with what you're
    /// willing to risk in automated/one-tap transactions.
    pub private_key: String,
    pub default_lp_usd_amount: f64,
    pub slippage_bps: u64,
    pub tick_range_percent: f64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ScreeningConfig {
    pub min_tvl_usd: f64,
    pub min_volume_24h_usd: f64,
    pub min_apr_percent: f64,
    pub max_apr_percent: f64,
    pub min_pool_age_hours: f64,
    pub require_verified_tokens: bool,
    pub poll_interval_secs: u64,
    pub lookback_blocks_for_volume: u64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct TelegramConfig {
    pub bot_token: String,
    pub chat_id: i64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct MonitoringConfig {
    /// Close (with confirmation) once a position's PnL reaches +this percent.
    pub take_profit_percent: f64,
    /// Close (with confirmation) once a position's PnL reaches -this percent.
    pub stop_loss_percent: f64,
    /// How often to re-check open positions for PnL / TP-SL / volume spikes.
    pub position_check_interval_secs: u64,
    /// Alert when recent-window volume is at least this many times the
    /// previous window's volume (e.g. 3.0 = a 3x jump).
    pub volume_spike_multiplier: f64,
    /// Size of each comparison window, in hours (recent vs. the window
    /// immediately before it).
    pub volume_spike_window_hours: f64,
    /// Robinhood Chain runs sub-second blocks; used to convert
    /// volume_spike_window_hours into a block range. Check the explorer for
    /// the current average and adjust if it drifts.
    pub approx_blocks_per_hour: u64,
}

#[derive(Debug, Deserialize, Clone)]
pub struct AppConfig {
    pub chain: ChainConfig,
    pub wallet: WalletConfig,
    pub screening: ScreeningConfig,
    pub monitoring: MonitoringConfig,
    pub telegram: TelegramConfig,
}

impl AppConfig {
    pub fn load(path: &str) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config file at {path}"))?;
        let cfg: AppConfig = toml::from_str(&raw).context("failed to parse config.toml")?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        let placeholder = |s: &str| s.trim().is_empty() || s.contains("0x0000000000000000000000000000000000000000") || s.to_lowercase().contains("todo");

        if placeholder(&self.chain.uniswap_v3_factory) {
            anyhow::bail!(
                "chain.uniswap_v3_factory is not set. Look up the AMM factory address on \
                 robinhoodchain.blockscout.com or the Robinhood Chain developer docs and put it in config.toml."
            );
        }
        if placeholder(&self.chain.position_manager) {
            anyhow::bail!(
                "chain.position_manager is not set. Same source as the factory address."
            );
        }
        Address::from_str(&self.chain.uniswap_v3_factory).context("invalid uniswap_v3_factory address")?;
        Address::from_str(&self.chain.position_manager).context("invalid position_manager address")?;
        Address::from_str(&self.chain.weth_address).context("invalid weth_address")?;
        Address::from_str(&self.chain.usdc_address).context("invalid usdc_address")?;

        if self.wallet.private_key.trim().is_empty() {
            anyhow::bail!("wallet.private_key is not set");
        }
        if self.telegram.bot_token.trim().is_empty() {
            anyhow::bail!("telegram.bot_token is not set");
        }
        Ok(())
    }
}
