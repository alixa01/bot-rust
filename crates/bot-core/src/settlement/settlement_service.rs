use std::sync::Mutex;

use anyhow::Result;
use once_cell::sync::Lazy;

use crate::types::{
    ClaimProcessingResult, ExecutionResult, MarketResolutionSource, MarketSide, TradeResult,
    V3Config,
};

static RESOLUTION_CACHE: Lazy<Mutex<Vec<String>>> = Lazy::new(|| Mutex::new(Vec::new()));

#[derive(Debug, Clone)]
pub struct TradePnlResult {
    pub outcome: TradeResult,
    pub redeemed_usd: f64,
    pub pnl_usd: f64,
}

pub fn cleanup_resolution_cache() {
    if let Ok(mut cache) = RESOLUTION_CACHE.lock() {
        cache.clear();
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
    _config: &V3Config,
    _trade_id: &str,
    _window_slug: &str,
    _condition_id: &str,
) -> Result<ClaimProcessingResult> {
    Ok(ClaimProcessingResult {
        completed: false,
        tx_hash: None,
        market_resolved: false,
        resolution_source: Some(MarketResolutionSource::Polling),
        market_resolved_at_ms: None,
        error: Some("claim processor is not implemented yet".to_owned()),
    })
}
