use std::collections::HashMap;

use anyhow::Result;
use serde_json::json;

use crate::types::{ExecutionResult, ExecutionStatus, V3Config};

pub async fn execute_live_entry(
    _config: &V3Config,
    _token_id: &str,
    _stake_usd: f64,
    _close_time_sec: u64,
) -> Result<ExecutionResult> {
    let mut raw = HashMap::new();
    raw.insert(
        "reason".to_owned(),
        json!("execute_live_entry is not implemented yet in Rust"),
    );

    Ok(ExecutionResult {
        status: ExecutionStatus::Skipped,
        order_id: String::new(),
        filled_price: 0.0,
        filled_size: 0.0,
        spent_usd: 0.0,
        used_fallback_limit: false,
        raw_response: raw,
    })
}
