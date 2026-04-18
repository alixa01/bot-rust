use std::path::Path;

use anyhow::{Context, Result};
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;

use crate::types::{ClaimStatus, MarketResolutionSource, TradeRecord};

#[derive(Debug, Clone, Default)]
pub struct ClaimStatusUpdate {
    pub claim_status: Option<ClaimStatus>,
    pub claim_attempts: Option<u64>,
    pub claim_tx_hash: Option<String>,
    pub claim_last_error: Option<String>,
    pub claim_updated_at_ms: Option<u64>,
    pub market_resolved: Option<bool>,
    pub market_resolution_source: Option<MarketResolutionSource>,
    pub market_resolved_at_ms: Option<u64>,
}

pub async fn log_trade_record(path: &Path, record: &TradeRecord) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
        .with_context(|| format!("failed to open trade log file: {}", path.display()))?;

    let line = format!("{}\n", serde_json::to_string(record)?);
    file.write_all(line.as_bytes()).await?;
    file.flush().await?;
    Ok(())
}

pub async fn read_trade_records(path: &Path) -> Result<Vec<TradeRecord>> {
    let content = match fs::read_to_string(path).await {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error).with_context(|| format!("failed reading {}", path.display()))
        }
    };

    let mut out = Vec::new();
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if let Ok(record) = serde_json::from_str::<TradeRecord>(trimmed) {
            out.push(record);
        }
    }

    Ok(out)
}

pub async fn update_trade_claim_status(
    path: &Path,
    trade_id: &str,
    updates: ClaimStatusUpdate,
) -> Result<bool> {
    let mut records = read_trade_records(path).await?;
    let mut changed = false;

    for record in &mut records {
        if record.id != trade_id {
            continue;
        }

        if let Some(value) = updates.claim_status {
            record.claim_status = Some(value);
        }
        if let Some(value) = updates.claim_attempts {
            record.claim_attempts = Some(value);
        }
        if let Some(value) = updates.claim_tx_hash.clone() {
            record.claim_tx_hash = Some(value);
        }
        if let Some(value) = updates.claim_last_error.clone() {
            record.claim_last_error = Some(value);
        }
        if let Some(value) = updates.claim_updated_at_ms {
            record.claim_updated_at_ms = Some(value);
        }
        if let Some(value) = updates.market_resolved {
            record.market_resolved = Some(value);
        }
        if let Some(value) = updates.market_resolution_source {
            record.market_resolution_source = Some(value);
        }
        if let Some(value) = updates.market_resolved_at_ms {
            record.market_resolved_at_ms = Some(value);
        }

        changed = true;
        break;
    }

    if !changed {
        return Ok(false);
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }

    let mut serialized = String::new();
    for record in records {
        serialized.push_str(&serde_json::to_string(&record)?);
        serialized.push('\n');
    }

    fs::write(path, serialized).await?;
    Ok(true)
}
