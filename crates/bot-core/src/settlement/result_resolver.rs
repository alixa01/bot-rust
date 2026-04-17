use anyhow::{bail, Result};

use crate::types::{SettlementOutcome, Config};

pub async fn resolve_window_outcome(
    _config: &Config,
    _window_start_sec: u64,
    _slug: &str,
) -> Result<SettlementOutcome> {
    bail!("window outcome resolver is not implemented yet")
}
