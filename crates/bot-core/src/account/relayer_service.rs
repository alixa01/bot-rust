use anyhow::{bail, Result};

use crate::types::{SettlementTxMode, V3Config};

pub fn should_use_relayer_for_settlement(config: &V3Config) -> bool {
    matches!(config.settlement_tx_mode, SettlementTxMode::RelayerSafe)
}

pub async fn relay_redeem_positions(
    _config: &V3Config,
    _condition_id: &str,
) -> Result<String> {
    bail!("relayer redeem path is not implemented yet")
}
