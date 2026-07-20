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

    ScreenResult {
        candidate,
        passed,
        reasons,
    }
}
