use anyhow::{bail, Result};

use crate::data::binance::fetch_window_open_close;
use crate::data::market_discovery::fallback_resolve_from_gamma;
use crate::types::{SettlementOutcome, Config};
use crate::types::{MarketSide, SettlementSource};
use crate::utils::logger::log_warn;

fn resolve_direction_from_open_close(open_price: f64, close_price: f64) -> MarketSide {
    if close_price >= open_price {
        MarketSide::Up
    } else {
        MarketSide::Down
    }
}

pub async fn resolve_window_outcome(
    config: &Config,
    window_start_sec: u64,
    slug: &str,
) -> Result<SettlementOutcome> {
    match fetch_window_open_close(config, window_start_sec).await {
        Ok((open_price, close_price)) => {
            return Ok(SettlementOutcome {
                outcome: resolve_direction_from_open_close(open_price, close_price),
                open_price,
                close_price,
                source: SettlementSource::Binance,
            });
        }
        Err(error) => {
            log_warn(
                "Settlement",
                &format!("Binance resolution failed: {error}"),
            );
        }
    }

    let fallback = fallback_resolve_from_gamma(config, slug).await?;
    if let Some(outcome) = fallback {
        return Ok(SettlementOutcome {
            outcome,
            open_price: 0.0,
            close_price: 0.0,
            source: SettlementSource::Polymarket,
        });
    }

    bail!("unable to resolve market outcome from Binance and Polymarket")
}
