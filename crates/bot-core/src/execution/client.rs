use anyhow::{bail, Result};

use crate::types::V3Config;

#[derive(Debug, Clone)]
pub struct ClobClient;

pub fn create_clob_client(_config: &V3Config) -> Result<ClobClient> {
    bail!("CLOB client integration is not implemented yet")
}

pub fn get_clob_client(config: &V3Config) -> Result<ClobClient> {
    create_clob_client(config)
}
