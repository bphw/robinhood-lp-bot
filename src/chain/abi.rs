use ethers::prelude::abigen;

// Minimal human-readable ABIs — only the functions/events this bot actually needs.
// These match the standard Uniswap V3 interfaces, which is what Robinhood Chain's
// day-one AMM partner (Uniswap) deploys. If Robinhood Chain's dedicated AMM diverges
// from the standard Uniswap V3 interface (e.g. it's a v4-style singleton with hooks),
// these signatures will need updating to match — check the verified contract's ABI
// on robinhoodchain.blockscout.com.

abigen!(
    UniswapV3Factory,
    r#"[
        event PoolCreated(address indexed token0, address indexed token1, uint24 indexed fee, int24 tickSpacing, address pool)
        function getPool(address tokenA, address tokenB, uint24 fee) external view returns (address pool)
    ]"#
);

abigen!(
    UniswapV3Pool,
    r#"[
        function token0() external view returns (address)
        function token1() external view returns (address)
        function fee() external view returns (uint24)
        function liquidity() external view returns (uint128)
        function slot0() external view returns (uint160 sqrtPriceX96, int24 tick, uint16 observationIndex, uint16 observationCardinality, uint16 observationCardinalityNext, uint8 feeProtocol, bool unlocked)
        event Swap(address indexed sender, address indexed recipient, int256 amount0, int256 amount1, uint160 sqrtPriceX96, uint128 liquidity, int24 tick)
    ]"#
);

abigen!(
    Erc20,
    r#"[
        function name() external view returns (string)
        function symbol() external view returns (string)
        function decimals() external view returns (uint8)
        function totalSupply() external view returns (uint256)
        function balanceOf(address account) external view returns (uint256)
        function approve(address spender, uint256 amount) external returns (bool)
        function allowance(address owner, address spender) external view returns (uint256)
    ]"#
);

abigen!(
    NonfungiblePositionManager,
    r#"[
        struct MintParams { address token0; address token1; uint24 fee; int24 tickLower; int24 tickUpper; uint256 amount0Desired; uint256 amount1Desired; uint256 amount0Min; uint256 amount1Min; address recipient; uint256 deadline; }
        struct DecreaseLiquidityParams { uint256 tokenId; uint128 liquidity; uint256 amount0Min; uint256 amount1Min; uint256 deadline; }
        struct CollectParams { uint256 tokenId; address recipient; uint128 amount0Max; uint128 amount1Max; }
        function mint(MintParams params) external payable returns (uint256 tokenId, uint128 liquidity, uint256 amount0, uint256 amount1)
        function positions(uint256 tokenId) external view returns (uint96 nonce, address operator, address token0, address token1, uint24 fee, int24 tickLower, int24 tickUpper, uint128 liquidity, uint256 feeGrowthInside0LastX128, uint256 feeGrowthInside1LastX128, uint128 tokensOwed0, uint128 tokensOwed1)
        function decreaseLiquidity(DecreaseLiquidityParams params) external payable returns (uint256 amount0, uint256 amount1)
        function collect(CollectParams params) external payable returns (uint256 amount0, uint256 amount1)
        function burn(uint256 tokenId) external payable
        event IncreaseLiquidity(uint256 indexed tokenId, uint128 liquidity, uint256 amount0, uint256 amount1)
        event DecreaseLiquidity(uint256 indexed tokenId, uint128 liquidity, uint256 amount0, uint256 amount1)
    ]"#
);
