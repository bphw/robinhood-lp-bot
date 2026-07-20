# Robinhood Chain LP Screener + Telegram Bot

Screens Uniswap V3 pools on [Robinhood Chain](https://docs.robinhood.com/chain) (chain ID
`4663`) against your criteria, and alerts you on Telegram with a one-tap
button to add liquidity to any pool that passes.

## What it does

1. Polls the Uniswap V3 factory on Robinhood Chain for newly created pools
   (via `PoolCreated` events, read directly on-chain — no subgraph dependency,
   since one doesn't reliably exist yet for a chain this new).
2. For each new pool, computes:
   - **TVL** (via on-chain reserves + the pool's own price, priced against
     WETH/USDG)
   - **24h volume estimate** (by summing `Swap` events over a lookback
     window)
   - **Estimated fee APR** (`volume × fee tier ÷ TVL`, annualized)
   - **Pool age**
   - **Contract verification status** of both tokens (via Blockscout's API)
3. Screens each pool against the thresholds in `config.toml`.
4. If it passes, sends you a Telegram message with an inline **"✅ Add LP
   now"** button.
5. When you tap it, the bot builds and sends the actual `mint()` transaction
   to Uniswap's `NonfungiblePositionManager`, sized at your configured USD
   amount, centered on the pool's current price with your configured range
   and slippage. The resulting position (NFT `tokenId`) is tracked in
   `state.json` going forward.
6. For every position it's tracking, a separate loop (its own configurable
   interval) re-checks:
   - **PnL** — current value of the underlying liquidity plus any
     uncollected fees, minus what you originally put in.
   - **Take-profit / stop-loss** — if PnL crosses either threshold, you get
     a Telegram alert with a **"🔴 Close position"** button. Tapping it
     removes all liquidity and collects both principal and fees back to
     your wallet.
   - **Volume spikes** — if recent-window volume on that position's pool
     jumps well above the window before it, you get an alert (with the same
     close button, since a spike can be a reason to exit either direction).
7. **`/positions`** (or `/pnl`) — message the bot this anytime for a live
   PnL snapshot of every open position, on demand.

## What it does NOT do (yet)

- **No fully-automatic closing.** Every close, whether triggered by
  take-profit, stop-loss, or a volume spike, requires you to tap the
  Telegram button. Nothing sells on your behalf without a tap.
- **Slippage protection on close is now in place.** `close_position`
  computes the expected token amounts for the position's liquidity at the
  current on-chain price (same liquidity math used for PnL), applies
  `wallet.slippage_bps` to get `amount0Min`/`amount1Min`, and sends those to
  `decreaseLiquidity`. If the price has moved beyond your tolerance by the
  time the transaction lands — including from a sandwich attempt — it
  reverts instead of paying out at a worse price.
- **Entry cost basis is approximate.** A new position's `entry_cost_usd` is
  recorded as `wallet.default_lp_usd_amount` (what you asked to add), not
  the exact USD value actually deposited on-chain — these can differ
  slightly due to price impact/slippage at mint time. Good enough for
  PnL-based alerting; not exact accounting.

## Known limitations (from the original build — still apply)

- **Uniswap v2, v3, v4, and UniswapX are all live on Robinhood Chain.** This
  bot only understands **v3-style pools** (a factory that deploys individual
  pool contracts, each with its own address). It will not see v2 pools, and
  it will **not** see Uniswap v4 pools, which live inside a single
  `PoolManager` contract with a completely different architecture (no
  per-pair contracts, no factory `PoolCreated` events, liquidity managed via
  "hooks"). If new pool creation shifts mostly to v4, this bot needs a
  different, v4-specific rewrite to keep seeing them.
- **TVL/volume/APR pricing only works for pools where one side is WETH or
  USDG.** A new pool between two tokens neither of which is WETH/USDG can't
  be priced by this bot and will always fail screening (logged as
  "unpriceable"), even if it's a fine pool. This is a deliberate choice —
  guessing a price without a reference asset would be worse than not
  scoring it at all.
- **Token verification ≠ a safety guarantee.** `require_verified_tokens`
  checks whether the token's source is verified on Blockscout. It does not
  detect honeypots, hidden transfer taxes, mint backdoors, or blacklist
  functions. There's no tx-simulation/honeypot-checking step wired in —
  review any pool manually before sizing up past your default LP amount.
  See the comment in `src/chain/safety.rs` for what a fuller check would
  need.
- **The bot holds a private key and signs real transactions.** Tapping "Add
  LP now" is not a preview — it sends money. Use a **dedicated wallet**,
  funded only with what you're comfortable risking in one-tap automated
  transactions. Never point this at your main wallet.
- **The estimated APR is backward-looking and can be misleading** for very
  new or thin pools — a single large swap can produce a nonsensical
  annualized number. `max_apr_percent` exists specifically to filter out
  implausibly high estimates rather than chase them.
- Contract addresses were confirmed against Uniswap's official deployments
  page as of July 2026. **Re-verify before relying on this with real money**:
  https://developers.uniswap.org/docs/protocols/v3/deployments/v3-robinhood-chain-deployments

## Setup

1. **Install Rust** (1.75+; a current stable via [rustup](https://rustup.rs)
   is recommended — this was built and tested against 1.75 with a handful of
   dependency version pins for compatibility, all of which are safe to
   relax/remove if you're on a newer toolchain — see the comments next to
   each pinned dependency in `Cargo.toml`).

2. **Copy the config template:**
   ```
   cp config.example.toml config.toml
   ```

3. **Fill in `config.toml`:**
   - `chain.factory_deployment_block` — look up the Uniswap V3 factory's
     creation block on
     [Blockscout](https://robinhoodchain.blockscout.com/address/0x1f7d7550b1b028f7571e69a784071f0205fd2efa)
     and use that, so the bot doesn't scan from genesis (Robinhood Chain runs
     ~0.1s blocks, so genesis-to-now is tens of millions of blocks).
   - `wallet.private_key` — a **dedicated** hot wallet, funded with only what
     you're willing to risk. Fund it with a small amount of ETH (for gas) and
     whatever token(s) you want available for LP adds.
   - `telegram.bot_token` — create a bot by messaging
     [@BotFather](https://t.me/BotFather) on Telegram, `/newbot`, and copy the
     token it gives you.
   - `telegram.chat_id` — message [@userinfobot](https://t.me/userinfobot) on
     Telegram to get your numeric chat ID.
   - Adjust the `[screening]` thresholds to taste.

4. **Build and run:**
   ```
   cargo build --release
   ./target/release/robinhood_lp_bot
   ```

   The bot logs to stdout (set `RUST_LOG=debug` for more detail) and keeps a
   small `state.json` file in the working directory to track the last
   scanned block and which pools it's already alerted on, so restarts don't
   re-send old alerts.

## Project layout

```
src/
  config.rs          Loads and validates config.toml
  models.rs           Shared data types (PoolInfo, PoolMetrics, Position, PositionPnl, ScreenResult, VolumeSpike)
  screener.rs         Applies your thresholds, explains why a pool passed/failed
  storage.rs          Tracks last-scanned block, alerted pools, open positions, TP/SL + spike alert dedup
  chain/
    abi.rs            Uniswap V3 + ERC20 + NonfungiblePositionManager bindings
    mod.rs            RPC provider / wallet signer setup
    pools.rs           Discovers pools via PoolCreated events
    pricing.rs          Shared WETH/USDG pricing helpers (used by metrics, position, spike)
    metrics.rs          Computes TVL, volume, APR, age for newly discovered pools
    position.rs         Uniswap V3 liquidity math + PnL computation for open positions
    spike.rs            Volume-spike detection (recent window vs. previous window)
    safety.rs          Contract-verification check via Blockscout API
    lp.rs               Builds/sends the add-liquidity and close-position transactions
  telegram/
    mod.rs             Alerts, the "Add LP now" / "Close position" buttons, and the /positions command
  main.rs              Wires up three loops: pool discovery, Telegram dispatcher, position monitoring
```

## Extending this

- **Real honeypot/rug detection**: wire in a simulation step (e.g. quote a
  buy then a sell through the pool via `eth_call` state overrides, or a
  third-party honeypot-checking API once one supports this chain) before
  trusting `require_verified_tokens` alone.
- **Uniswap v4 support**: would need a parallel discovery path against the
  `PoolManager` singleton and its `Initialize`/`Swap` events instead of a
  factory, plus handling pool "hooks."
- **Precise USD sizing in `chain/lp.rs`**: right now the add-liquidity sizing
  approximates a 50/50 split using the pool's own price ratio; wiring in the
  same WETH/USDG pricing helper used in `metrics.rs` would make the USD
  amount exact rather than approximate.
- **Fully-automatic closing**: if you'd rather skip the confirmation tap for
  take-profit/stop-loss, `run_monitoring_cycle` in `main.rs` is the place to
  call `chain::lp::close_position` directly instead of going through
  `telegram::send_tp_sl_alert`.
