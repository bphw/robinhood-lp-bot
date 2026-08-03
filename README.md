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
   - **Market cap (FDV)** — on-chain `total_supply` × computed USD price of
     the non-reference token. This is fully-diluted, not "circulating" —
     see the caveat below.
   - **24h volume estimate** (by summing `Swap` events over a lookback
     window), plus an **approximate unique-trader count** over the same
     window
   - **Estimated fee APR** (`volume × fee tier ÷ TVL`, annualized)
   - **Pool age**
   - **Contract verification status** of both tokens (via Blockscout's API)
   - **Honeypot check** — simulates a small buy-then-sell round trip via
     Uniswap's Quoter; rejects the pool if the simulated sell reverts or
     loses more than `max_honeypot_loss_percent` to a hidden tax
   - **Holder count, supply concentration, buy/sell tax, mint/ownership/
     blacklist status, and LP-lock status** — via GoPlus Security's Token
     Security API, which supports Robinhood Chain directly. See "GoPlus
     security screening" below for exactly what each field means and its
     limitations.
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
     removes all liquidity, collects both principal and fees, and
     **automatically swaps both tokens into WETH** before confirming — see
     "Auto-swap on close" below.
   - **Volume spikes** — if recent-window volume on that position's pool
     jumps well above the window before it, you get an alert (with the same
     close button, since a spike can be a reason to exit either direction).
7. **`/positions`** (or `/pnl`) — message the bot this anytime for a live
   PnL snapshot of every open position, on demand.
8. **`/dextools_top10`** — fetches the top-ranked pools from DexTools for
   Robinhood chain, filters for Uniswap V3, >=$200k 1h volume, DexScore >=70,
   and <=3 audit issues. Returns up to 10 tokens with compact M/V/L/S/Shield/
   Age readout plus a copyable pool address.
9. **`/uniswap_top10`** — same filtering logic but sourced from DexScreener
   (token-pairs endpoint, no API key needed). Useful when DexTools API is
   unavailable or you want a second opinion.

## GoPlus security screening

Most of the "is this token safe" criteria — holder count, supply
concentration, dev holdings, buy/sell tax, mint function, ownership,
blacklist/pause capability, LP lock — come from
[GoPlus Security's Token Security API](https://gopluslabs.io), called fresh
for each new pool (`src/chain/goplus.rs`). This is far more reliable than
hand-rolling bytecode analysis, and GoPlus does support Robinhood Chain
directly (chain ID `4663`) as of when this was built.

**Two of the criteria you might expect from Solana-token screening tools
don't map directly onto EVM/ERC20 and are translated:**
- "Mint authority not revoked" → `require_not_mintable`: checks for a
  privileged mint function reachable by an owner. ERC20 has no separate
  "authority" concept like an SPL token's mint authority — a contract
  either has an owner-callable mint function coded in, or it doesn't.
- "Freeze authority not revoked" → `require_not_blacklistable`: checks for
  a blacklist or transfer-pause function. Same idea, different mechanism —
  EVM tokens don't have a native freeze primitive, but plenty implement
  their own via a blacklist mapping or a pausable modifier.

**Real limitations, not just theoretical ones:**
- **GoPlus needs to have indexed the token first.** A token that's minutes
  old may have no GoPlus data yet at all. `require_goplus_data` (default
  `true`) controls what happens then: fail every GoPlus-derived check
  conservatively, or skip them and let the pool pass on
  TVL/volume/APR/honeypot alone. Watch your first few real alerts and
  adjust if `true` is rejecting too much of what's actually new/legitimate.
- **LP-lock detection is built for Uniswap v2-style fungible LP tokens.**
  Uniswap v3 positions are NFTs — GoPlus frequently has no lock data for
  them at all, so `lp_locked_pct` will often be `None` even when every
  other GoPlus field is populated. `min_lp_locked_pct` defaults to `0.0`
  (disabled) for exactly this reason; only raise it after checking whether
  GoPlus actually returns lock data for pools on this chain in practice.
  `None` here means "couldn't verify," not "unlocked."
  Combined with `require_goplus_data`, ordering matters:
  `require_goplus_data` gates *missing token data entirely*; a nonzero
  `min_lp_locked_pct` is a separate, additional gate specifically on the
  LP-lock field once token data does exist.
- **The `percent` fields' format is assumed, not independently verified**
  against a live response for a real Robinhood Chain meme token — GoPlus's
  own examples elsewhere (buy_tax/sell_tax) confirm ratios are expressed as
  a fraction of 1 (e.g. `"1"` = 100%), and `top10_holder_pct`/
  `dev_holding_pct` assume the same convention for consistency. Sanity-check
  the first few real alerts against https://gopluslabs.io by hand.
- **"Unique traders (24h)" counts distinct Swap-event `recipient`
  addresses**, not `sender` — for swaps routed through SwapRouter02 (as
  this bot's own are), `sender` is the router contract itself, which would
  make almost every trade look like it came from the same address. Using
  `recipient` is a reasonable proxy for the end-user wallet but can still
  undercount on complex multi-hop routes.
- **Market cap is fully-diluted** (`total_supply × price`), not circulating
  supply. If a meaningful chunk of a token's supply is vested/locked
  elsewhere, this overstates what's actually liquid in the market.

## Auto-swap on close

Every close routes both legs of the position into **WETH** automatically —
that's what you land with in your wallet, not a mix of whatever two tokens
the pool happened to be. No extra confirmation tap; it's part of the same
close action. Routing logic (in `chain/autoswap.rs`):

- A leg that's already WETH: left alone.
- A leg that's USDG: swapped directly, USDG → WETH.
- Any other token: it was paired with WETH or USDG in the pool you just
  exited (screening guarantees this), so it either swaps directly to WETH
  at that same fee tier, or two-hops through USDG if that's what it was
  paired with.

Each swap gets a **QuoterV2 quote first**, then applies `wallet.slippage_bps`
as a real minimum — same slippage-protection principle as the
`decreaseLiquidity` fix, applied here too.

**Force-close: a failing leg doesn't block the rest.** If one side can't be
swapped — most likely because the token turned into a honeypot after it was
screened — that leg's failure is isolated (in `chain/autoswap.rs`) rather
than erroring out the whole close. The other leg (usually where most of the
real value is, since it's WETH/USDG) still gets swapped and confirmed
normally. The Telegram close message tells you explicitly which leg got
stuck, in what token, and why, so you can decide whether to hold it or try
swapping it manually later. Nothing is silently lost — collect() has
already moved everything into your wallet before any swap is attempted; a
failed swap just means that leg stays as its original token instead of
becoming WETH.

**Edge case still not auto-routed**: if a pool's two tokens are neither WETH
nor USDG on either side, there's no route to auto-swap through — that leg
gets reported as a failed/stuck leg the same way a honeypot would. This
shouldn't come up in practice, since screening already excludes pools like
that (they're unpriceable in the first place).

## What it does NOT do (yet)

- **The close itself still isn't fully automatic.** Every close, whether
  triggered by take-profit, stop-loss, or a volume spike, still requires you
  to tap the "Close position" button — nothing sells on your behalf without
  a tap. What *is* automatic is what happens after you tap: liquidity
  removal, fee collection, and the swap into WETH all happen in one
  go, no second confirmation needed for the swap step.
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

## Telegram commands

| Command | What it does |
|---------|-------------|
| `/positions` (or `/pnl`) | Live PnL snapshot of all open positions |
| `/check <address>` | Manual pool verification — scores a single pool the same way auto-screening does |
| `/dexscreener <symbol>` | Quick DexScreener lookup by token symbol or address |
| `/dextools_top10` | Top 10 DexTools pools on Robinhood chain. Filters: Uniswap V3, >=$200k 1h volume, score >=70, audit issues <=3. Requires `dextools_api_key` in config |
| `/uniswap_top10` | Top 10 Uniswap V3 pools from DexScreener (no API key needed). Same filters as above but score is computed locally from liquidity + volume |
| `/toggle_screening` (or `/toggle`) | Enable/disable the auto-screening loop |
| `/screening_status` (or `/status`) | Current bot config, screening state, and command list |

**Format of `/dextools_top10` and `/uniswap_top10` output:**
```
📊 #1 TOKEN / PAIR
M$1.2M V$500K L$850K S85 🛡️ A2d
0xabcd...1234
[📋 Copy pool address]
```
- **M** = Market cap (compact: $K / $M / $B)
- **V** = 1-hour volume
- **L** = Liquidity
- **S** = Score (DexScore for DexTools, computed for Uniswap)
- **🛡️** = Audit shield (number = issue count, plain shield = zero issues)
- **A** = Pool age
- Pool address is raw text (long-press to copy on mobile) plus a "Copy" button

## Security model

**Every incoming Telegram message and button tap is checked against
`telegram.chat_id` before anything runs.** This bot holds a real private key
and executes real transactions, so this check is the entire security
boundary between "only I can trigger trades" and "anyone who finds my bot's
username can." Concretely (in `telegram/mod.rs`):

- Any message from a different chat is logged (`Ignoring message from
  unauthorized chat ...`) and dropped — no command runs.
- Any button tap from a different chat is logged and dropped the same way —
  `add_liquidity` and `close_position` are unreachable from anywhere else.

**What this does NOT protect against:**
- If your `telegram.bot_token` leaks, an attacker still can't act (wrong
  chat ID gets rejected) — but they could spam your bot or see it exists.
  Regenerate the token via @BotFather if you suspect it leaked.
- If your `wallet.private_key` leaks, this check is irrelevant — whoever has
  the key can transact directly on-chain, bypassing Telegram entirely. The
  chat-ID check protects the *bot's* interface, not the wallet itself. This
  is exactly why the wallet should be a dedicated hot wallet with limited
  funds, not your main one.
- This bot doesn't run a lock file to prevent two copies of itself running
  at once against the same Telegram bot token and wallet — doing so would
  cause both a Telegram polling conflict (`409`) and, more importantly,
  transaction nonce collisions if both tried to trade simultaneously. Don't
  run more than one instance against the same config.

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
- **Token verification and the honeypot check are real signals, not a full
  guarantee.** `require_verified_tokens` only confirms the source is
  verified on Blockscout — it says nothing about what that source *does*.
  The honeypot check (`chain/honeypot.rs`) simulates a buy-then-sell round
  trip via Uniswap's Quoter and catches the common case (a sell that
  reverts, or a large hidden tax) — but it exercises the *simulated* call
  path, not a real transaction from your actual wallet. A sufficiently
  adversarial token could in principle behave differently for a real
  transaction than for a simulated one (e.g. gating on `tx.origin`, or on
  transaction history). A fully rigorous check would need state-override
  `eth_call` simulation against your specific wallet address, which isn't
  implemented. Review any pool manually before sizing up past your default
  LP amount, especially on a brand-new/low-liquidity token.
- **If a token turns into a honeypot *after* you're already in a position**,
  closing still isolates the failure per-leg — see "Auto-swap on close"
  above. The stuck leg is left in your wallet and reported, not silently
  lost or blocking the rest of the close.
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
     Telegram to get your numeric chat ID. **This is a security boundary, not
     just an address** — the bot only acts on messages/button-taps from this
     exact chat; everything else is logged and silently ignored (see
     "Security model" below).
   - `dextools_api_key` (optional) — get a key at
     [developer.dextools.io](https://developer.dextools.io) if you want to use
     `/dextools_top10`. Without it, that command returns a config reminder;
     `/uniswap_top10` works with no key.
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
    honeypot.rs          Buy-then-sell round-trip simulation via Quoter
    position.rs         Uniswap V3 liquidity math + PnL computation for open positions
    spike.rs            Volume-spike detection (recent window vs. previous window)
    safety.rs          Contract-verification check via Blockscout API
    lp.rs               Builds/sends the add-liquidity and close-position transactions
    autoswap.rs          Auto-swaps close-position proceeds into WETH (force-close on failure)
    dexscreener.rs       DexScreener API client (search, pair lookup, token-pairs, metrics fallback)
    dextools.rs          DexTools API v2 client (hot pools, pool score, token audit)
    goplus.rs            GoPlus Token Security API client
  telegram/
    mod.rs             Alerts, buttons, /positions, /check, /dexscreener, /dextools_top10, /uniswap_top10
  main.rs              Wires up three loops: pool discovery, Telegram dispatcher, position monitoring
```

## Extending this

- **More rigorous honeypot detection**: the current check (Quoter round-trip
  simulation, see `chain/honeypot.rs`) catches the common cases but exercises
  a simulated call path, not a real transaction from your wallet. A fully
  rigorous check would use state-override `eth_call` simulation against your
  specific address (or a third-party honeypot-checking API/GMGN-style
  service, once one supports this chain).
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
