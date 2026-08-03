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
    /// Uniswap SwapRouter02 — used to auto-swap closed-position proceeds
    /// into a single stable asset (USDG).
    pub swap_router: String,
    /// Uniswap QuoterV2 — used to get an accurate expected-output quote
    /// before each auto-swap, so a real slippage minimum can be applied
    /// (rather than swapping with no floor at all).
    pub quoter_v2: String,
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
    pub enabled: bool,
    pub min_tvl_usd: f64,
    pub min_volume_24h_usd: f64,
    pub min_apr_percent: f64,
    pub max_apr_percent: f64,
    pub min_pool_age_hours: f64,
    pub require_verified_tokens: bool,
    /// Reject a pool if the simulated sell reverts, or if a simulated
    /// buy-then-sell round trip loses more than this percent — see
    /// `chain::honeypot`. This is a real check but not a full guarantee
    /// (see the module doc comment there for why).
    pub max_honeypot_loss_percent: f64,
    /// Size (in USD-equivalent of the reference asset) of the simulated
    /// buy used for the honeypot round-trip test. Small enough not to
    /// matter, large enough that quantized/minimum-trade-size tokens still
    /// produce a meaningful quote.
    pub honeypot_test_amount_usd: f64,
    pub poll_interval_secs: u64,
    pub lookback_blocks_for_volume: u64,

    // --- Trader interest / supply concentration / rug-risk (via GoPlus
    // Security, chain::goplus) — all None-tolerant per require_goplus_data
    // below. See README "GoPlus security screening" for what maps to what.
    pub min_holder_count: u64,
    pub min_unique_traders_24h: u64,
    /// Fail if the top 10 holders combined hold more than this percent of
    /// supply.
    pub max_top10_holder_pct: f64,
    /// Fail if the creator/deployer's own holdings exceed this percent.
    pub max_dev_holding_pct: f64,
    pub max_buy_tax_percent: f64,
    pub max_sell_tax_percent: f64,
    /// Require the contract to have no mint function reachable by a
    /// privileged caller (GoPlus `is_mintable` == false).
    pub require_not_mintable: bool,
    /// Require ownership renounced (GoPlus `owner_address` is the zero
    /// address, or there's no privileged owner at all).
    pub require_ownership_renounced: bool,
    /// Fail if GoPlus reports the contract as blacklist-capable or
    /// transfer-pausable — the closest EVM equivalent to a "freeze
    /// authority" that hasn't been revoked.
    pub require_not_blacklistable: bool,
    /// Minimum percent of LP GoPlus reports as locked. Set to 0 to disable
    /// this check entirely — it's frequently unavailable for Uniswap v3
    /// pools (see PoolMetrics::lp_locked_pct), so a nonzero threshold here
    /// combined with require_goplus_data=true will fail most v3 pools.
    pub min_lp_locked_pct: f64,
    /// If true, a pool whose GoPlus data isn't available at all (common for
    /// a token minutes old) fails every GoPlus-derived check above rather
    /// than skipping them. If false, missing data just skips those specific
    /// checks (the pool can still pass on TVL/volume/APR/honeypot alone).
    pub require_goplus_data: bool,
    /// Temporarily hide GoPlus security checks and omit them from Telegram
    /// responses. When true, GoPlus is neither fetched nor mentioned.
    pub hide_goplus: bool,

    // --- GeckoTerminal Security scoring (chain::geckoterminal) — used as
    // primary source when available, with GoPlus as fallback. ---
    /// Minimum overall gt_score (0-100) to pass. Set to 0 to disable.
    pub min_gt_score: f64,
    /// If true, require gt_verified == true (project submitted verified
    /// info to GeckoTerminal).
    pub require_gt_verified: bool,
    /// Which GeckoTerminal network ID to query (e.g. "eth", "bsc"). Set to
    /// the same chain as your LP bot, or to a parent chain if the token
    /// also exists there. Use "" to skip GeckoTerminal entirely.
    pub gecko_network_id: String,
    /// Temporarily hide GeckoTerminal security checks and omit them from
    /// Telegram responses. When true, GeckoTerminal is neither fetched
    /// nor mentioned.
    pub hide_geckoterminal: bool,
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
    /// DEXTools API v2 key (from https://developer.dextools.io). Required
    /// for the `/dextools_top10` command. Leave empty to disable the command.
    #[serde(default)]
    pub dextools_api_key: String,
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
        Address::from_str(&self.chain.swap_router).context("invalid swap_router address")?;
        Address::from_str(&self.chain.quoter_v2).context("invalid quoter_v2 address")?;

        if self.wallet.private_key.trim().is_empty() {
            anyhow::bail!("wallet.private_key is not set");
        }
        if self.telegram.bot_token.trim().is_empty() {
            anyhow::bail!("telegram.bot_token is not set");
        }
        Ok(())
    }
}
