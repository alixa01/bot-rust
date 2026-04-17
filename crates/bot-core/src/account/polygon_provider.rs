use anyhow::{bail, Result};

use crate::types::V3Config;

pub fn parse_polygon_rpc_endpoints(config: &V3Config) -> Vec<String> {
    config
        .polygon_rpc_url
        .split(',')
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .collect()
}

pub fn get_primary_polygon_rpc(config: &V3Config) -> Result<String> {
    let endpoints = parse_polygon_rpc_endpoints(config);
    if let Some(first) = endpoints.first() {
        Ok(first.clone())
    } else {
        bail!("POLYGON_RPC_URL does not contain any valid endpoint")
    }
}
