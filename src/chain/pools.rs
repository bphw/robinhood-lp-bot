use super::abi::UniswapV3Factory;
use super::ChainClient;
use crate::models::PoolInfo;
use anyhow::{Context, Result};
use ethers::middleware::Middleware;
use ethers::types::Address;
use std::str::FromStr;
use std::sync::Arc;

/// Most public RPC providers cap eth_getLogs to a limited block range per call.
/// Scan in chunks so we don't get rejected on chains with a long history.
const LOG_CHUNK_SIZE: u64 = 5_000;

/// Scan for pools created between `from_block` and the current chain head.
/// Returns the discovered pools and the block number scanning stopped at,
/// so the caller can persist it and resume from there next time.
pub async fn discover_new_pools(
    client: Arc<ChainClient>,
    factory_address: &str,
    from_block: u64,
) -> Result<(Vec<PoolInfo>, u64)> {
    let factory_addr = Address::from_str(factory_address).context("bad factory address")?;
    let factory = UniswapV3Factory::new(factory_addr, client.clone());

    let latest = client
        .get_block_number()
        .await
        .context("failed to fetch latest block number")?
        .as_u64();

    if from_block >= latest {
        return Ok((vec![], from_block));
    }

    let mut pools = Vec::new();
    let mut start = from_block;

    while start <= latest {
        let end = (start + LOG_CHUNK_SIZE).min(latest);

        let events = factory
            .event::<super::abi::PoolCreatedFilter>()
            .from_block(start)
            .to_block(end)
            .query_with_meta()
            .await
            .with_context(|| format!("querying PoolCreated logs {start}-{end}"))?;

        for (ev, meta) in events {
            let created_block = meta.block_number.as_u64();
            let created_timestamp = match client.get_block(meta.block_number).await {
                Ok(Some(b)) => b.timestamp.as_u64(),
                _ => 0,
            };

            pools.push(PoolInfo {
                address: ev.pool,
                token0: ev.token_0,
                token1: ev.token_1,
                fee: ev.fee,
                created_block,
                created_timestamp,
            });
        }

        start = end + 1;
    }

    Ok((pools, latest))
}

/// Look up a pool directly by its contract address.
/// Used for the manual `/check <address>` command when auto-screening is off.
pub async fn lookup_pool_by_address(
    client: Arc<ChainClient>,
    pool_address: Address,
) -> Result<PoolInfo> {
    let pool = super::abi::UniswapV3Pool::new(pool_address, client);

    let token0 = pool.token_0().call().await.context("fetching token0 from pool")?;
    let token1 = pool.token_1().call().await.context("fetching token1 from pool")?;
    let fee = pool.fee().call().await.context("fetching fee from pool")?;

    Ok(PoolInfo {
        address: pool_address,
        token0,
        token1,
        fee,
        created_block: 0,
        created_timestamp: 0,
    })
}
