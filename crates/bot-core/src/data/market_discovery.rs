use anyhow::{anyhow, Result};
use reqwest::Client;
use serde_json::Value;

use crate::types::{DiscoveredMarket, MarketSide, Config};

#[derive(Debug, Clone)]
pub struct GammaResolutionProbe {
    pub resolved: bool,
    pub yes_payout: Option<f64>,
    pub no_payout: Option<f64>,
    pub error: Option<String>,
}

fn is_bytes32(value: &str) -> bool {
    let trimmed = value.trim();
    trimmed.len() == 66
        && trimmed.starts_with("0x")
        && trimmed.chars().skip(2).all(|c| c.is_ascii_hexdigit())
}

fn parse_string_array(raw: &Value) -> Vec<String> {
    match raw {
        Value::Array(items) => items
            .iter()
            .filter_map(|item| item.as_str())
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .collect(),
        Value::String(value) if !value.trim().is_empty() => {
            if let Ok(Value::Array(items)) = serde_json::from_str::<Value>(value) {
                items
                    .iter()
                    .filter_map(|item| item.as_str())
                    .map(|v| v.trim().to_owned())
                    .filter(|v| !v.is_empty())
                    .collect()
            } else {
                value
                    .split(',')
                    .map(|v| v.trim().trim_matches('"').to_owned())
                    .filter(|v| !v.is_empty())
                    .collect()
            }
        }
        _ => Vec::new(),
    }
}

fn parse_number_array(raw: &Value) -> Vec<Option<f64>> {
    match raw {
        Value::Array(items) => items
            .iter()
            .map(|item| item.as_f64())
            .map(|value| value.filter(|v| v.is_finite()))
            .collect(),
        Value::String(value) if !value.trim().is_empty() => {
            if let Ok(parsed) = serde_json::from_str::<Value>(value) {
                return parse_number_array(&parsed);
            }

            value
                .split(',')
                .map(|v| v.trim().trim_matches('"').parse::<f64>().ok())
                .map(|v| v.filter(|n| n.is_finite()))
                .collect()
        }
        _ => Vec::new(),
    }
}

fn yes_no_indices(outcomes: &[String]) -> Option<(usize, usize)> {
    let normalized: Vec<String> = outcomes.iter().map(|v| v.to_lowercase()).collect();
    let yes_aliases = ["yes", "up", "higher", "above", "true"];
    let no_aliases = ["no", "down", "lower", "below", "false"];

    let yes_index = normalized
        .iter()
        .position(|o| yes_aliases.iter().any(|alias| alias == o))?;
    let no_index = normalized
        .iter()
        .position(|o| no_aliases.iter().any(|alias| alias == o))?;

    if yes_index == no_index {
        None
    } else {
        Some((yes_index, no_index))
    }
}

fn normalize_condition_id(raw: Option<&Value>) -> Option<String> {
    let value = raw.and_then(Value::as_str)?.trim().to_lowercase();
    if is_bytes32(&value) {
        Some(value)
    } else {
        None
    }
}

fn extract_condition_id(market: &Value) -> Option<String> {
    normalize_condition_id(market.get("conditionId"))
        .or_else(|| normalize_condition_id(market.get("condition_id")))
        .or_else(|| normalize_condition_id(market.get("market")))
}

fn normalize_discovered_market(event: &Value, fallback_slug: &str) -> Option<DiscoveredMarket> {
    let event_slug = event
        .get("slug")
        .and_then(Value::as_str)
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
        .unwrap_or(fallback_slug)
        .to_owned();

    let event_question = event
        .get("title")
        .and_then(Value::as_str)
        .map(|v| v.trim().to_owned())
        .or_else(|| {
            event
                .get("question")
                .and_then(Value::as_str)
                .map(|v| v.trim().to_owned())
        })
        .unwrap_or_default();

    let markets = event.get("markets").and_then(Value::as_array)?;

    for market in markets {
        let token_ids = parse_string_array(market.get("clobTokenIds").unwrap_or(&Value::Null));
        let outcomes = parse_string_array(market.get("outcomes").unwrap_or(&Value::Null));
        let prices = parse_number_array(market.get("outcomePrices").unwrap_or(&Value::Null));

        let (yes_index, no_index) = match yes_no_indices(&outcomes) {
            Some(indices) => indices,
            None => continue,
        };

        if token_ids.len() < 2 || prices.len() <= yes_index || prices.len() <= no_index {
            continue;
        }

        let condition_id = match extract_condition_id(market) {
            Some(value) => value,
            None => continue,
        };

        let question = market
            .get("question")
            .and_then(Value::as_str)
            .map(|v| v.trim())
            .filter(|v| !v.is_empty())
            .unwrap_or(&event_question)
            .to_owned();

        return Some(DiscoveredMarket {
            slug: event_slug.clone(),
            condition_id,
            yes_token_id: token_ids[yes_index].clone(),
            no_token_id: token_ids[no_index].clone(),
            question,
            yes_price: prices[yes_index],
            no_price: prices[no_index],
        });
    }

    None
}

async fn fetch_events_by_slug(config: &Config, slug: &str) -> Result<Vec<Value>> {
    let base = config.polymarket_gamma_url.trim_end_matches('/');
    let response = Client::new()
        .get(format!("{base}/events"))
        .query(&[("slug", slug)])
        .timeout(std::time::Duration::from_millis(8000))
        .send()
        .await?;

    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(Vec::new());
    }

    if !response.status().is_success() {
        return Err(anyhow!("Gamma fetch failed with HTTP {}", response.status()));
    }

    let payload: Value = response.json().await?;
    Ok(payload.as_array().cloned().unwrap_or_default())
}

pub async fn fetch_market_by_slug(config: &Config, slug: &str) -> Result<Option<DiscoveredMarket>> {
    let events = fetch_events_by_slug(config, slug).await?;
    for event in events {
        if let Some(normalized) = normalize_discovered_market(&event, slug) {
            return Ok(Some(normalized));
        }
    }

    Ok(None)
}

pub async fn fallback_resolve_from_gamma(config: &Config, slug: &str) -> Result<Option<MarketSide>> {
    let market = fetch_market_by_slug(config, slug).await?;
    let Some(market) = market else {
        return Ok(None);
    };

    match (market.yes_price, market.no_price) {
        (Some(yes), Some(no)) if yes > no => Ok(Some(MarketSide::Up)),
        (Some(yes), Some(no)) if no > yes => Ok(Some(MarketSide::Down)),
        _ => Ok(None),
    }
}

fn parse_bool_like(value: Option<&Value>) -> Option<bool> {
    match value {
        Some(Value::Bool(v)) => Some(*v),
        Some(Value::Number(v)) => v.as_i64().map(|n| n != 0),
        Some(Value::String(v)) => match v.trim().to_lowercase().as_str() {
            "1" | "true" | "yes" | "y" => Some(true),
            "0" | "false" | "no" | "n" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn derive_resolution_from_market(market: &Value) -> (bool, Option<f64>, Option<f64>) {
    let outcomes = parse_string_array(market.get("outcomes").unwrap_or(&Value::Null));
    let prices = parse_number_array(market.get("outcomePrices").unwrap_or(&Value::Null));

    let (mut yes_payout, mut no_payout) = (None, None);
    if let Some((yes_index, no_index)) = yes_no_indices(&outcomes) {
        if prices.len() > yes_index && prices.len() > no_index {
            yes_payout = prices[yes_index];
            no_payout = prices[no_index];
        }
    }

    let resolved_flag = parse_bool_like(market.get("resolved"));
    let closed_flag = parse_bool_like(market.get("closed"));
    let active_flag = parse_bool_like(market.get("active"));

    let mut resolved = resolved_flag == Some(true)
        || closed_flag == Some(true)
        || active_flag == Some(false);

    if let (Some(yes), Some(no)) = (yes_payout, no_payout) {
        if (yes + no - 1.0).abs() <= 0.01 && (yes == 1.0 || no == 1.0) {
            resolved = true;
        }
    }

    (resolved, yes_payout, no_payout)
}

pub async fn probe_gamma_resolution_by_slug(
    config: &Config,
    slug: &str,
    condition_id: &str,
) -> Result<GammaResolutionProbe> {
    let normalized_condition_id = condition_id.trim().to_lowercase();
    if !is_bytes32(&normalized_condition_id) {
        return Ok(GammaResolutionProbe {
            resolved: false,
            yes_payout: None,
            no_payout: None,
            error: Some(format!("invalid conditionId {condition_id}")),
        });
    }

    let events = fetch_events_by_slug(config, slug).await?;
    if events.is_empty() {
        return Ok(GammaResolutionProbe {
            resolved: false,
            yes_payout: None,
            no_payout: None,
            error: Some("gamma event not found by slug".to_owned()),
        });
    }

    for event in events {
        let Some(markets) = event.get("markets").and_then(Value::as_array) else {
            continue;
        };

        for market in markets {
            let Some(market_condition_id) = extract_condition_id(market) else {
                continue;
            };

            if market_condition_id != normalized_condition_id {
                continue;
            }

            let (resolved, yes_payout, no_payout) = derive_resolution_from_market(market);
            return Ok(GammaResolutionProbe {
                resolved,
                yes_payout,
                no_payout,
                error: None,
            });
        }
    }

    Ok(GammaResolutionProbe {
        resolved: false,
        yes_payout: None,
        no_payout: None,
        error: Some(format!(
            "conditionId {condition_id} not found in gamma slug payload"
        )),
    })
}
