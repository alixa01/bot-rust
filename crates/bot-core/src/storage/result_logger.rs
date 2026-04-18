use std::path::Path;

use anyhow::Result;
use tokio::fs::{self, OpenOptions};
use tokio::io::AsyncWriteExt;

use crate::types::ResultRow;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogStatus {
    Written,
    SkippedDuplicate,
}

pub async fn log_result_row(path: &Path, row: &ResultRow) -> Result<LogStatus> {
    let content = match fs::read_to_string(path).await {
        Ok(value) => value,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error.into()),
    };

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        if let Ok(existing) = serde_json::from_str::<ResultRow>(trimmed) {
            if existing.market == row.market {
                return Ok(LogStatus::SkippedDuplicate);
            }
        }
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).await?;
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await?;
    let line = format!("{}\n", serde_json::to_string(row)?);
    file.write_all(line.as_bytes()).await?;
    file.flush().await?;

    Ok(LogStatus::Written)
}
