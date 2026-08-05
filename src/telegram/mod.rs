use crate::chain::lp::{add_liquidity, close_position};
use crate::chain::position::compute_pnl;
use crate::chain::ChainClient;
use crate::config::AppConfig;
use crate::models::{PoolCandidate, Position, ScreenResult, VolumeSpike};
use anyhow::Result;
use ethers::middleware::Middleware;
use ethers::types::Address;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};
use tokio::sync::Mutex;

use crate::storage::Storage;

fn format_alert(result: &ScreenResult) -> String {
    let m = &result.candidate.metrics;
    let info = &result.candidate.info;

    let mut lines = vec![
        "🟢 *Pool passed screening*".to_string(),
        format!("*Pair:* {}/{}", m.token0_symbol, m.token1_symbol),
        format!("*Address:* `{:#x}`", info.address),
        format!("*Fee tier:* {:.2}%", info.fee as f64 / 10_000.0),
    ];
    if let Some(tvl) = m.tvl_usd {
        lines.push(format!("*TVL:* ${:.0}", tvl));
    }
    if let Some(mcap) = m.market_cap_usd {
        lines.push(format!("*Market cap (FDV):* ${:.0}", mcap));
    }
    if let Some(v) = m.volume_24h_usd {
        lines.push(format!("*24h volume:* ${:.0}", v));
    }
    if let Some(apr) = m.apr_percent {
        lines.push(format!("*Est. fee APR:* {:.1}%", apr));
    }
    if let Some(holders) = m.holder_count {
        lines.push(format!("*Holders:* {holders}"));
    }
    if let Some(traders) = m.unique_traders_24h {
        lines.push(format!("*Unique traders (24h, approx.):* {traders}"));
    }
    if let Some(top10) = m.top10_holder_pct {
        lines.push(format!("*Top-10 holders:* {top10:.1}% of supply"));
    }
    if let Some(dev) = m.dev_holding_pct {
        lines.push(format!("*Dev/creator holdings:* {dev:.1}%"));
    }
    if let (Some(buy), Some(sell)) = (m.buy_tax_percent, m.sell_tax_percent) {
        lines.push(format!("*Buy/sell tax:* {buy:.1}% / {sell:.1}%"));
    }
    if let Some(renounced) = m.ownership_renounced {
        lines.push(format!("*Ownership renounced:* {}", if renounced { "yes" } else { "⚠️ no" }));
    }
    if let Some(mintable) = m.is_mintable {
        lines.push(format!("*Mintable:* {}", if mintable { "⚠️ yes" } else { "no" }));
    }
    if let Some(lp_locked) = m.lp_locked_pct {
        lines.push(format!("*LP locked (GoPlus):* {lp_locked:.1}%"));
    }
    if let (Some(true), Some(loss)) = (m.honeypot_sellable, m.honeypot_round_trip_loss_percent) {
        lines.push(format!("*Honeypot check:* passed (simulated round-trip loss {:.1}%)", loss));
    }
    lines.push(format!("*Age:* {:.1}h", m.age_hours));
    lines.push(String::new());
    lines.push("*Why it passed:*".to_string());
    for r in &result.reasons {
        lines.push(format!("• {r}"));
    }
    lines.push(String::new());
    lines.push(
        "⚠️ This is an automated screen, not financial advice. Verify the pool yourself before \
         adding liquidity — tapping the button below will sign and send a real transaction."
            .to_string(),
    );

    lines.join("\n")
}

pub async fn send_alert(bot: &Bot, chat_id: ChatId, result: &ScreenResult) -> Result<()> {
    let text = format_alert(result);
    let pool_addr = format!("{:#x}", result.candidate.info.address);
    let keyboard = InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback(
        "✅ Add LP now",
        format!("addlp:{pool_addr}"),
    )]]);

    bot.send_message(chat_id, text)
        .parse_mode(teloxide::types::ParseMode::Markdown)
        .reply_markup(keyboard)
        .await?;
    Ok(())
}

fn close_button(token_id: u64) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback(
        "🔴 Close position",
        format!("closepos:{token_id}"),
    )]])
}

pub async fn send_tp_sl_alert(bot: &Bot, chat_id: ChatId, position: &Position, pnl_percent: f64, hit: &str) -> Result<()> {
    let text = format!(
        "🎯 *{hit} hit*\n*Pair:* {}/{}\n*Position:* `#{}`\n*PnL:* {:+.1}%\n\n\
         Tap below to close and collect principal + fees. This sends a real transaction.",
        position.token0_symbol, position.token1_symbol, position.token_id, pnl_percent
    );
    bot.send_message(chat_id, text)
        .parse_mode(teloxide::types::ParseMode::Markdown)
        .reply_markup(close_button(position.token_id))
        .await?;
    Ok(())
}

pub async fn send_spike_alert(bot: &Bot, chat_id: ChatId, position: &Position, spike: &VolumeSpike) -> Result<()> {
    let text = format!(
        "📈 *Volume spike detected*\n*Pair:* {}/{}\n*Position:* `#{}`\n\
         Recent-window volume: ${:.0}\nPrevious window: ${:.0}\n*Ratio:* {:.1}x\n\n\
         Worth a look — could be a good exit or a warning sign depending on direction. \
         Tap below if you'd like to close this position.",
        position.token0_symbol, position.token1_symbol, position.token_id,
        spike.recent_volume_usd, spike.previous_volume_usd, spike.ratio
    );
    bot.send_message(chat_id, text)
        .parse_mode(teloxide::types::ParseMode::Markdown)
        .reply_markup(close_button(position.token_id))
        .await?;
    Ok(())
}

fn format_position_line(position: &Position, pnl_percent: Option<f64>, pnl_usd: Option<f64>, in_range: Option<bool>) -> String {
    let pnl_str = match (pnl_percent, pnl_usd) {
        (Some(pct), Some(usd)) => format!("{:+.1}% ({:+.2} USD)", pct, usd),
        _ => "unavailable".to_string(),
    };
    let range_str = match in_range {
        Some(true) => "in range",
        Some(false) => "OUT OF RANGE",
        None => "unknown",
    };
    format!(
        "*#{}* {}/{} — entry ${:.2}\nPnL: {} · {}",
        position.token_id, position.token0_symbol, position.token1_symbol,
        position.entry_cost_usd, pnl_str, range_str
    )
}

/// Handles the `/positions` command: lists every open position with a
/// freshly computed PnL snapshot.
pub async fn handle_positions_command(
    bot: &Bot,
    chat_id: ChatId,
    cfg: &AppConfig,
    client: Arc<ChainClient>,
    storage: Arc<Mutex<Storage>>,
) -> Result<()> {
    let open = { storage.lock().await.open_positions() };
    if open.is_empty() {
        bot.send_message(chat_id, "No open positions.").await?;
        return Ok(());
    }

    let mut lines = vec!["*Open positions:*".to_string(), String::new()];
    for p in &open {
        match compute_pnl(client.clone(), cfg, p).await {
            Ok(pnl) => lines.push(format_position_line(p, Some(pnl.pnl_percent), Some(pnl.pnl_usd), Some(pnl.in_range))),
            Err(e) => {
                log::warn!("PnL computation failed for position {}: {e:?}", p.token_id);
                lines.push(format_position_line(p, None, None, None));
            }
        }
        lines.push(String::new());
    }

    bot.send_message(chat_id, lines.join("\n")).parse_mode(teloxide::types::ParseMode::Markdown).await?;
    Ok(())
}

/// The single security boundary for this bot: only the chat configured as
/// `telegram.chat_id` may trigger anything. Everyone else's messages and
/// button taps are logged and silently dropped — this bot holds a real
/// signing key, so an unauthenticated sender must never be able to reach
/// `add_liquidity` or `close_position`.
fn is_owner(cfg: &AppConfig, chat_id: i64) -> bool {
    chat_id == cfg.telegram.chat_id
}

/// Runs the Telegram dispatcher: listens for the "Add LP now" / "Close
/// position" button taps and the `/positions` command.
pub async fn run_bot(
    bot: Bot,
    cfg: Arc<AppConfig>,
    client: Arc<ChainClient>,
    storage: Arc<Mutex<Storage>>,
    screening_enabled: Arc<AtomicBool>,
) -> Result<()> {
    let cfg_cb = cfg.clone();
    let client_cb = client.clone();
    let storage_cb = storage.clone();
    let callback_handler = Update::filter_callback_query().endpoint(move |bot: Bot, q: CallbackQuery| {
        let cfg = cfg_cb.clone();
        let client = client_cb.clone();
        let storage = storage_cb.clone();
        async move {
            let sender_chat_id = q.message.as_ref().map(|m| m.chat.id.0).unwrap_or(q.from.id.0 as i64);
            if !is_owner(&cfg, sender_chat_id) {
                log::warn!(
                    "Ignoring callback query from unauthorized chat {sender_chat_id} (from user {}) — configured owner is {}",
                    q.from.id, cfg.telegram.chat_id
                );
                return respond(());
            }

            if let Some(data) = q.data.clone() {
                if let Some(pool_hex) = data.strip_prefix("addlp:") {
                    handle_add_lp_tap(&bot, &q, cfg, client, storage, pool_hex).await;
                } else if let Some(token_id_str) = data.strip_prefix("closepos:") {
                    handle_close_tap(&bot, &q, cfg, client, storage, token_id_str).await;
                } else if let Some(addr) = data.strip_prefix("copyaddr:") {
                    let _ = bot.send_message(ChatId(sender_chat_id), addr).await;
                }
            }
            respond(())
        }
    });

    let cfg_msg = cfg.clone();
    let client_msg = client.clone();
    let storage_msg = storage.clone();
    let screening_msg = screening_enabled.clone();
    let message_handler = Update::filter_message().endpoint(move |bot: Bot, msg: Message| {
        let cfg = cfg_msg.clone();
        let client = client_msg.clone();
        let storage = storage_msg.clone();
        let screening = screening_msg.clone();
        async move {
            if !is_owner(&cfg, msg.chat.id.0) {
                log::warn!(
                    "Ignoring message from unauthorized chat {} — configured owner is {}",
                    msg.chat.id, cfg.telegram.chat_id
                );
                return respond(());
            }

            if let Some(text) = msg.text() {
                let trimmed = text.trim();
                if trimmed.starts_with("/positions") || trimmed.starts_with("/pnl") {
                    if let Err(e) = handle_positions_command(&bot, msg.chat.id, &cfg, client.clone(), storage.clone()).await {
                        log::error!("Failed to handle /positions: {e:?}");
                    }
                } else if trimmed.starts_with("/check") {
                    if let Err(e) = handle_check_command(&bot, msg.chat.id, &cfg, client.clone(), storage.clone(), trimmed).await {
                        log::error!("Failed to handle /check: {e:?}");
                    }
                } else if trimmed == "/toggle_screening" || trimmed == "/toggle" {
                    if let Err(e) = handle_toggle_command(&bot, msg.chat.id, &cfg, trimmed, screening.clone()).await {
                        log::error!("Failed to handle /toggle_screening: {e:?}");
                    }
                } else if trimmed == "/screening_status" || trimmed == "/status" {
                    if let Err(e) = handle_status_command(&bot, msg.chat.id, &cfg).await {
                        log::error!("Failed to handle /screening_status: {e:?}");
                    }
                } else if trimmed.starts_with("/dexscreener") {
                    if let Err(e) = handle_dexscreener_command(&bot, msg.chat.id, trimmed).await {
                        log::error!("Failed to handle /dexscreener: {e:?}");
                    }
                } else if trimmed == "/dextools_top10" {
                    if let Err(e) = handle_dextools_top10_command(&bot, msg.chat.id, &cfg).await {
                        log::error!("Failed to handle /dextools_top10: {e:?}");
                    }
                } else if trimmed == "/uniswap_top10" {
                    if let Err(e) = handle_uniswap_top10_command(&bot, msg.chat.id, &cfg).await {
                        log::error!("Failed to handle /uniswap_top10: {e:?}");
                    }
                }
            }
            respond(())
        }
    });

    let handler = dptree::entry().branch(callback_handler).branch(message_handler);

    Dispatcher::builder(bot, handler).enable_ctrlc_handler().build().dispatch().await;

    Ok(())
}

async fn handle_add_lp_tap(
    bot: &Bot,
    q: &CallbackQuery,
    cfg: Arc<AppConfig>,
    client: Arc<ChainClient>,
    storage: Arc<Mutex<Storage>>,
    pool_hex: &str,
) {
    let _ = bot.answer_callback_query(q.id.clone()).text("Submitting transaction…").await;

    let pool_addr = match Address::from_str(pool_hex) {
        Ok(a) => a,
        Err(_) => {
            let _ = bot
                .send_message(q.from.id, "Couldn't parse the pool address from that button — please check config/logs.")
                .await;
            return;
        }
    };

    let result = add_liquidity(client.clone(), &cfg, pool_addr, cfg.wallet.default_lp_usd_amount).await;
    let chat_id = q.message.as_ref().map(|m| m.chat.id);

    let text = match &result {
        Ok(r) => {
            let mut msg = format!("✅ LP transaction sent\n[View on explorer]({})", r.explorer_tx_url);
            if let Some(token_id) = r.token_id {
                // Persist the position so PnL / TP-SL / spike monitoring can
                // track it going forward.
                let erc0_sym = crate::chain::pricing::token_info(client.clone(), r.token0).await.map(|(s, _)| s).unwrap_or_default();
                let erc1_sym = crate::chain::pricing::token_info(client.clone(), r.token1).await.map(|(s, _)| s).unwrap_or_default();
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
                // pool_created_block: best-effort — we don't have it handy here,
                // 0 just means "no lower bound" for the spike-window clamp.
                let position = Position {
                    token_id,
                    pool_address: pool_addr,
                    pool_created_block: 0,
                    token0: r.token0,
                    token1: r.token1,
                    token0_symbol: erc0_sym,
                    token1_symbol: erc1_sym,
                    fee: r.fee,
                    tick_lower: r.tick_lower,
                    tick_upper: r.tick_upper,
                    entry_cost_usd: cfg.wallet.default_lp_usd_amount,
                    entry_timestamp: now,
                    mint_tx_hash: r.tx_hash.clone(),
                    closed: false,
                };
                let mut s = storage.lock().await;
                if let Err(e) = s.add_position(position) {
                    log::error!("Failed to persist new position: {e:?}");
                }
                msg.push_str(&format!("\nTracking as position `#{token_id}` — use /positions to check PnL any time."));
            } else {
                msg.push_str("\n⚠️ Couldn't parse a position ID from the transaction — PnL tracking won't be available for this position, but the LP was added successfully.");
            }
            msg
        }
        Err(e) => format!("❌ Failed to add LP for `{pool_hex}`:\n`{e}`"),
    };

    if let Some(chat_id) = chat_id {
        let _ = bot.send_message(chat_id, text).parse_mode(teloxide::types::ParseMode::Markdown).await;

        if result.is_ok() {
            if let Some(msg) = &q.message {
                let _ = bot
                    .edit_message_reply_markup(chat_id, msg.id)
                    .reply_markup(InlineKeyboardMarkup::new(Vec::<Vec<InlineKeyboardButton>>::new()))
                    .await;
            }
        }
    }
}

async fn handle_close_tap(
    bot: &Bot,
    q: &CallbackQuery,
    cfg: Arc<AppConfig>,
    client: Arc<ChainClient>,
    storage: Arc<Mutex<Storage>>,
    token_id_str: &str,
) {
    let _ = bot.answer_callback_query(q.id.clone()).text("Closing position…").await;
    let chat_id = q.message.as_ref().map(|m| m.chat.id);

    let token_id: u64 = match token_id_str.parse() {
        Ok(v) => v,
        Err(_) => {
            if let Some(chat_id) = chat_id {
                let _ = bot.send_message(chat_id, "Couldn't parse that position ID.").await;
            }
            return;
        }
    };

    let position = { storage.lock().await.get_position(token_id) };
    let pool_address = match position {
        Some(p) => p.pool_address,
        None => {
            if let Some(chat_id) = chat_id {
                let _ = bot
                    .send_message(chat_id, format!("Don't have position `#{token_id}` on record — can't determine its pool to close it safely."))
                    .await;
            }
            return;
        }
    };

    let result = close_position(client.clone(), &cfg, token_id, pool_address).await;
    let text = match &result {
        Ok(r) => {
            let mut s = storage.lock().await;
            if let Err(e) = s.mark_position_closed(token_id) {
                log::error!("Failed to mark position {token_id} closed: {e:?}");
            }
            let mut msg = format!(
                "✅ Position `#{token_id}` closed.\n[View on explorer]({})",
                r.explorer_tx_url
            );
            if r.swaps.is_empty() {
                msg.push_str("\nProceeds were already WETH — no swap needed.");
            } else {
                msg.push_str(&format!("\n🔄 Auto-swapped proceeds to WETH ({} swap(s)):", r.swaps.len()));
                for swap in &r.swaps {
                    msg.push_str(&format!("\n  `{:#x}` → ~{} out", swap.token_in, swap.amount_out));
                }
            }
            if !r.failed_legs.is_empty() {
                msg.push_str("\n\n⚠️ *Some proceeds couldn't be auto-swapped* (likely the token turned into a honeypot after screening — force-closed rather than blocking the whole close):");
                for leg in &r.failed_legs {
                    msg.push_str(&format!(
                        "\n  `{:#x}` — {} left in wallet as-is\n    reason: {}",
                        leg.stuck_token, leg.stuck_amount, leg.reason
                    ));
                }
                msg.push_str("\nCheck /positions and your wallet balance — you may want to try swapping this manually or just hold it.");
            }
            msg
        }
        Err(e) => format!(
            "❌ Failed to close position `#{token_id}`:\n`{e}`\n\nNote: if liquidity was already removed but the \
             auto-swap step failed, funds are still safely in your wallet as the original two tokens — check \
             /positions and the explorer before retrying."
        ),
    };

    if let Some(chat_id) = chat_id {
        let _ = bot.send_message(chat_id, text).parse_mode(teloxide::types::ParseMode::Markdown).await;
        if result.is_ok() {
            if let Some(msg) = &q.message {
                let _ = bot
                    .edit_message_reply_markup(chat_id, msg.id)
                    .reply_markup(InlineKeyboardMarkup::new(Vec::<Vec<InlineKeyboardButton>>::new()))
                    .await;
            }
        }
    }
}

/// Manual pool verification: user pastes a contract address and the bot
/// scores it the same way the auto-screener does.
async fn handle_check_command(
    bot: &Bot,
    chat_id: ChatId,
    cfg: &Arc<AppConfig>,
    client: Arc<ChainClient>,
    storage: Arc<Mutex<Storage>>,
    text: &str,
) -> Result<()> {
    let parts: Vec<&str> = text.split_whitespace().collect();
    if parts.len() < 2 {
        bot.send_message(chat_id, "Usage: `/check <pool_address>`\nExample: `/check 0x1234…abcd`").parse_mode(teloxide::types::ParseMode::Markdown).await?;
        return Ok(());
    }

    let pool_hex = parts[1].trim();
    let pool_addr = match Address::from_str(pool_hex) {
        Ok(a) => a,
        Err(_) => {
            bot.send_message(chat_id, format!("❌ Invalid address: `{pool_hex}`")).parse_mode(teloxide::types::ParseMode::Markdown).await?;
            return Ok(());
        }
    };

    let _ = bot.send_message(chat_id, format!("🔍 Looking up pool `{pool_hex}`…")).parse_mode(teloxide::types::ParseMode::Markdown).await;

    let pool_info = match crate::chain::pools::lookup_pool_by_address(client.clone(), pool_addr).await {
        Ok(p) => p,
        Err(e) => {
            bot.send_message(chat_id, format!("❌ Could not read pool contract `{pool_hex}`:\n`{e}`")).parse_mode(teloxide::types::ParseMode::Markdown).await?;
            return Ok(());
        }
    };

    let latest_block = match client.get_block_number().await {
        Ok(b) => b.as_u64(),
        Err(_) => 0,
    };
    let now_ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();

    let metrics = match crate::chain::dexscreener::compute_metrics_with_fallback(
        client.clone(), cfg, &pool_info, latest_block, now_ts,
    ).await {
        Ok(m) => m,
        Err(e) => {
            bot.send_message(chat_id, format!("❌ Failed to compute metrics for `{pool_hex}`:\n`{e}`")).parse_mode(teloxide::types::ParseMode::Markdown).await?;
            return Ok(());
        }
    };

    let candidate = PoolCandidate { info: pool_info.clone(), metrics };
    let result = crate::screener::screen(candidate, &cfg.screening);

    let mut lines = vec![
        format!("*Pool:* `{pool_hex}`"),
        format!("*Pair:* {}/{} (fee {:.2}%)", result.candidate.metrics.token0_symbol, result.candidate.metrics.token1_symbol, pool_info.fee as f64 / 10_000.0),
    ];
    if let Some(tvl) = result.candidate.metrics.tvl_usd {
        lines.push(format!("*TVL:* ${:.0}", tvl));
    }
    if let Some(v) = result.candidate.metrics.volume_24h_usd {
        lines.push(format!("*24h volume:* ${:.0}", v));
    }
    if let Some(apr) = result.candidate.metrics.apr_percent {
        lines.push(format!("*Est. fee APR:* {:.1}%", apr));
    }
    lines.push(format!("*Age:* {:.1}h", result.candidate.metrics.age_hours));
    lines.push(format!("*Token0 verified:* {}", if result.candidate.metrics.token0_verified.unwrap_or(false) { "✅" } else { "❌" }));
    lines.push(format!("*Token1 verified:* {}", if result.candidate.metrics.token1_verified.unwrap_or(false) { "✅" } else { "❌" }));
    lines.push(String::new());

    if result.passed {
        lines.push("🟢 *PASSED screening*".to_string());
    } else {
        lines.push("🔴 *FAILED screening*".to_string());
    }
    lines.push("*Reasons:*".to_string());
    for r in &result.reasons {
        lines.push(format!("• {r}"));
    }

    let reply_text = lines.join("\n");

    if result.passed {
        let keyboard = InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback(
            "✅ Add LP now",
            format!("addlp:{pool_hex}"),
        )]]);
        bot.send_message(chat_id, reply_text)
            .parse_mode(teloxide::types::ParseMode::Markdown)
            .reply_markup(keyboard)
            .await?;
    } else {
        bot.send_message(chat_id, reply_text)
            .parse_mode(teloxide::types::ParseMode::Markdown)
            .await?;
    }

    // Mark as alerted so the auto-screener doesn't duplicate-alert later.
    let mut s = storage.lock().await;
    let _ = s.mark_alerted(pool_addr);

    Ok(())
}

/// Fetch top 10 tokens from DexTools for Robinhood chain, filtered by:
///   1. Robinhood chain only
///   2. Volume >= $200k in last 1h (via DexScreener)
///   3. Audit issues <= 3 (via DexTools token audit)
///   4. Score >= 70 (via DexTools pool score)
///   5. Uniswap v3 only
async fn handle_dextools_top10_command(
    bot: &Bot,
    chat_id: ChatId,
    cfg: &Arc<AppConfig>,
) -> Result<()> {
    if cfg.dextools_api_key.trim().is_empty() {
        bot.send_message(
            chat_id,
            "❌ DexTools API key not configured.\n\
             Add `dextools_api_key = \"your-key\"` to config.toml and restart.\n\
             Get a key at https://developer.dextools.io",
        )
        .await?;
        return Ok(());
    }

    let _ = bot
        .send_message(chat_id, "🔍 Scanning DexTools top pools for Robinhood chain…")
        .await;

    let dt = crate::chain::dextools::DexToolsClient::new(&cfg.dextools_api_key);

    // 1. Fetch hot pools from DexTools.
    let hotpools = match dt.fetch_hotpools("robinhood").await {
        Ok(p) => p,
        Err(e) => {
            bot.send_message(
                chat_id,
                format!("❌ DexTools API error:\n`{e}`"),
            )
            .parse_mode(teloxide::types::ParseMode::Markdown)
            .await?;
            return Ok(());
        }
    };

    if hotpools.is_empty() {
        bot.send_message(chat_id, "No hot pools found on DexTools for Robinhood chain.").await?;
        return Ok(());
    }

    // 2. Filter for Uniswap v3.
    let mut v3_pools: Vec<_> = hotpools
        .into_iter()
        .filter(|p| {
            let ename = p.exchange_name.to_lowercase();
            ename.contains("uniswap") && p.fee > 0.0
        })
        .collect();

    if v3_pools.is_empty() {
        bot.send_message(chat_id, "No Uniswap v3 pools in the DexTools hot list for Robinhood chain.").await?;
        return Ok(());
    }

    // 3. Enrich with DexScreener data (volume, mcap, liquidity, age) in parallel.
    use futures::future::join_all;

    let ds_futures: Vec<_> = v3_pools
        .iter()
        .map(|p| crate::chain::dexscreener::fetch_pair("robinhood", &p.address))
        .collect();
    let ds_results = join_all(ds_futures).await;

    let mut enriched = Vec::new();
    for (pool, ds_result) in v3_pools.into_iter().zip(ds_results) {
        let ds_pair = match ds_result {
            Ok(Some(p)) => p,
            _ => continue, // skip if DexScreener doesn't know this pool
        };

        // Volume filter: >= $200k in last 1h.
        if ds_pair.volume.h1 < 200_000.0 {
            continue;
        }

        let mcap = ds_pair.market_cap.or(ds_pair.fdv).unwrap_or(0.0);
        let liq = ds_pair.liquidity.as_ref().map(|l| l.usd).unwrap_or(0.0);

        let age_sec = ds_pair.pair_created_at.map(|ms| {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            now_ms.saturating_sub(ms) / 1000
        });
        let vol1h = ds_pair.volume.h1;

        enriched.push((pool, ds_pair, mcap, liq, age_sec, vol1h));
    }

    if enriched.is_empty() {
        bot.send_message(
            chat_id,
            "No Uniswap v3 pools on Robinhood chain with >=$200k 1h volume found on DexScreener.",
        )
        .await?;
        return Ok(());
    }

    // 4. Fetch DexTools score + token audit for each candidate in parallel batches.
    let score_futs: Vec<_> = enriched
        .iter()
        .map(|(p, _, _, _, _, _)| dt.fetch_pool_score("robinhood", &p.address))
        .collect();
    let audit_futs: Vec<_> = enriched
        .iter()
        .map(|(p, _, _, _, _, _)| dt.fetch_token_audit("robinhood", &p.main_token.address))
        .collect();

    let scores = join_all(score_futs).await;
    let audits = join_all(audit_futs).await;

    let mut filtered = Vec::new();
    for (((pool, ds_pair, mcap, liq, age_sec, vol1h), score_res), audit_res) in
        enriched.into_iter().zip(scores).zip(audits)
    {
        let score = match score_res {
            Ok(s) => s.dext_score.total,
            Err(_) => 0.0,
        };
        if score < 70.0 {
            continue;
        }

        let audit_issues = match audit_res {
            Ok(a) => crate::chain::dextools::audit_issue_count(&a),
            Err(_) => 99, // treat missing audit as failing
        };
        if audit_issues > 3 {
            continue;
        }

        filtered.push((pool, ds_pair, mcap, liq, age_sec, vol1h, score, audit_issues));
    }

    // Sort by DexTools rank (ascending).
    filtered.sort_by(|a, b| a.0.rank.cmp(&b.0.rank));

    if filtered.is_empty() {
        bot.send_message(
            chat_id,
            "No pools passed all filters (Uniswap v3 + >=$200k 1h vol + score >=70 + audit issues <=3).",
        )
        .await?;
        return Ok(());
    }

    // 5. Send results — one message per token so the address is easy to copy.
    for (i, (pool, _ds_pair, mcap, liq, age_sec, vol1h, score, audit_issues)) in
        filtered.iter().take(10).enumerate()
    {
        let age_str = match age_sec {
            None => "?".to_string(),
            Some(s) if *s < 60 => "<1m".to_string(),
            Some(s) if *s < 3600 => format!("{:.0}m", *s as f64 / 60.0),
            Some(s) if *s < 86400 => format!("{:.0}h", *s as f64 / 3600.0),
            Some(s) => format!("{:.0}d", *s as f64 / 86400.0),
        };

        let mcap_str = fmt_compact_nk(*mcap);
        let vol_str = fmt_compact_nk(*vol1h);
        let liq_str = fmt_compact_nk(*liq);
        let shield = if *audit_issues == 0 {
            "🛡️".to_string()
        } else {
            format!("🛡️{}", audit_issues)
        };

        let text = format!(
            "📊 *#{} {} / {}*\nM{} V{} L{} S{:.0} {} A{}\n{}",
            i + 1,
            pool.main_token.symbol,
            pool.side_token.symbol,
            mcap_str,
            vol_str,
            liq_str,
            score,
            shield,
            age_str,
            pool.address.to_lowercase(),
        );

        let keyboard = InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback(
            "📋 Copy pool address",
            format!("copyaddr:{}", pool.address.to_lowercase()),
        )]]);

        bot.send_message(chat_id, text)
            .parse_mode(teloxide::types::ParseMode::Markdown)
            .reply_markup(keyboard)
            .await?;
    }

    Ok(())
}

/// Fetch top 10 Uniswap v3 pools on Robinhood chain from DexScreener,
/// filtered by:
///   1. Robinhood chain only
///   2. Volume >= $100k in last 24h
///   3. Market cap >= $20k
///   4. Computed score >= 50 (liquidity-weighted volume rank)
///   5. Uniswap v3 only
async fn handle_uniswap_top10_command(
    bot: &Bot,
    chat_id: ChatId,
    cfg: &Arc<AppConfig>,
) -> Result<()> {
    let _ = bot
        .send_message(chat_id, "🔍 Scanning Uniswap v3 top pools on Robinhood chain…")
        .await;

    // 1. Fetch pairs for both anchor tokens (WETH + USDG) to maximise coverage.
    let weth = &cfg.chain.weth_address;
    let usdg = &cfg.chain.usdc_address;

    let (weth_pairs, usdg_pairs) = tokio::join!(
        crate::chain::dexscreener::fetch_token_pairs("robinhood", weth),
        crate::chain::dexscreener::fetch_token_pairs("robinhood", usdg),
    );

    let mut all_pairs: Vec<_> = weth_pairs.unwrap_or_default();
    all_pairs.extend(usdg_pairs.unwrap_or_default());

    // Deduplicate by pair address, keep the one with higher 24h volume.
    let mut seen: std::collections::HashMap<String, crate::chain::dexscreener::DexScreenerPair> = std::collections::HashMap::new();
    for p in all_pairs {
        let addr = p.pair_address.to_lowercase();
        let existing = seen.get(&addr);
        let keep = match existing {
            Some(ep) => p.volume.h24 > ep.volume.h24,
            None => true,
        };
        if keep {
            seen.insert(addr, p);
        }
    }
    let mut unique_pairs: Vec<_> = seen.into_values().collect();

    // 2. Filter for Uniswap v3.
    let mut v3_pairs: Vec<_> = unique_pairs
        .into_iter()
        .filter(|p| {
            let is_uniswap = p.dex_id.to_lowercase().contains("uniswap");
            let is_v3 = p.labels.as_ref().map(|l| l.iter().any(|x| x.to_lowercase().contains("v3"))).unwrap_or(false);
            is_uniswap && is_v3
        })
        .collect();

    if v3_pairs.is_empty() {
        bot.send_message(chat_id, "No Uniswap v3 pools found on Robinhood chain via DexScreener.").await?;
        return Ok(());
    }

    // 3. Volume filter: >= $100k in last 24h.
    v3_pairs.retain(|p| p.volume.h24 >= 100_000.0);

    if v3_pairs.is_empty() {
        bot.send_message(chat_id, "No Uniswap v3 pools on Robinhood chain with >=$100k 24h volume.").await?;
        return Ok(());
    }

    let mut filtered = Vec::new();
    for pair in v3_pairs {
        let mcap = pair.market_cap.or(pair.fdv).unwrap_or(0.0);
        if mcap < 20_000.0 {
            continue;
        }
        let liq = pair.liquidity.as_ref().map(|l| l.usd).unwrap_or(0.0);
        let vol24h = pair.volume.h24;

        // Compute a simple score: 0-100 based on liquidity + volume ranking.
        let score = if liq > 0.0 {
            let vol_score = (vol24h / 1_000_000.0).min(100.0); // up to 100 for $1M+ 24h vol
            let liq_score = (liq / 500_000.0).min(100.0);     // up to 100 for $500k+ liq
            (vol_score * 0.6 + liq_score * 0.4).min(100.0)
        } else {
            0.0
        };
        if score < 50.0 {
            continue;
        }

        let age_sec = pair.pair_created_at.map(|ms| {
            let now_ms = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            now_ms.saturating_sub(ms) / 1000
        });

        filtered.push((pair, mcap, liq, age_sec, vol24h, score));
    }

    // Sort by 24h volume descending.
    filtered.sort_by(|a, b| b.4.partial_cmp(&a.4).unwrap_or(std::cmp::Ordering::Equal));

    if filtered.is_empty() {
        bot.send_message(
            chat_id,
            "No pools passed all filters (Uniswap v3 + >=$100k 24h vol + mcap >=$20k + score >=50).",
        )
        .await?;
        return Ok(());
    }

    // 4. Send results — one message per token so the address is easy to copy.
    for (i, (pair, mcap, liq, age_sec, vol24h, score)) in
        filtered.iter().take(10).enumerate()
    {
        let age_str = match age_sec {
            None => "?".to_string(),
            Some(s) if *s < 60 => "<1m".to_string(),
            Some(s) if *s < 3600 => format!("{:.0}m", *s as f64 / 60.0),
            Some(s) if *s < 86400 => format!("{:.0}h", *s as f64 / 3600.0),
            Some(s) => format!("{:.0}d", *s as f64 / 86400.0),
        };

        let mcap_str = fmt_compact_nk(*mcap);
        let vol_str = fmt_compact_nk(*vol24h);
        let liq_str = fmt_compact_nk(*liq);

        let text = format!(
            "📊 *#{} {} / {}*\nM{} V{} L{} S{:.0} A{}\n{}",
            i + 1,
            pair.base_token.symbol,
            pair.quote_token.symbol,
            mcap_str,
            vol_str,
            liq_str,
            score,
            age_str,
            pair.pair_address.to_lowercase(),
        );

        let keyboard = InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback(
            "📋 Copy pool address",
            format!("copyaddr:{}", pair.pair_address.to_lowercase()),
        )]]);

        bot.send_message(chat_id, text)
            .parse_mode(teloxide::types::ParseMode::Markdown)
            .reply_markup(keyboard)
            .await?;
    }

    Ok(())
}

/// Compact formatter: $1.2M, $500K, $850.
fn fmt_compact_nk(v: f64) -> String {
    if v >= 1_000_000_000.0 {
        format!("${:.1}B", v / 1_000_000_000.0)
    } else if v >= 1_000_000.0 {
        format!("${:.1}M", v / 1_000_000.0)
    } else if v >= 1_000.0 {
        format!("${:.0}K", v / 1_000.0)
    } else {
        format!("${:.0}", v)
    }
}

async fn handle_toggle_command(
    bot: &Bot,
    chat_id: ChatId,
    _cfg: &Arc<AppConfig>,
    _text: &str,
    screening_enabled: Arc<AtomicBool>,
) -> Result<()> {
    let current = screening_enabled.load(Ordering::Relaxed);
    let new_val = !current;
    screening_enabled.store(new_val, Ordering::Relaxed);

    let status = if new_val { "🟢 ON" } else { "🔴 OFF" };
    let msg = if new_val {
        format!("{status}\n\nAuto-screening is now *enabled*. The bot will discover and screen new pools every {}s.", _cfg.screening.poll_interval_secs)
    } else {
        format!("{status}\n\nAuto-screening is now *disabled*. Use `/check <pool_address>` to manually verify any pool before adding LP.")
    };

    bot.send_message(chat_id, msg).parse_mode(teloxide::types::ParseMode::Markdown).await?;
    Ok(())
}

async fn handle_status_command(
    bot: &Bot,
    chat_id: ChatId,
    cfg: &Arc<AppConfig>,
) -> Result<()> {
    let lines = vec![
        "*Bot status:*".to_string(),
        String::new(),
        format!("*Auto-screening:* {}", if cfg.screening.enabled { "🟢 ON" } else { "🔴 OFF" }),
        format!("*Poll interval:* {}s", cfg.screening.poll_interval_secs),
        format!("*Min TVL:* ${:.0}", cfg.screening.min_tvl_usd),
        format!("*Min volume:* ${:.0}", cfg.screening.min_volume_24h_usd),
        format!("*Min APR:* {:.0}%", cfg.screening.min_apr_percent),
        format!("*Max APR:* {:.0}%", cfg.screening.max_apr_percent),
        format!("*Min age:* {:.0}h", cfg.screening.min_pool_age_hours),
        format!("*Verified tokens required:* {}", if cfg.screening.require_verified_tokens { "yes" } else { "no" }),
        String::new(),
        "*Commands:*".to_string(),
        "`/check <address>` — score a pool manually".to_string(),
        "`/dexscreener <symbol>` — quick DexScreener lookup".to_string(),
        "`/dextools_top10` — top 10 DexTools pools (Robinhood, V3, score>=70)".to_string(),
        "`/uniswap_top10` — top 10 Uniswap v3 pools on Robinhood chain".to_string(),
        "`/toggle_screening` — flip auto-screening on/off".to_string(),
        "`/positions` — list open positions".to_string(),
    ];

    bot.send_message(chat_id, lines.join("\n"))
        .parse_mode(teloxide::types::ParseMode::Markdown)
        .await?;
    Ok(())
}

/// Format a millisecond timestamp into a human-readable age string.
fn format_age_ms(pair_created_at_ms: Option<u64>) -> String {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let age_sec = pair_created_at_ms.map(|ms| now_ms.saturating_sub(ms) / 1000);
    match age_sec {
        None => "unknown".to_string(),
        Some(s) if s < 60 => "<1m".to_string(),
        Some(s) if s < 3600 => format!("{:.0}m", s as f64 / 60.0),
        Some(s) if s < 86400 => format!("{:.1}h", s as f64 / 3600.0),
        Some(s) if s < 2_592_000 => format!("{:.0}d", s as f64 / 86400.0),
        Some(s) => format!("{:.1}mo", s as f64 / 2_592_000.0),
    }
}

/// Compact USD formatter: 5.04M, 45.1K, 1,057, <1.
fn fmt_compact(v: f64) -> String {
    if v >= 1_000_000_000.0 {
        format!("${:.2}B", v / 1_000_000_000.0)
    } else if v >= 1_000_000.0 {
        format!("${:.2}M", v / 1_000_000.0)
    } else if v >= 1_000.0 {
        format!("${:.1}K", v / 1_000.0)
    } else if v >= 1.0 {
        format!("${:.0}", v)
    } else {
        "<$1".to_string()
    }
}

/// Format a number with comma separators.
fn fmt_num(n: u64) -> String {
    n.to_string()
        .as_bytes()
        .rchunks(3)
        .rev()
        .map(std::str::from_utf8)
        .collect::<Result<Vec<_>, _>>()
        .unwrap_or_default()
        .join(",")
}

/// Average trade size = volume / total transactions. Returns compact string.
fn avg_trade(vol: f64, buys: u64, sells: u64) -> String {
    let txns = buys + sells;
    if txns == 0 {
        "—".to_string()
    } else {
        fmt_compact(vol / txns as f64)
    }
}

/// What percentage of the 24h volume does this shorter window represent?
fn pct_of_24h(window_vol: f64, h24_vol: f64) -> String {
    if h24_vol <= 0.0 {
        "—".to_string()
    } else {
        format!("{:.1}%", window_vol / h24_vol * 100.0)
    }
}

/// Format an Option<bool> into a display string with yes/no/unknown labels.
fn fmt_bool(v: Option<bool>, yes: &str, no: &str, unknown: &str) -> String {
    match v {
        Some(true) => yes.to_string(),
        Some(false) => no.to_string(),
        None => unknown.to_string(),
    }
}

/// Build the full DexScreener pair message with a monospace table.
fn dexscreener_pair_message(i: usize, p: &crate::chain::dexscreener::DexScreenerPair) -> String {
    let age = format_age_ms(p.pair_created_at);
    let h24_vol = p.volume.h24;

    let mut lines = vec![format!(
        "📊 *#{} {} / {}* · {} · Age: *{}* · Price: *{}*",
        i + 1,
        p.base_token.symbol,
        p.quote_token.symbol,
        p.chain_id,
        age,
        p.price_usd
    )];

    // Raw pool address on its own line for easy copy-paste (long-press to select)
    lines.push(p.pair_address.clone());

    // Monospace table inside a code block — renders perfectly on every Telegram client
    lines.push("```".to_string());
    lines.push(format!(
        "{:>3} {:>8} {:>8} {:>10} {:>10} {:>7}",
        "TF", "Buys", "Sells", "Volume", "AvgTrade", "vs24h"
    ));

    // helper to build one row
    let mut push_row = |tf: &str, buys: u64, sells: u64, vol: f64| {
        let vs = if tf == "24h" {
            "—".to_string()
        } else {
            pct_of_24h(vol, h24_vol)
        };
        lines.push(format!(
            "{:>3} {:>8} {:>8} {:>10} {:>10} {:>7}",
            tf,
            fmt_num(buys),
            fmt_num(sells),
            fmt_compact(vol),
            avg_trade(vol, buys, sells),
            vs
        ));
    };

    push_row("24h", p.txns.h24.buys, p.txns.h24.sells, p.volume.h24);
    push_row("6h", p.txns.h6.buys, p.txns.h6.sells, p.volume.h6);
    push_row("1h", p.txns.h1.buys, p.txns.h1.sells, p.volume.h1);
    push_row("5m", p.txns.m5.buys, p.txns.m5.sells, p.volume.m5);

    lines.push("```".to_string());

    // Summary line outside the code block so markdown bold/link work
    let mut summary_parts = vec![];
    if let Some(liq) = &p.liquidity {
        summary_parts.push(format!("💧 {}", fmt_compact(liq.usd)));
    }
    if let Some(fdv) = p.fdv {
        summary_parts.push(format!("🏦 {}", fmt_compact(fdv)));
    }
    if let Some(mc) = p.market_cap {
        summary_parts.push(format!("📈 {}", fmt_compact(mc)));
    }
    if let Some(pc) = p.price_change.h24 {
        let emoji = if pc >= 0.0 { "🟢" } else { "🔴" };
        summary_parts.push(format!("24h {} {:.2}%", emoji, pc));
    }
    if !summary_parts.is_empty() {
        lines.push(summary_parts.join(" · "));
    }

    lines.push(format!("[Open on DexScreener]({})", p.url));
    lines.join("\n")
}

/// Quick DexScreener lookup by token symbol or address.
/// Shows up to 5 pairs with multi-timeframe metrics.
async fn handle_dexscreener_command(
    bot: &Bot,
    chat_id: ChatId,
    text: &str,
) -> Result<()> {
    let parts: Vec<&str> = text.split_whitespace().collect();
    if parts.len() < 2 {
        bot.send_message(
            chat_id,
            "Usage: `/dexscreener <token_address_or_symbol>`\n\
             Example: `/dexscreener FRANK` or `/dexscreener 0xC36A…`",
        )
        .parse_mode(teloxide::types::ParseMode::Markdown)
        .await?;
        return Ok(());
    }

    let query = parts[1..].join(" ");
    let _ = bot
        .send_message(chat_id, format!("🔍 Searching DexScreener for `{}`…", query))
        .parse_mode(teloxide::types::ParseMode::Markdown)
        .await;

    let pairs = match crate::chain::dexscreener::fetch_search(&query).await {
        Ok(p) => p,
        Err(e) => {
            bot.send_message(
                chat_id,
                format!("❌ DexScreener search failed:\n`{e}`"),
            )
            .parse_mode(teloxide::types::ParseMode::Markdown)
            .await?;
            return Ok(());
        }
    };

    let pairs: Vec<_> = pairs.into_iter().filter(|p| p.volume.h24 >= 200_000.0).collect();

    if pairs.is_empty() {
        bot.send_message(
            chat_id,
            format!("No results with >=$200k 24h volume found for `{}` on DexScreener.", query),
        )
        .parse_mode(teloxide::types::ParseMode::Markdown)
        .await?;
        return Ok(());
    }

    for (i, p) in pairs.iter().take(5).enumerate() {
        let text = dexscreener_pair_message(i, p);
        let keyboard = InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback(
            "📋 Copy pool address",
            format!("copyaddr:{}", p.pair_address),
        )]]);
        bot.send_message(chat_id, text)
            .parse_mode(teloxide::types::ParseMode::Markdown)
            .reply_markup(keyboard)
            .await?;
    }

    Ok(())
}
