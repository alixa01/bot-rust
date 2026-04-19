use std::sync::Mutex;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use ethers::providers::{Http, Middleware, Provider};
use ethers::types::U256;
use once_cell::sync::Lazy;

use crate::types::Config;

#[derive(Clone)]
struct CachedProvider {
    rpc_key: String,
    provider: Provider<Http>,
}

static CACHED_PROVIDER: Lazy<Mutex<Option<CachedProvider>>> = Lazy::new(|| Mutex::new(None));

pub fn parse_polygon_rpc_endpoints(config: &Config) -> Vec<String> {
    config
        .polygon_rpc_url
        .split(',')
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .collect()
}

pub fn get_primary_polygon_rpc(config: &Config) -> Result<String> {
    let endpoints = parse_polygon_rpc_endpoints(config);
    if let Some(first) = endpoints.first() {
        Ok(first.clone())
    } else {
        bail!("POLYGON_RPC_URL does not contain any valid endpoint")
    }
}

async fn is_provider_healthy(provider: &Provider<Http>) -> Result<()> {
    let chain_id = provider
        .get_chainid()
        .await
        .context("failed to fetch chainId from RPC")?;

    if chain_id != U256::from(137_u64) {
        bail!("unexpected chainId from RPC: {chain_id}");
    }

    Ok(())
}

pub async fn get_working_polygon_provider(config: &Config) -> Result<Provider<Http>> {
    let endpoints = parse_polygon_rpc_endpoints(config);
    if endpoints.is_empty() {
        bail!("POLYGON_RPC_URL does not contain a usable endpoint");
    }

    let rpc_key = endpoints.join("|");
    let cached = CACHED_PROVIDER.lock().ok().and_then(|guard| guard.clone());
    if let Some(entry) = cached {
        if entry.rpc_key == rpc_key {
            if is_provider_healthy(&entry.provider).await.is_ok() {
                return Ok(entry.provider);
            }

            if let Ok(mut guard) = CACHED_PROVIDER.lock() {
                *guard = None;
            }
        }
    }

    let mut errors: Vec<String> = Vec::new();
    for endpoint in endpoints {
        let provider = match Provider::<Http>::try_from(endpoint.clone()) {
            Ok(provider) => provider.interval(Duration::from_millis(250)),
            Err(error) => {
                errors.push(format!("{}: invalid endpoint ({})", endpoint, error));
                continue;
            }
        };

        match is_provider_healthy(&provider).await {
            Ok(()) => {
                if let Ok(mut guard) = CACHED_PROVIDER.lock() {
                    *guard = Some(CachedProvider {
                        rpc_key: rpc_key.clone(),
                        provider: provider.clone(),
                    });
                }

                return Ok(provider);
            }
            Err(error) => {
                errors.push(format!("{}: {}", endpoint, error));
            }
        }
    }

    bail!(
        "all polygon rpc endpoints failed: {}",
        if errors.is_empty() {
            "unknown error".to_owned()
        } else {
            errors.join(" | ")
        }
    )
}
