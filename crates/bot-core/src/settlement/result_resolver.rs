use anyhow::{bail, Result};

use crate::types::{SettlementOutcome, V3Config};

pub async fn resolve_window_outcome(
    _config: &V3Config,
    _window_start_sec: u64,
    _slug: &str,
) -> Result<SettlementOutcome> {
    bail!("window outcome resolver is not implemented yet")
}
