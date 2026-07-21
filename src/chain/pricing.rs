use super::abi::{Erc20, UniswapV3Factory, UniswapV3Pool};
use super::ChainClient;
use crate::config::AppConfig;
use anyhow::Result;
use ethers::types::{Address, U256};
use std::str::FromStr;
use std::sync::Arc;

const STABLE_DECIMALS_FALLBACK: u8 = 6;

/// Common Uniswap V3 fee tiers, checked in this order when looking for
/// whichever pool actually exists between two tokens.
const COMMON_FEE_TIERS: [u32; 4] = [500, 3000, 10000, 100];

/// Finds the fee tier of the first existing pool between `token_a` and
/// `token_b`, checking common fee tiers in order. Returns None if no pool
/// exists between them at any of the checked tiers.
pub async fn find_pool_fee(client: Arc<ChainClient>, cfg: &AppConfig, token_a: Address, token_b: Address) -> Result<Option<u32>> {
    let factory_addr = Address::from_str(&cfg.chain.uniswap_v3_factory)?;
    let factory = UniswapV3Factory::new(factory_addr, client);
    for fee in COMMON_FEE_TIERS {
        let pool_addr = factory.get_pool(token_a, token_b, fee).call().await.unwrap_or_default();
        if pool_addr != Address::zero() {
            return Ok(Some(fee));
        }
    }
    Ok(None)
}

pub async fn eth_price_usd(client: Arc<ChainClient>, cfg: &AppConfig) -> Result<f64> {
    let weth = Address::from_str(&cfg.chain.weth_address)?;
    let usdc = Address::from_str(&cfg.chain.usdc_address)?;

    let Some(fee) = find_pool_fee(client.clone(), cfg, weth, usdc).await? else {
        anyhow::bail!("could not find a WETH/USDC pool to price ETH from");
    };

    let factory_addr = Address::from_str(&cfg.chain.uniswap_v3_factory)?;
    let factory = UniswapV3Factory::new(factory_addr, client.clone());
    let pool_addr = factory.get_pool(weth, usdc, fee).call().await?;
    let pool = UniswapV3Pool::new(pool_addr, client.clone());
    let (t0, _t1) = (pool.token_0().call().await?, pool.token_1().call().await?);
    let slot0 = pool.slot_0().call().await?;
    let sqrt_price_x96 = slot0.0;

    let (weth_decimals, usdc_decimals) = (18u8, STABLE_DECIMALS_FALLBACK);
    let price_1_per_0 = sqrt_price_x96_to_price(sqrt_price_x96, weth_decimals, usdc_decimals, t0 == weth);

    // price_1_per_0 here is defined as "USD per WETH" once we account for
    // which token is which — see sqrt_price_x96_to_price for the convention.
    Ok(price_1_per_0)
}

/// Converts a Uniswap V3 slot0 sqrtPriceX96 into a human-readable price of
/// "USD-like token per ETH-like token", assuming `weth_is_token0` tells us
/// which side of the pool WETH sits on. decimals0/1 must match token0/token1
/// respectively as returned by the pool contract.
pub fn sqrt_price_x96_to_price(sqrt_price_x96: U256, decimals0: u8, decimals1: u8, weth_is_token0: bool) -> f64 {
    let sqrt_price = sqrt_price_x96.as_u128() as f64 / (2f64.powi(96));
    let raw_price_1_per_0 = sqrt_price * sqrt_price; // token1 per token0, raw units
    let adj = 10f64.powi(decimals0 as i32 - decimals1 as i32);
    let price_1_per_0_human = raw_price_1_per_0 * adj; // human token1 per human token0

    if weth_is_token0 {
        // price_1_per_0_human = USDC per WETH = USD per ETH directly.
        price_1_per_0_human
    } else {
        // token0 is USDC, token1 is WETH: price_1_per_0_human = WETH per USDC.
        // Invert to get USD per ETH.
        1.0 / price_1_per_0_human
    }
}

pub async fn token_info(client: Arc<ChainClient>, token: Address) -> Result<(String, u8)> {
    let erc20 = Erc20::new(token, client);
    let symbol = erc20.symbol().call().await.unwrap_or_else(|_| "???".to_string());
    let decimals = erc20.decimals().call().await.unwrap_or(18);
    Ok((symbol, decimals))
}

/// Derives the USD price of both sides of a pool, given the pool's current
/// price and knowledge of WETH/USDG (the "$1 reference" configured in
/// chain.usdc_address). Returns None if neither side of the pool is a token
/// we know how to price.
pub async fn price_pool_tokens(
    client: Arc<ChainClient>,
    cfg: &AppConfig,
    pool_addr: Address,
    token0: Address,
    token1: Address,
    decimals0: u8,
    decimals1: u8,
    eth_usd: f64,
) -> Option<(f64, f64)> {
    let weth = Address::from_str(&cfg.chain.weth_address).ok()?;
    let usdc = Address::from_str(&cfg.chain.usdc_address).ok()?;

    let (known_side_is_0, known_price_usd) = if token0 == weth {
        (true, eth_usd)
    } else if token1 == weth {
        (false, eth_usd)
    } else if token0 == usdc {
        (true, 1.0)
    } else if token1 == usdc {
        (false, 1.0)
    } else {
        return None;
    };

    let pool = UniswapV3Pool::new(pool_addr, client);
    let slot0 = pool.slot_0().call().await.ok()?;
    let sqrt_price_x96 = slot0.0;
    let sqrt_price = sqrt_price_x96.as_u128() as f64 / 2f64.powi(96);
    let raw_price_1_per_0 = sqrt_price * sqrt_price;
    let price_1_per_0_human = raw_price_1_per_0 * 10f64.powi(decimals0 as i32 - decimals1 as i32);

    if known_side_is_0 {
        let price1 = known_price_usd / price_1_per_0_human;
        Some((known_price_usd, price1))
    } else {
        let price0 = known_price_usd * price_1_per_0_human;
        Some((price0, known_price_usd))
    }
}

pub fn i256_to_f64(v: ethers::types::I256) -> f64 {
    // I256 -> f64 via string round-trip keeps this simple and safe against
    // overflow; precision loss is acceptable for volume/PnL estimates.
    v.to_string().parse::<f64>().unwrap_or(0.0)
}
