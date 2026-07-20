use ethers::types::Address;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolInfo {
    pub address: Address,
    pub token0: Address,
    pub token1: Address,
    pub fee: u32,
    pub created_block: u64,
    pub created_timestamp: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PoolMetrics {
    pub token0_symbol: String,
    pub token1_symbol: String,
    /// None means "could not be priced" (neither side is WETH/a stablecoin
    /// we know how to value) — such pools are excluded from TVL/APR-based
    /// screening rather than silently passed.
    pub tvl_usd: Option<f64>,
    pub volume_24h_usd: Option<f64>,
    pub apr_percent: Option<f64>,
    pub age_hours: f64,
    pub token0_verified: Option<bool>,
    pub token1_verified: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct PoolCandidate {
    pub info: PoolInfo,
    pub metrics: PoolMetrics,
}

#[derive(Debug, Clone)]
pub struct ScreenResult {
    pub candidate: PoolCandidate,
    pub passed: bool,
    /// Human-readable reasons, used both for the Telegram alert and for logs
    /// when a pool fails (so you can see why without re-deriving it by hand).
    pub reasons: Vec<String>,
}
