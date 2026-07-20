use crate::chain::lp::add_liquidity;
use crate::chain::ChainClient;
use crate::config::AppConfig;
use crate::models::ScreenResult;
use anyhow::Result;
use ethers::types::Address;
use std::str::FromStr;
use std::sync::Arc;
use teloxide::prelude::*;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};

fn format_alert(result: &ScreenResult) -> String {
    let m = &result.candidate.metrics;
    let info = &result.candidate.info;

    let mut lines = vec![
        format!("🟢 *Pool passed screening*"),
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

/// Runs the Telegram dispatcher that listens for the "Add LP now" button tap
/// and executes the on-chain add-liquidity transaction with the configured
/// wallet. Runs forever; spawn this as its own task.
pub async fn run_callback_listener(
    bot: Bot,
    cfg: Arc<AppConfig>,
    client: Arc<ChainClient>,
) -> Result<()> {
    let handler = Update::filter_callback_query().endpoint(
        move |bot: Bot, q: CallbackQuery| {
            let cfg = cfg.clone();
            let client = client.clone();
            async move {
                if let Some(data) = q.data.clone() {
                    if let Some(pool_hex) = data.strip_prefix("addlp:") {
                        handle_add_lp_tap(&bot, &q, cfg, client, pool_hex).await;
                    }
                }
                respond(())
            }
        },
    );

    Dispatcher::builder(bot, handler)
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;

    Ok(())
}

async fn handle_add_lp_tap(
    bot: &Bot,
    q: &CallbackQuery,
    cfg: Arc<AppConfig>,
    client: Arc<ChainClient>,
    pool_hex: &str,
) {
    // Acknowledge the tap immediately so Telegram doesn't show a loading spinner
    // forever while the transaction is in flight.
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

    let result = add_liquidity(client, &cfg, pool_addr, cfg.wallet.default_lp_usd_amount).await;

    let chat_id = q.message.as_ref().map(|m| m.chat.id);
    let text = match result {
        Ok(r) => format!(
            "✅ LP transaction sent for `{pool_hex}`\n[View on explorer]({})",
            r.explorer_tx_url
        ),
        Err(e) => format!("❌ Failed to add LP for `{pool_hex}`:\n`{e}`"),
    };

    if let Some(chat_id) = chat_id {
        let _ = bot
            .send_message(chat_id, text)
            .parse_mode(teloxide::types::ParseMode::Markdown)
            .await;

        // Remove the button on the original alert so a second tap can't
        // double-submit the same LP add.
        if let Some(msg) = &q.message {
            let _ = bot
                .edit_message_reply_markup(chat_id, msg.id)
                .reply_markup(InlineKeyboardMarkup::new(Vec::<Vec<InlineKeyboardButton>>::new()))
                .await;
        }
    }
}
