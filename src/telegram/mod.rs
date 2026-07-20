use crate::chain::lp::{add_liquidity, close_position};
use crate::chain::position::compute_pnl;
use crate::chain::ChainClient;
use crate::config::AppConfig;
use crate::models::{Position, ScreenResult, VolumeSpike};
use anyhow::Result;
use ethers::types::Address;
use std::str::FromStr;
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
    if let Some(v) = m.volume_24h_usd {
        lines.push(format!("*24h volume:* ${:.0}", v));
    }
    if let Some(apr) = m.apr_percent {
        lines.push(format!("*Est. fee APR:* {:.1}%", apr));
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

/// Runs the Telegram dispatcher: listens for the "Add LP now" / "Close
/// position" button taps and the `/positions` command.
pub async fn run_bot(
    bot: Bot,
    cfg: Arc<AppConfig>,
    client: Arc<ChainClient>,
    storage: Arc<Mutex<Storage>>,
) -> Result<()> {
    let cfg_cb = cfg.clone();
    let client_cb = client.clone();
    let storage_cb = storage.clone();
    let callback_handler = Update::filter_callback_query().endpoint(move |bot: Bot, q: CallbackQuery| {
        let cfg = cfg_cb.clone();
        let client = client_cb.clone();
        let storage = storage_cb.clone();
        async move {
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
    let message_handler = Update::filter_message().endpoint(move |bot: Bot, msg: Message| {
        let cfg = cfg_msg.clone();
        let client = client_msg.clone();
        let storage = storage_msg.clone();
        async move {
            if let Some(text) = msg.text() {
                if text.trim().starts_with("/positions") || text.trim().starts_with("/pnl") {
                    if let Err(e) = handle_positions_command(&bot, msg.chat.id, &cfg, client, storage).await {
                        log::error!("Failed to handle /positions: {e:?}");
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

    let result = close_position(client.clone(), &cfg, token_id).await;
    let text = match &result {
        Ok(r) => {
            let mut s = storage.lock().await;
            if let Err(e) = s.mark_position_closed(token_id) {
                log::error!("Failed to mark position {token_id} closed: {e:?}");
            }
            format!(
                "✅ Position `#{token_id}` closed.\n[View on explorer]({})",
                r.explorer_tx_url
            )
        }
        Err(e) => format!("❌ Failed to close position `#{token_id}`:\n`{e}`"),
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
