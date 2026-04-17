use anyhow::{anyhow, Result};
use reqwest::Client;
use serde::Deserialize;

use crate::types::{OrderBookSnapshot, Config};

#[derive(Debug, Deserialize)]
struct BookLevelRaw {
    price: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OrderBookRaw {
    bids: Option<Vec<BookLevelRaw>>,
    asks: Option<Vec<BookLevelRaw>>,
    min_order_size: Option<String>,
}

fn parse_best_ask(asks: &[BookLevelRaw]) -> (f64, bool, usize, usize) {
    let ask_levels = asks.len();
    if asks.is_empty() {
        return (0.0, false, ask_levels, 0);
    }

    let mut numeric: Vec<f64> = asks
        .iter()
        .filter_map(|ask| ask.price.as_deref())
        .filter_map(|price| price.parse::<f64>().ok())
        .filter(|price| price.is_finite() && *price > 0.0)
        .collect();

    numeric.sort_by(|a, b| a.total_cmp(b));
    let parsed_ask_levels = numeric.len();

    if let Some(best_ask) = numeric.first() {
        (*best_ask, true, ask_levels, parsed_ask_levels)
    } else {
        (0.0, false, ask_levels, parsed_ask_levels)
    }
}

fn parse_best_bid(bids: &[BookLevelRaw]) -> f64 {
    if bids.is_empty() {
        return 0.0;
    }

    let mut numeric: Vec<f64> = bids
        .iter()
        .filter_map(|bid| bid.price.as_deref())
        .filter_map(|price| price.parse::<f64>().ok())
        .filter(|price| price.is_finite() && *price > 0.0)
        .collect();

    numeric.sort_by(|a, b| b.total_cmp(a));
    numeric.first().copied().unwrap_or(0.0)
}

pub async fn fetch_orderbook_snapshot(config: &Config, token_id: &str) -> Result<OrderBookSnapshot> {
    let base = config.polymarket_clob_url.trim_end_matches('/');
    let url = format!("{base}/book");

    let response = Client::new()
        .get(url)
        .query(&[("token_id", token_id)])
        .timeout(std::time::Duration::from_millis(7000))
        .send()
        .await?;

    if !response.status().is_success() {
        return Err(anyhow!(
            "orderbook fetch failed with HTTP {}",
            response.status()
        ));
    }

    let payload: OrderBookRaw = response.json().await?;
    let asks_raw = payload.asks.unwrap_or_default();
    let bids_raw = payload.bids.unwrap_or_default();

    let (best_ask, asks_present, ask_levels, parsed_ask_levels) = parse_best_ask(&asks_raw);
    let best_bid = parse_best_bid(&bids_raw);

    Ok(OrderBookSnapshot {
        best_bid,
        best_ask,
        asks_present,
        ask_levels,
        parsed_ask_levels,
        min_order_size: payload
            .min_order_size
            .as_deref()
            .and_then(|value| value.parse::<f64>().ok())
            .unwrap_or(0.0),
        tick_size: "0.01".to_owned(),
    })
}
