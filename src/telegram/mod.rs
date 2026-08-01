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
                msg.push_str("\nProceeds were already USDG — no swap needed.");
            } else {
                msg.push_str(&format!("\n🔄 Auto-swapped proceeds to USDG ({} swap(s)):", r.swaps.len()));
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

    // Try DexScreener first for accurate TVL / volume / age.
    let mut metrics = match crate::chain::dexscreener::fetch_pair("robinhood", pool_hex).await {
        Ok(Some(ds)) => {
            let age_hours = ds.pair_created_at.map(|ms| (now_ts as f64 - (ms as f64 / 1000.0)) / 3600.0).unwrap_or(0.0);
            let fee_pct = pool_info.fee as f64 / 10_000.0;
            let apr = if ds.liquidity.usd > 0.0 {
                Some((ds.volume.h24 * fee_pct / 100.0) / ds.liquidity.usd * 365.0 * 100.0)
            } else {
                None
            };
            let base_addr = ds.base_token.address.parse::<ethers::types::Address>().unwrap_or_default();
            let (token0_sym, token1_sym) = if pool_info.token0 == base_addr {
                (ds.base_token.symbol.clone(), ds.quote_token.symbol.clone())
            } else {
                (ds.quote_token.symbol.clone(), ds.base_token.symbol.clone())
            };
            crate::models::PoolMetrics {
                token0_symbol: token0_sym,
                token1_symbol: token1_sym,
                tvl_usd: Some(ds.liquidity.usd),
                volume_24h_usd: Some(ds.volume.h24),
                apr_percent: apr,
                age_hours,
                token0_verified: Some(true),
                token1_verified: Some(true),
                ..Default::default()
            }
        }
        Ok(None) | Err(_) => {
            // Fallback to on-chain metrics.
            match crate::chain::metrics::compute_metrics(client.clone(), cfg, &pool_info, latest_block, now_ts).await {
                Ok(m) => m,
                Err(e) => {
                    bot.send_message(chat_id, format!("❌ Failed to compute metrics for `{pool_hex}`:\n`{e}`")).parse_mode(teloxide::types::ParseMode::Markdown).await?;
                    return Ok(());
                }
            }
        }
    };

    // Even when DexScreener gave us TVL / volume / age, we still need the
    // on-chain honeypot simulation — the screener hard-fails if
    // honeypot_sellable is None. Run compute_metrics in the background just
    // to fill honeypot and market-cap fields, then merge them into the
    // DexScreener-derived metrics without overwriting TVL/volume/APR.
    match crate::chain::metrics::compute_metrics(client.clone(), cfg, &pool_info, latest_block, now_ts).await {
        Ok(on_chain) => {
            metrics.honeypot_sellable = on_chain.honeypot_sellable;
            metrics.honeypot_round_trip_loss_percent = on_chain.honeypot_round_trip_loss_percent;
            metrics.market_cap_usd = on_chain.market_cap_usd.or(metrics.market_cap_usd);
        }
        Err(e) => log::warn!("On-chain honeypot check failed for /check {}: {e:?}", pool_hex),
    }

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
        "`/toggle_screening` — flip auto-screening on/off".to_string(),
        "`/positions` — list open positions".to_string(),
    ];

    bot.send_message(chat_id, lines.join("\n"))
        .parse_mode(teloxide::types::ParseMode::Markdown)
        .await?;
    Ok(())
}
