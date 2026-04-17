use anyhow::{bail, Result};

use crate::types::V3Config;

#[derive(Debug, Clone)]
pub struct ChainlinkWindowPrice {
    pub price: f64,
    pub updated_at_sec: u64,
    pub round_id: String,
}

pub async fn resolve_window_open_price_from_chainlink(
    _config: &V3Config,
    _window_start_sec: u64,
) -> Result<ChainlinkWindowPrice> {
    bail!("chainlink anchor resolver is not implemented yet")
}
