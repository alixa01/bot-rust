use anyhow::{bail, Result};

use crate::types::{SettlementTxMode, Config};

pub fn should_use_relayer_for_settlement(config: &Config) -> bool {
    matches!(config.settlement_tx_mode, SettlementTxMode::RelayerSafe)
}

pub async fn relay_redeem_positions(
    _config: &Config,
    _condition_id: &str,
) -> Result<String> {
    bail!("relayer redeem path is not implemented yet")
}
