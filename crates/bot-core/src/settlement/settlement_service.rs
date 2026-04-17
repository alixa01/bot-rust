use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use once_cell::sync::Lazy;

use crate::data::market_discovery::probe_gamma_resolution_by_slug;
use crate::types::{
    ClaimProcessingResult, ExecutionResult, MarketResolutionSource, MarketSide, TradeResult,
    Config,
};

const RESOLUTION_CACHE_TTL_MS: u64 = 10 * 60 * 1000;

#[derive(Debug, Clone, Copy)]
struct ResolutionCacheEntry {
    timestamp_ms: u64,
    resolved: bool,
}

static RESOLUTION_CACHE: Lazy<Mutex<HashMap<String, ResolutionCacheEntry>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

#[derive(Debug, Clone)]
pub struct TradePnlResult {
    pub outcome: TradeResult,
    pub redeemed_usd: f64,
    pub pnl_usd: f64,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn is_bytes32(value: &str) -> bool {
    let trimmed = value.trim();
    trimmed.len() == 66
        && trimmed.starts_with("0x")
        && trimmed.chars().skip(2).all(|c| c.is_ascii_hexdigit())
}

pub fn cleanup_resolution_cache() {
    if let Ok(mut cache) = RESOLUTION_CACHE.lock() {
        let now = now_ms();
        cache.retain(|_, entry| now.saturating_sub(entry.timestamp_ms) <= RESOLUTION_CACHE_TTL_MS);
    }
}

pub fn compute_trade_pnl(
    side: MarketSide,
    execution: &ExecutionResult,
    resolved_outcome: MarketSide,
) -> TradePnlResult {
    let is_win = side == resolved_outcome;

    if is_win {
        let redeemed_usd = execution.filled_size.max(0.0);
        let pnl_usd = redeemed_usd - execution.spent_usd;
        TradePnlResult {
            outcome: TradeResult::Win,
            redeemed_usd,
            pnl_usd,
        }
    } else {
        TradePnlResult {
            outcome: TradeResult::Loss,
            redeemed_usd: 0.0,
            pnl_usd: -execution.spent_usd,
        }
    }
}

pub async fn process_pending_claim(
    config: &Config,
    _trade_id: &str,
    window_slug: &str,
    condition_id: &str,
) -> Result<ClaimProcessingResult> {
    if !config.on_chain_auto_claim_enabled {
        return Ok(ClaimProcessingResult {
            completed: false,
            tx_hash: None,
            market_resolved: false,
            resolution_source: Some(MarketResolutionSource::Polling),
            market_resolved_at_ms: None,
            error: Some("auto-claim disabled".to_owned()),
        });
    }

    if !is_bytes32(condition_id) {
        return Ok(ClaimProcessingResult {
            completed: false,
            tx_hash: None,
            market_resolved: false,
            resolution_source: Some(MarketResolutionSource::Polling),
            market_resolved_at_ms: None,
            error: Some(format!("Invalid conditionId: {condition_id}")),
        });
    }

    let slug = window_slug.trim();
    if slug.is_empty() {
        return Ok(ClaimProcessingResult {
            completed: false,
            tx_hash: None,
            market_resolved: false,
            resolution_source: Some(MarketResolutionSource::Polling),
            market_resolved_at_ms: None,
            error: Some("window slug is required for claim resolution".to_owned()),
        });
    }

    cleanup_resolution_cache();

    let condition_key = condition_id.trim().to_lowercase();
    if let Ok(cache) = RESOLUTION_CACHE.lock() {
        if let Some(entry) = cache.get(&condition_key) {
            return Ok(ClaimProcessingResult {
                completed: false,
                tx_hash: None,
                market_resolved: entry.resolved,
                resolution_source: Some(MarketResolutionSource::Cached),
                market_resolved_at_ms: Some(entry.timestamp_ms),
                error: Some("redeem path is not implemented yet".to_owned()),
            });
        }
    }

    let probe = probe_gamma_resolution_by_slug(config, slug, condition_id).await?;
    if !probe.resolved {
        return Ok(ClaimProcessingResult {
            completed: false,
            tx_hash: None,
            market_resolved: false,
            resolution_source: Some(MarketResolutionSource::GammaFallback),
            market_resolved_at_ms: None,
            error: probe.error.or_else(|| Some("market unresolved".to_owned())),
        });
    }

    let resolved_at_ms = now_ms();
    if let Ok(mut cache) = RESOLUTION_CACHE.lock() {
        cache.insert(
            condition_key,
            ResolutionCacheEntry {
                timestamp_ms: resolved_at_ms,
                resolved: true,
            },
        );
    }

    Ok(ClaimProcessingResult {
        completed: false,
        tx_hash: None,
        market_resolved: true,
        resolution_source: Some(MarketResolutionSource::GammaFallback),
        market_resolved_at_ms: Some(resolved_at_ms),
        error: Some("redeem path is not implemented yet".to_owned()),
    })
}
