use crate::config::ScreeningConfig;
use crate::models::{PoolCandidate, ScreenResult};

pub fn screen(candidate: PoolCandidate, cfg: &ScreeningConfig) -> ScreenResult {
    let m = &candidate.metrics;
    let mut reasons = Vec::new();
    let mut passed = true;

    match m.tvl_usd {
        Some(tvl) if tvl >= cfg.min_tvl_usd => {
            reasons.push(format!("TVL ${:.0} >= min ${:.0}", tvl, cfg.min_tvl_usd));
        }
        Some(tvl) => {
            passed = false;
            reasons.push(format!("TVL ${:.0} below min ${:.0}", tvl, cfg.min_tvl_usd));
        }
        None => {
            passed = false;
            reasons.push("TVL unpriceable (neither side is WETH/USDC)".to_string());
        }
    }

    match m.volume_24h_usd {
        Some(v) if v >= cfg.min_volume_24h_usd => {
            reasons.push(format!("24h volume ${:.0} >= min ${:.0}", v, cfg.min_volume_24h_usd));
        }
        Some(v) => {
            passed = false;
            reasons.push(format!("24h volume ${:.0} below min ${:.0}", v, cfg.min_volume_24h_usd));
        }
        None => {
            passed = false;
            reasons.push("Volume unpriceable".to_string());
        }
    }

    match m.apr_percent {
        Some(apr) if apr >= cfg.min_apr_percent && apr <= cfg.max_apr_percent => {
            reasons.push(format!("Estimated APR {:.1}% within [{:.1}%, {:.1}%]", apr, cfg.min_apr_percent, cfg.max_apr_percent));
        }
        Some(apr) if apr > cfg.max_apr_percent => {
            passed = false;
            reasons.push(format!(
                "Estimated APR {:.1}% looks unsustainably high (> {:.1}%) — likely low liquidity / manipulated, treated as a red flag not a bonus",
                apr, cfg.max_apr_percent
            ));
        }
        Some(apr) => {
            passed = false;
            reasons.push(format!("Estimated APR {:.1}% below min {:.1}%", apr, cfg.min_apr_percent));
        }
        None => {
            passed = false;
            reasons.push("APR unpriceable".to_string());
        }
    }

    if m.age_hours >= cfg.min_pool_age_hours {
        reasons.push(format!("Pool age {:.1}h >= min {:.1}h", m.age_hours, cfg.min_pool_age_hours));
    } else {
        passed = false;
        reasons.push(format!("Pool age {:.1}h below min {:.1}h", m.age_hours, cfg.min_pool_age_hours));
    }

    if cfg.require_verified_tokens {
        let v0 = m.token0_verified.unwrap_or(false);
        let v1 = m.token1_verified.unwrap_or(false);
        if v0 && v1 {
            reasons.push("Both token contracts verified on Blockscout".to_string());
        } else {
            passed = false;
            reasons.push(format!(
                "Unverified token contract(s): {} {}",
                if v0 { "" } else { &m.token0_symbol },
                if v1 { "" } else { &m.token1_symbol }
            ));
        }
    }

    match m.honeypot_sellable {
        Some(true) => match m.honeypot_round_trip_loss_percent {
            Some(loss) if loss <= cfg.max_honeypot_loss_percent => {
                reasons.push(format!(
                    "Honeypot check passed: simulated buy-then-sell round trip lost {:.1}% (<= {:.1}% max)",
                    loss, cfg.max_honeypot_loss_percent
                ));
            }
            Some(loss) => {
                passed = false;
                reasons.push(format!(
                    "Simulated round-trip loss {:.1}% exceeds max {:.1}% — likely a hidden sell tax",
                    loss, cfg.max_honeypot_loss_percent
                ));
            }
            None => {
                passed = false;
                reasons.push("Honeypot check returned sellable but no loss figure — treating as inconclusive".to_string());
            }
        },
        Some(false) => {
            passed = false;
            reasons.push("Honeypot check FAILED: simulated sell reverted — token likely can't be sold".to_string());
        }
        None => {
            passed = false;
            reasons.push("Honeypot check inconclusive (pool unpriceable or quoter call failed)".to_string());
        }
    }

    // GoPlus's own honeypot verdict is an independent cross-check against
    // this bot's own on-chain simulation above — a hard fail regardless of
    // require_goplus_data, since "a reputable third party flagged this as a
    // honeypot" shouldn't be overridable by a config toggle.
    if !cfg.hide_goplus && m.is_honeypot_goplus == Some(true) {
        passed = false;
        reasons.push("GoPlus flags this token as a honeypot".to_string());
    }

    if !cfg.hide_geckoterminal {
        // --- GeckoTerminal weighted score check ---
        if cfg.min_gt_score > 0.0 {
            match m.gt_score {
                Some(score) if score >= cfg.min_gt_score => {
                    reasons.push(format!(
                        "GeckoTerminal gt_score {:.1} >= min {:.1} (components: pool={:.0} tx={:.0} creation={:.0} info={:.0} holders={:.0})",
                        score,
                        cfg.min_gt_score,
                        m.gt_score_pool.unwrap_or(0.0),
                        m.gt_score_transaction.unwrap_or(0.0),
                        m.gt_score_creation.unwrap_or(0.0),
                        m.gt_score_info.unwrap_or(0.0),
                        m.gt_score_holders.unwrap_or(0.0),
                    ));
                }
                Some(score) => {
                    passed = false;
                    reasons.push(format!(
                        "GeckoTerminal gt_score {:.1} below min {:.1} — weighted security score too low",
                        score, cfg.min_gt_score
                    ));
                }
                None => {
                    // No GeckoTerminal data: skip if gt_score check is configured but
                    // data is missing. Don't hard-fail here since GoPlus fallback
                    // handles the same underlying risks.
                    reasons.push("GeckoTerminal gt_score unavailable — skipping (GoPlus fallback active)".to_string());
                }
            }
        }

        if cfg.require_gt_verified {
            match m.gt_verified {
                Some(true) => reasons.push("GeckoTerminal verified ✓".to_string()),
                Some(false) => {
                    passed = false;
                    reasons.push("GeckoTerminal NOT verified — project hasn't submitted verified info".to_string());
                }
                None => {
                    reasons.push("GeckoTerminal verification status unknown — skipped".to_string());
                }
            }
        }

        // Also use GeckoTerminal's own honeypot flag as an independent cross-check
        // (same rationale as GoPlus honeypot above).
        if m.gecko_is_honeypot == Some(true) {
            passed = false;
            reasons.push("GeckoTerminal flags this token as a honeypot".to_string());
        }
    }

    if !cfg.hide_goplus {
        missing_or_check(
            m.holder_count.map(|v| v as f64),
            cfg.min_holder_count as f64,
            true,
            cfg,
            &mut passed,
            &mut reasons,
            |v, min| format!("Holder count {v:.0} >= min {min:.0}"),
            |v, min| format!("Holder count {v:.0} below min {min:.0}"),
            "Holder count",
        );

        missing_or_check(
            m.unique_traders_24h.map(|v| v as f64),
            cfg.min_unique_traders_24h as f64,
            true,
            cfg,
            &mut passed,
            &mut reasons,
            |v, min| format!("Unique traders (24h) {v:.0} >= min {min:.0}"),
            |v, min| format!("Unique traders (24h) {v:.0} below min {min:.0}"),
            "Unique traders (24h)",
        );

        missing_or_check(
            m.top10_holder_pct,
            cfg.max_top10_holder_pct,
            false,
            cfg,
            &mut passed,
            &mut reasons,
            |v, max| format!("Top-10 holder concentration {v:.1}% within max {max:.1}%"),
            |v, max| format!("Top-10 holder concentration {v:.1}% exceeds max {max:.1}%"),
            "Top-10 holder concentration",
        );

        missing_or_check(
            m.dev_holding_pct,
            cfg.max_dev_holding_pct,
            false,
            cfg,
            &mut passed,
            &mut reasons,
            |v, max| format!("Dev/creator holdings {v:.1}% within max {max:.1}%"),
            |v, max| format!("Dev/creator holdings {v:.1}% exceeds max {max:.1}%"),
            "Dev/creator holdings",
        );

        missing_or_check(
            m.buy_tax_percent,
            cfg.max_buy_tax_percent,
            false,
            cfg,
            &mut passed,
            &mut reasons,
            |v, max| format!("Buy tax {v:.1}% within max {max:.1}%"),
            |v, max| format!("Buy tax {v:.1}% exceeds max {max:.1}%"),
            "Buy tax",
        );

        missing_or_check(
            m.sell_tax_percent,
            cfg.max_sell_tax_percent,
            false,
            cfg,
            &mut passed,
            &mut reasons,
            |v, max| format!("Sell tax {v:.1}% within max {max:.1}%"),
            |v, max| format!("Sell tax {v:.1}% exceeds max {max:.1}%"),
            "Sell tax",
        );

        if cfg.require_not_mintable {
            match m.is_mintable {
                Some(false) => reasons.push("No privileged mint function (GoPlus)".to_string()),
                Some(true) => {
                    passed = false;
                    reasons.push("Contract has a privileged mint function — supply could be inflated at will".to_string());
                }
                None => fail_or_skip_bool(cfg, &mut passed, &mut reasons, "Mintability"),
            }
        }

        if cfg.require_ownership_renounced {
            match m.ownership_renounced {
                Some(true) => reasons.push("Ownership renounced / no privileged owner (GoPlus)".to_string()),
                Some(false) => {
                    passed = false;
                    reasons.push(
                        "Ownership NOT renounced — a privileged owner still exists (closest EVM equivalent to \
                         an unrevoked mint/freeze authority)"
                            .to_string(),
                    );
                }
                None => fail_or_skip_bool(cfg, &mut passed, &mut reasons, "Ownership-renounced status"),
            }
        }

        if cfg.require_not_blacklistable {
            let blacklistable = m.is_blacklistable.or(m.transfer_pausable);
            match blacklistable {
                Some(false) => reasons.push("No blacklist/pause function found (GoPlus)".to_string()),
                Some(true) => {
                    passed = false;
                    reasons.push(
                        "Contract can blacklist addresses or pause transfers — closest EVM equivalent to a \
                         freeze authority that hasn't been revoked"
                            .to_string(),
                    );
                }
                None => fail_or_skip_bool(cfg, &mut passed, &mut reasons, "Blacklist/pause capability"),
            }
        }

        if cfg.min_lp_locked_pct > 0.0 {
            missing_or_check(
                m.lp_locked_pct,
                cfg.min_lp_locked_pct,
                true,
                cfg,
                &mut passed,
                &mut reasons,
                |v, min| format!("LP locked {v:.1}% >= min {min:.1}%"),
                |v, min| format!("LP locked {v:.1}% below min {min:.1}% — GoPlus lock data is often unavailable for Uniswap v3 NFT positions, so this may reflect missing data rather than genuinely unlocked liquidity"),
                "LP locked",
            );
        }
    }

    ScreenResult {
        candidate,
        passed,
        reasons,
    }
}

/// Shared plumbing for a GoPlus-derived numeric threshold check.
/// `min_is_floor=true` means `value >= threshold` passes (a minimum);
/// `false` means `value <= threshold` passes (a maximum).
#[allow(clippy::too_many_arguments)]
fn missing_or_check(
    value: Option<f64>,
    threshold: f64,
    min_is_floor: bool,
    cfg: &ScreeningConfig,
    passed: &mut bool,
    reasons: &mut Vec<String>,
    ok_msg: impl Fn(f64, f64) -> String,
    fail_msg: impl Fn(f64, f64) -> String,
    label: &str,
) {
    match value {
        Some(v) if (min_is_floor && v >= threshold) || (!min_is_floor && v <= threshold) => {
            reasons.push(ok_msg(v, threshold));
        }
        Some(v) => {
            *passed = false;
            reasons.push(fail_msg(v, threshold));
        }
        None => {
            if cfg.require_goplus_data {
                *passed = false;
                reasons.push(format!("{label} unknown (GoPlus has no data on this token yet) — failing since require_goplus_data=true"));
            } else {
                reasons.push(format!("{label} unknown (GoPlus has no data yet) — skipped, not counted against this pool"));
            }
        }
    }
}

fn fail_or_skip_bool(cfg: &ScreeningConfig, passed: &mut bool, reasons: &mut Vec<String>, label: &str) {
    if cfg.require_goplus_data {
        *passed = false;
        reasons.push(format!("{label} unknown (GoPlus has no data on this token yet) — failing since require_goplus_data=true"));
    } else {
        reasons.push(format!("{label} unknown (GoPlus has no data yet) — skipped, not counted against this pool"));
    }
}
