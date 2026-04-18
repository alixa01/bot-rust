use anyhow::{anyhow, bail, Result};
use reqwest::Client;
use serde_json::Value;

use crate::types::Config;
use crate::utils::network::fetch_with_timeout;

const DEFAULT_BINANCE_BASE_URLS: [&str; 5] = [
    "https://data-api.binance.vision/api/v3",
    "https://api.binance.com/api/v3",
    "https://api1.binance.com/api/v3",
    "https://api2.binance.com/api/v3",
    "https://api3.binance.com/api/v3",
];

fn normalize_base_url(value: &str) -> String {
    value.trim_end_matches('/').to_owned()
}

fn build_binance_base_urls(preferred_base_url: &str) -> Vec<String> {
    let preferred = normalize_base_url(preferred_base_url);
    let mut out = Vec::new();

    if !preferred.is_empty() {
        out.push(preferred);
    }

    for base in DEFAULT_BINANCE_BASE_URLS {
        let normalized = normalize_base_url(base);
        if !out.iter().any(|existing| existing == &normalized) {
            out.push(normalized);
        }
    }

    out
}

async fn fetch_from_any_binance(binance_base_url: &str, path: &str) -> Result<reqwest::Response> {
    let client = Client::new();
    let mut last_error: Option<anyhow::Error> = None;

    for base in build_binance_base_urls(binance_base_url) {
        let url = format!("{base}{path}");
        match fetch_with_timeout(&client, &url, 7000).await {
            Ok(response) if response.status().is_success() => {
                return Ok(response);
            }
            Ok(response) => {
                last_error = Some(anyhow!("HTTP {} from {}", response.status(), base));
            }
            Err(error) => {
                last_error = Some(error);
            }
        }
    }

    Err(last_error.unwrap_or_else(|| anyhow!("all binance endpoints failed")))
}

fn parse_kline_price(
    rows: &Value,
    candle_index: usize,
    field_index: usize,
    field_name: &str,
) -> Result<f64> {
    let row = rows
        .as_array()
        .and_then(|arr| arr.get(candle_index))
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing Binance candle index {candle_index}"))?;

    let parsed = row
        .get(field_index)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("invalid {field_name} field from Binance"))?
        .parse::<f64>()?;

    if !parsed.is_finite() || parsed <= 0.0 {
        bail!("invalid {field_name} price from Binance");
    }

    Ok(parsed)
}

pub async fn fetch_window_open_close(config: &Config, window_start_sec: u64) -> Result<(f64, f64)> {
    let path = format!(
        "/klines?symbol=BTCUSDT&interval=1m&startTime={}&limit=5",
        window_start_sec.saturating_mul(1000)
    );

    let response = fetch_from_any_binance(&config.binance_base_url, &path).await?;
    let rows: Value = response.json().await?;

    let candle_count = rows.as_array().map(|arr| arr.len()).unwrap_or(0);
    if candle_count < 5 {
        bail!("insufficient 1m candles to resolve 5m window");
    }

    let open_price = parse_kline_price(&rows, 0, 1, "open")?;
    let close_price = parse_kline_price(&rows, 4, 4, "close")?;

    Ok((open_price, close_price))
}

pub async fn fetch_window_open_price(config: &Config, window_start_sec: u64) -> Result<f64> {
    let path = format!(
        "/klines?symbol=BTCUSDT&interval=1m&startTime={}&limit=1",
        window_start_sec.saturating_mul(1000)
    );

    let response = fetch_from_any_binance(&config.binance_base_url, &path).await?;
    let rows: Value = response.json().await?;
    let first = rows
        .as_array()
        .and_then(|arr| arr.first())
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("no candle found for window open price"))?;

    let open_price = first
        .get(1)
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("invalid open price field from Binance"))?
        .parse::<f64>()?;

    if !open_price.is_finite() || open_price <= 0.0 {
        bail!("invalid BTC open price from Binance");
    }

    Ok(open_price)
}
