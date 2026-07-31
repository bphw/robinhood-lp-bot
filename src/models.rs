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
    /// Honeypot round-trip simulation result for the non-reference token in
    /// the pair (None if the pool is unpriceable, same condition as
    /// tvl_usd/volume_24h_usd/apr_percent being None).
    pub honeypot_sellable: Option<bool>,
    pub honeypot_round_trip_loss_percent: Option<f64>,
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

/// An LP position this bot opened, tracked from mint until close.
/// token_id is a Uniswap V3 NFT position ID (stored as u64 — position IDs
/// increment sequentially from a counter and won't realistically exceed it).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub token_id: u64,
    pub pool_address: Address,
    pub pool_created_block: u64,
    pub token0: Address,
    pub token1: Address,
    pub token0_symbol: String,
    pub token1_symbol: String,
    pub fee: u32,
    pub tick_lower: i32,
    pub tick_upper: i32,
    pub entry_cost_usd: f64,
    pub entry_timestamp: u64,
    pub mint_tx_hash: String,
    pub closed: bool,
}

/// A point-in-time PnL snapshot for an open position, computed fresh each
/// time — not persisted (the position's current value changes constantly).
#[derive(Debug, Clone)]
pub struct PositionPnl {
    pub current_value_usd: f64,
    pub uncollected_fees_usd: f64,
    pub pnl_usd: f64,
    pub pnl_percent: f64,
    pub in_range: bool,
}

#[derive(Debug, Clone)]
pub struct VolumeSpike {
    pub recent_volume_usd: f64,
    pub previous_volume_usd: f64,
    pub ratio: f64,
}
