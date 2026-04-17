use std::collections::HashMap;

use anyhow::{Error, Result};
use serde_json::{json, Map, Value};

use crate::data::orderbook::fetch_orderbook_snapshot;
use crate::execution::client::{get_clob_client, ClobClient};
use crate::types::{ExecutionResult, ExecutionStatus, Config};
use crate::utils::logger::{log_error, log_info, log_warn};
use crate::utils::time::{now_sec, sleep_ms};

#[derive(Debug, Clone)]
struct EntryDiagnostics {
    time_budget_start_sec: u64,
    attempts: u64,
    fok_attempts: u64,
    no_asks_count: u64,
    gate_reject_count: u64,
    orderbook_error_count: u64,
    last_attempt_status: Option<String>,
    last_best_bid: Option<f64>,
    last_best_ask: Option<f64>,
    last_final_price: Option<f64>,
    last_tick_size: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpentComputationMode {
    MarketBuyRequestedStake,
    FillSizeXPrice,
}

#[derive(Debug, Clone, Copy)]
struct SpentComputationContext {
    mode: SpentComputationMode,
    requested_stake_usd: Option<f64>,
}

#[derive(Debug, Clone, Copy)]
struct SpentComputationResult {
    spent_usd: f64,
    implied_spent_usd: f64,
    spent_source: &'static str,
    requested_stake_usd: Option<f64>,
}

#[derive(Debug, Clone)]
enum PollStatusOutcome {
    Terminal(ExecutionResult),
    Timeout,
}

struct SubmitFokParams<'a> {
    config: &'a Config,
    client: &'a ClobClient,
    token_id: &'a str,
    stake_usd: f64,
    final_price: f64,
    close_time_sec: u64,
    tick_size: &'a str,
}

struct SubmitLimitFallbackParams<'a> {
    config: &'a Config,
    client: &'a ClobClient,
    token_id: &'a str,
    requested_stake_usd: f64,
    close_time_sec: u64,
    tick_size: &'a str,
}

fn round_to_tick_ceil(price: f64, tick_size: &str) -> f64 {
    let tick = tick_size.trim().parse::<f64>().unwrap_or(0.0);
    if !tick.is_finite() || tick <= 0.0 {
        return price;
    }

    let rounded = (price / tick).ceil() * tick;
    let decimals = tick_size
        .split('.')
        .nth(1)
        .map(|value| value.len())
        .unwrap_or(0);
    let factor = 10_f64.powi(decimals as i32);
    (rounded * factor).round() / factor
}

fn apply_buy_slippage(base_price: f64, slippage_percent: f64) -> f64 {
    (base_price * (1.0 + (slippage_percent / 100.0))).min(0.99)
}

fn clamp_price(price: f64) -> f64 {
    if !price.is_finite() {
        return 0.99;
    }

    price.max(0.01).min(0.99)
}

fn status_poll_deadline_sec(close_time_sec: u64, status_poll_grace_sec: u64) -> u64 {
    close_time_sec.saturating_add(status_poll_grace_sec)
}

fn buy_band_reject_reason(price: f64, config: &Config) -> Option<&'static str> {
    if !config.entry_price_gate_enabled {
        return None;
    }

    if price < config.price_range_min {
        return Some("below_min");
    }

    if price > config.price_range_max {
        return Some("above_max");
    }

    None
}

fn format_error_chain(error: &Error) -> String {
    error
        .chain()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(" -> ")
}

fn fallback_limit_price(config: &Config) -> f64 {
    const PREFERRED_FALLBACK_PRICE: f64 = 0.95;

    if !config.entry_price_gate_enabled {
        return PREFERRED_FALLBACK_PRICE;
    }

    let bounded = PREFERRED_FALLBACK_PRICE
        .max(config.price_range_min)
        .min(config.price_range_max);

    clamp_price(bounded)
}

fn status_label(status: ExecutionStatus) -> &'static str {
    match status {
        ExecutionStatus::Pending => "PENDING",
        ExecutionStatus::Filled => "FILLED",
        ExecutionStatus::Partial => "PARTIAL",
        ExecutionStatus::Cancelled => "CANCELLED",
        ExecutionStatus::Failed => "FAILED",
        ExecutionStatus::Skipped => "SKIPPED",
    }
}

fn value_shape(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn looks_like_order_status_record(record: &Map<String, Value>) -> bool {
    record.contains_key("status")
        || record.contains_key("size_matched")
        || record.contains_key("price")
}

fn extract_status_record<'a>(payload: &'a Value) -> Option<&'a Map<String, Value>> {
    if let Some(record) = payload.as_object() {
        if looks_like_order_status_record(record) {
            return Some(record);
        }

        if let Some(order) = record.get("order").and_then(Value::as_object) {
            if looks_like_order_status_record(order) {
                return Some(order);
            }
        }

        if let Some(data) = record.get("data").and_then(Value::as_object) {
            if looks_like_order_status_record(data) {
                return Some(data);
            }

            if let Some(order) = data.get("order").and_then(Value::as_object) {
                if looks_like_order_status_record(order) {
                    return Some(order);
                }
            }
        }
    }

    if let Some(items) = payload.as_array() {
        if let Some(first) = items.first() {
            return extract_status_record(first);
        }
    }

    None
}

fn parse_f64(value: Option<&Value>) -> f64 {
    match value {
        Some(Value::Number(number)) => number.as_f64().unwrap_or(0.0),
        Some(Value::String(text)) => text.trim().parse::<f64>().unwrap_or(0.0),
        _ => 0.0,
    }
}

fn to_raw_map(value: Value) -> HashMap<String, Value> {
    if let Value::Object(object) = value {
        object.into_iter().collect()
    } else {
        let mut map = HashMap::new();
        map.insert("value".to_owned(), value);
        map
    }
}

fn build_skipped_result(
    reason: &str,
    diagnostics: &EntryDiagnostics,
    extras: Map<String, Value>,
) -> ExecutionResult {
    let mut raw = HashMap::new();
    raw.insert("reason".to_owned(), json!(reason));

    for (key, value) in extras {
        raw.insert(key, value);
    }

    raw.insert(
        "timeBudgetStartSec".to_owned(),
        json!(diagnostics.time_budget_start_sec),
    );
    raw.insert("attempts".to_owned(), json!(diagnostics.attempts));
    raw.insert("fokAttempts".to_owned(), json!(diagnostics.fok_attempts));
    raw.insert("noAsksCount".to_owned(), json!(diagnostics.no_asks_count));
    raw.insert(
        "gateRejectCount".to_owned(),
        json!(diagnostics.gate_reject_count),
    );
    raw.insert(
        "orderbookErrorCount".to_owned(),
        json!(diagnostics.orderbook_error_count),
    );
    raw.insert(
        "lastAttemptStatus".to_owned(),
        diagnostics
            .last_attempt_status
            .as_ref()
            .map(|value| json!(value))
            .unwrap_or(Value::Null),
    );
    raw.insert(
        "lastBestBid".to_owned(),
        diagnostics
            .last_best_bid
            .map(|value| json!(value))
            .unwrap_or(Value::Null),
    );
    raw.insert(
        "lastBestAsk".to_owned(),
        diagnostics
            .last_best_ask
            .map(|value| json!(value))
            .unwrap_or(Value::Null),
    );
    raw.insert(
        "lastFinalPrice".to_owned(),
        diagnostics
            .last_final_price
            .map(|value| json!(value))
            .unwrap_or(Value::Null),
    );
    raw.insert(
        "lastTickSize".to_owned(),
        diagnostics
            .last_tick_size
            .as_ref()
            .map(|value| json!(value))
            .unwrap_or(Value::Null),
    );

    ExecutionResult {
        status: ExecutionStatus::Skipped,
        order_id: String::new(),
        filled_price: diagnostics.last_final_price.unwrap_or(0.0),
        filled_size: 0.0,
        spent_usd: 0.0,
        used_fallback_limit: false,
        raw_response: raw,
    }
}

fn compute_spent_usd(
    context: SpentComputationContext,
    raw_status: &str,
    filled_size: f64,
    filled_price: f64,
) -> SpentComputationResult {
    let implied_spent_usd = if filled_size.is_finite() && filled_price.is_finite() {
        (filled_size * filled_price).max(0.0)
    } else {
        0.0
    };

    if context.mode != SpentComputationMode::MarketBuyRequestedStake {
        return SpentComputationResult {
            spent_usd: implied_spent_usd,
            implied_spent_usd,
            spent_source: "filled_size_x_price",
            requested_stake_usd: None,
        };
    }

    let requested = context.requested_stake_usd.unwrap_or(f64::NAN);
    let has_requested = requested.is_finite() && requested > 0.0;
    if !has_requested {
        return SpentComputationResult {
            spent_usd: implied_spent_usd,
            implied_spent_usd,
            spent_source: "filled_size_x_price",
            requested_stake_usd: None,
        };
    }

    if raw_status == "MATCHED" || raw_status == "FILLED" {
        return SpentComputationResult {
            spent_usd: requested,
            implied_spent_usd,
            spent_source: "requested_stake_market_buy",
            requested_stake_usd: Some(requested),
        };
    }

    if filled_size <= 0.0 || filled_price <= 0.0 {
        return SpentComputationResult {
            spent_usd: 0.0,
            implied_spent_usd,
            spent_source: "requested_stake_market_buy_no_fill",
            requested_stake_usd: Some(requested),
        };
    }

    SpentComputationResult {
        spent_usd: requested.min(implied_spent_usd),
        implied_spent_usd,
        spent_source: "requested_stake_market_buy_capped_partial",
        requested_stake_usd: Some(requested),
    }
}

fn attach_fill_band_audit(result: ExecutionResult, config: &Config) -> ExecutionResult {
    if !config.entry_price_gate_enabled {
        return result;
    }

    if !result.filled_price.is_finite() || result.filled_price <= 0.0 || result.filled_size <= 0.0 {
        return result;
    }

    let Some(violation) = buy_band_reject_reason(result.filled_price, config) else {
        return result;
    };

    log_error(
        "Entry",
        &format!(
            "fill band violation order={} filled={:.3} band={:.2}-{:.2} reason={}",
            result.order_id,
            result.filled_price,
            config.price_range_min,
            config.price_range_max,
            violation
        ),
    );

    let mut raw = result.raw_response.clone();
    raw.insert("fillBandViolation".to_owned(), json!(true));
    raw.insert(
        "fillBandViolationReason".to_owned(),
        json!(violation),
    );
    raw.insert(
        "fillBandObservedPrice".to_owned(),
        json!(result.filled_price),
    );
    raw.insert("fillBandMin".to_owned(), json!(config.price_range_min));
    raw.insert("fillBandMax".to_owned(), json!(config.price_range_max));

    ExecutionResult { raw_response: raw, ..result }
}

async fn poll_final_order_status(
    config: &Config,
    client: &ClobClient,
    order_id: &str,
    poll_until_sec: u64,
    spent_context: SpentComputationContext,
    used_fallback_limit: bool,
) -> Result<PollStatusOutcome> {
    while now_sec() <= poll_until_sec {
        let status = client.get_order(order_id).await?;
        let Some(status_value) = status else {
            log_warn(
                "Entry",
                &format!(
                    "order={} get_order returned empty status; treat as transient and continue polling",
                    order_id
                ),
            );
            sleep_ms(config.status_poll_interval_ms).await;
            continue;
        };

        let Some(status_record) = extract_status_record(&status_value) else {
            let shape = value_shape(&status_value);
            log_warn(
                "Entry",
                &format!(
                    "order={} get_order returned unsupported status payload shape={} payload={} ; treat as transient and continue polling",
                    order_id,
                    shape,
                    truncate_text(&status_value.to_string(), 220)
                ),
            );
            sleep_ms(config.status_poll_interval_ms).await;
            continue;
        };

        let raw_status = status_record
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("PENDING")
            .to_uppercase();

        let filled_size = parse_f64(status_record.get("size_matched"));
        let filled_price = parse_f64(status_record.get("price"));

        let spent = compute_spent_usd(spent_context, &raw_status, filled_size, filled_price);

        if spent_context.mode == SpentComputationMode::MarketBuyRequestedStake
            && (raw_status == "MATCHED" || raw_status == "FILLED")
            && spent.requested_stake_usd.is_some()
        {
            let requested = spent.requested_stake_usd.unwrap_or(0.0);
            let diff = (spent.implied_spent_usd - requested).abs();
            if diff >= 0.01 {
                log_info(
                    "Entry",
                    &format!(
                        "order={} implied spent=${:.2} from size*price differs from requested stake=${:.2}; using requested stake for market BUY accounting",
                        order_id,
                        spent.implied_spent_usd,
                        requested
                    ),
                );
            }
        }

        let mut raw_response: HashMap<String, Value> = status_record
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        raw_response.insert("spentSource".to_owned(), json!(spent.spent_source));
        raw_response.insert("impliedSpentUsd".to_owned(), json!(spent.implied_spent_usd));
        raw_response.insert(
            "requestedStakeUsd".to_owned(),
            spent
                .requested_stake_usd
                .map(|value| json!(value))
                .unwrap_or(Value::Null),
        );

        if raw_status == "MATCHED" || raw_status == "FILLED" {
            return Ok(PollStatusOutcome::Terminal(attach_fill_band_audit(
                ExecutionResult {
                    status: ExecutionStatus::Filled,
                    order_id: order_id.to_owned(),
                    filled_price,
                    filled_size,
                    spent_usd: spent.spent_usd,
                    used_fallback_limit,
                    raw_response,
                },
                config,
            )));
        }

        if raw_status == "CANCELED" || raw_status == "CANCELLED" || raw_status == "REJECTED" {
            return Ok(PollStatusOutcome::Terminal(attach_fill_band_audit(
                ExecutionResult {
                    status: if filled_size > 0.0 {
                        ExecutionStatus::Partial
                    } else {
                        ExecutionStatus::Cancelled
                    },
                    order_id: order_id.to_owned(),
                    filled_price,
                    filled_size,
                    spent_usd: spent.spent_usd,
                    used_fallback_limit,
                    raw_response,
                },
                config,
            )));
        }

        sleep_ms(config.status_poll_interval_ms).await;
    }

    Ok(PollStatusOutcome::Timeout)
}

fn string_field(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
        .map(ToOwned::to_owned)
}

fn extract_order_id_from_object(record: &Map<String, Value>) -> Option<String> {
    string_field(record.get("orderID"))
        .or_else(|| string_field(record.get("orderId")))
        .or_else(|| string_field(record.get("order_id")))
        .or_else(|| string_field(record.get("id")))
        .or_else(|| {
            record
                .get("order")
                .and_then(Value::as_object)
                .and_then(extract_order_id_from_object)
        })
        .or_else(|| {
            record
                .get("data")
                .and_then(Value::as_object)
                .and_then(extract_order_id_from_object)
        })
}

fn summarize_top_level_keys(value: &Value) -> String {
    if let Some(record) = value.as_object() {
        let mut keys: Vec<String> = record.keys().cloned().collect();
        keys.sort();
        return keys.join(",");
    }

    if value.is_array() {
        return "<array>".to_owned();
    }

    format!("<{}>", value_shape(value))
}

fn extract_order_id(post_result: &Value) -> String {
    if let Some(value) = post_result.as_str() {
        let trimmed = value.trim();
        if !trimmed.is_empty() {
            return trimmed.to_owned();
        }
    }

    if let Some(record) = post_result.as_object() {
        if let Some(order_id) = extract_order_id_from_object(record) {
            return order_id;
        }
    }

    String::new()
}

async fn submit_fok_buy(params: SubmitFokParams<'_>) -> Result<Option<ExecutionResult>> {
    let signed_order = params
        .client
        .create_market_order_buy(
            params.token_id,
            params.stake_usd,
            params.final_price,
            params.tick_size,
        )
        .await?;

    let post_result = params
        .client
        .post_order(&signed_order, "FOK", false, None)
        .await?;

    let order_id = extract_order_id(&post_result);
    if order_id.is_empty() {
        log_warn(
            "Entry",
            &format!(
                "FOK post_order returned without order id keys=[{}] payload={}",
                summarize_top_level_keys(&post_result),
                truncate_text(&post_result.to_string(), 240)
            ),
        );
        return Ok(None);
    }

    let poll_until_sec = status_poll_deadline_sec(
        params.close_time_sec,
        params.config.status_poll_grace_sec,
    );

    match poll_final_order_status(
            params.config,
            params.client,
            &order_id,
            poll_until_sec,
            SpentComputationContext {
                mode: SpentComputationMode::MarketBuyRequestedStake,
                requested_stake_usd: Some(params.stake_usd),
            },
            false,
        )
        .await?
    {
        PollStatusOutcome::Terminal(polled) => Ok(Some(polled)),
        PollStatusOutcome::Timeout => {
            let mut raw = to_raw_map(post_result);
            raw.insert("reason".to_owned(), json!("order_status_poll_timeout"));
            raw.insert(
                "statusPollIntervalMs".to_owned(),
                json!(params.config.status_poll_interval_ms),
            );
            raw.insert(
                "statusPollGraceSec".to_owned(),
                json!(params.config.status_poll_grace_sec),
            );
            raw.insert("statusPollUntilSec".to_owned(), json!(poll_until_sec));

            Ok(Some(ExecutionResult {
                status: ExecutionStatus::Pending,
                order_id,
                filled_price: params.final_price,
                filled_size: 0.0,
                spent_usd: 0.0,
                used_fallback_limit: false,
                raw_response: raw,
            }))
        }
    }
}

async fn submit_limit_fallback(
    params: SubmitLimitFallbackParams<'_>,
) -> Result<Option<ExecutionResult>> {
    let fallback_price = fallback_limit_price(params.config);
    if let Some(reject_reason) = buy_band_reject_reason(fallback_price, params.config) {
        let mut raw = HashMap::new();
        raw.insert("reason".to_owned(), json!("buy_price_gate_reject"));
        raw.insert("gateSource".to_owned(), json!("fallback_limit_price"));
        raw.insert("gateReason".to_owned(), json!(reject_reason));
        raw.insert("gateMin".to_owned(), json!(params.config.price_range_min));
        raw.insert("gateMax".to_owned(), json!(params.config.price_range_max));
        raw.insert("fallbackPrice".to_owned(), json!(fallback_price));

        return Ok(Some(ExecutionResult {
            status: ExecutionStatus::Skipped,
            order_id: String::new(),
            filled_price: fallback_price,
            filled_size: 0.0,
            spent_usd: 0.0,
            used_fallback_limit: true,
            raw_response: raw,
        }));
    }

    let min_stake_by_rule = 5.0 * fallback_price;
    let stake_usd = params.requested_stake_usd.max(min_stake_by_rule);
    let shares = 5.0_f64.max(stake_usd / fallback_price);

    let signed_order = params
        .client
        .create_limit_order_buy(params.token_id, shares, fallback_price, params.tick_size)
        .await?;

    let post_result = params
        .client
        .post_order(&signed_order, "GTC", false, Some(false))
        .await?;

    let order_id = extract_order_id(&post_result);
    if order_id.is_empty() {
        log_warn(
            "Entry",
            &format!(
                "GTC post_order returned without order id keys=[{}] payload={}",
                summarize_top_level_keys(&post_result),
                truncate_text(&post_result.to_string(), 240)
            ),
        );
        return Ok(None);
    }

    let poll_until_sec = status_poll_deadline_sec(
        params.close_time_sec,
        params.config.status_poll_grace_sec,
    );

    match poll_final_order_status(
            params.config,
            params.client,
            &order_id,
            poll_until_sec,
            SpentComputationContext {
                mode: SpentComputationMode::FillSizeXPrice,
                requested_stake_usd: None,
            },
            true,
        )
        .await?
    {
        PollStatusOutcome::Terminal(polled) => Ok(Some(polled)),
        PollStatusOutcome::Timeout => {
            let mut raw = to_raw_map(post_result);
            raw.insert("reason".to_owned(), json!("order_status_poll_timeout"));
            raw.insert(
                "statusPollIntervalMs".to_owned(),
                json!(params.config.status_poll_interval_ms),
            );
            raw.insert(
                "statusPollGraceSec".to_owned(),
                json!(params.config.status_poll_grace_sec),
            );
            raw.insert("statusPollUntilSec".to_owned(), json!(poll_until_sec));

            Ok(Some(ExecutionResult {
                status: ExecutionStatus::Pending,
                order_id,
                filled_price: fallback_price,
                filled_size: 0.0,
                spent_usd: 0.0,
                used_fallback_limit: true,
                raw_response: raw,
            }))
        }
    }
}

fn stop_for_attempt_limit(
    diagnostics: &mut EntryDiagnostics,
    config: &Config,
    attempt: u64,
) -> ExecutionResult {
    diagnostics.last_attempt_status = Some("ATTEMPT_LIMIT_REACHED".to_owned());
    log_warn(
        "Entry",
        &format!(
            "attempt #{} stop retries: maxAttempts={}",
            attempt, config.order_max_attempts
        ),
    );

    let mut extras = Map::new();
    extras.insert("maxAttempts".to_owned(), json!(config.order_max_attempts));
    extras.insert("retryIntervalMs".to_owned(), json!(config.order_retry_interval_ms));

    build_skipped_result("order_max_attempts_exceeded", diagnostics, extras)
}

pub async fn execute_live_entry(
    config: &Config,
    token_id: &str,
    stake_usd: f64,
    close_time_sec: u64,
) -> Result<ExecutionResult> {
    let started_at_sec = now_sec();

    let mut diagnostics = EntryDiagnostics {
        time_budget_start_sec: close_time_sec.saturating_sub(started_at_sec),
        attempts: 0,
        fok_attempts: 0,
        no_asks_count: 0,
        gate_reject_count: 0,
        orderbook_error_count: 0,
        last_attempt_status: None,
        last_best_bid: None,
        last_best_ask: None,
        last_final_price: None,
        last_tick_size: None,
    };

    log_info(
        "Entry",
        &format!(
            "start closeIn={}s stake=${:.2} retry={}ms maxAttempts={}",
            diagnostics.time_budget_start_sec,
            stake_usd,
            config.order_retry_interval_ms,
            config.order_max_attempts
        ),
    );

    if diagnostics.time_budget_start_sec == 0 {
        diagnostics.last_attempt_status = Some("WINDOW_ALREADY_CLOSED".to_owned());
        let mut extras = Map::new();
        extras.insert("closeTimeSec".to_owned(), json!(close_time_sec));
        extras.insert("nowSec".to_owned(), json!(started_at_sec));
        return Ok(build_skipped_result(
            "window_already_closed",
            &diagnostics,
            extras,
        ));
    }

    let client = get_clob_client(config)?;

    while now_sec() < close_time_sec {
        diagnostics.attempts += 1;
        let attempt = diagnostics.attempts;

        let attempt_start_sec = now_sec();
        let time_to_close_before_attempt = close_time_sec.saturating_sub(attempt_start_sec);

        if time_to_close_before_attempt <= 1 {
            diagnostics.last_attempt_status = Some("TIME_BUDGET_BEFORE_ATTEMPT".to_owned());
            log_warn(
                "Entry",
                &format!(
                    "attempt #{} skipped: close too near (ttc={}s)",
                    attempt, time_to_close_before_attempt
                ),
            );

            let mut extras = Map::new();
            extras.insert("timeToCloseSec".to_owned(), json!(time_to_close_before_attempt));

            return Ok(build_skipped_result(
                "entry_time_budget_exhausted_before_attempt",
                &diagnostics,
                extras,
            ));
        }

        let orderbook = match fetch_orderbook_snapshot(config, token_id).await {
            Ok(book) => book,
            Err(error) => {
                diagnostics.orderbook_error_count += 1;
                diagnostics.last_attempt_status = Some("ORDERBOOK_FETCH_ERROR".to_owned());
                let message = error.to_string();

                log_warn(
                    "Entry",
                    &format!("attempt #{} orderbook failed: {}", attempt, message),
                );

                if attempt >= config.order_max_attempts {
                    return Ok(stop_for_attempt_limit(&mut diagnostics, config, attempt));
                }

                let remaining_ms = close_time_sec.saturating_sub(now_sec()) * 1_000;
                if remaining_ms <= config.order_retry_interval_ms {
                    diagnostics.last_attempt_status = Some("ORDERBOOK_ERROR_UNTIL_CLOSE".to_owned());

                    let mut extras = Map::new();
                    extras.insert("error".to_owned(), json!(truncate_text(&message, 180)));
                    extras.insert("remainingMs".to_owned(), json!(remaining_ms));
                    extras.insert(
                        "retryIntervalMs".to_owned(),
                        json!(config.order_retry_interval_ms),
                    );

                    return Ok(build_skipped_result(
                        "orderbook_error_until_close",
                        &diagnostics,
                        extras,
                    ));
                }

                sleep_ms(config.order_retry_interval_ms).await;
                continue;
            }
        };

        diagnostics.last_best_bid = if orderbook.best_bid > 0.0 {
            Some(orderbook.best_bid)
        } else {
            None
        };
        diagnostics.last_best_ask = if orderbook.asks_present && orderbook.best_ask > 0.0 {
            Some(orderbook.best_ask)
        } else {
            None
        };

        let tick_size = match client.get_tick_size(token_id).await {
            Ok(value) => value,
            Err(error) => {
                let message = error.to_string();
                log_warn(
                    "Entry",
                    &format!(
                        "attempt #{} tick size fetch failed, fallback to 0.01: {}",
                        attempt, message
                    ),
                );
                "0.01".to_owned()
            }
        };
        diagnostics.last_tick_size = Some(tick_size.clone());

        log_info(
            "Entry",
            &format!(
                "attempt #{} ttc={}s ob bid={:.3} ask={} asks={}",
                attempt,
                close_time_sec.saturating_sub(now_sec()),
                orderbook.best_bid,
                if orderbook.asks_present {
                    format!("{:.3}", orderbook.best_ask)
                } else {
                    "NA".to_owned()
                },
                if orderbook.asks_present { "Y" } else { "N" }
            ),
        );

        if orderbook.asks_present && orderbook.best_ask > 0.0 {
            if let Some(reject_reason) = buy_band_reject_reason(orderbook.best_ask, config) {
                diagnostics.gate_reject_count += 1;
                diagnostics.last_attempt_status = Some("GATE_REJECT_BEST_ASK".to_owned());
                diagnostics.last_final_price = Some(orderbook.best_ask);

                log_warn(
                    "Entry",
                    &format!(
                        "attempt #{} gate reject bestAsk={:.3} band={:.2}-{:.2} reason={}",
                        attempt,
                        orderbook.best_ask,
                        config.price_range_min,
                        config.price_range_max,
                        reject_reason
                    ),
                );

                let mut extras = Map::new();
                extras.insert("observedPrice".to_owned(), json!(orderbook.best_ask));
                extras.insert("gateSource".to_owned(), json!("best_ask"));
                extras.insert("gateReason".to_owned(), json!(reject_reason));
                extras.insert("gateMin".to_owned(), json!(config.price_range_min));
                extras.insert("gateMax".to_owned(), json!(config.price_range_max));

                return Ok(build_skipped_result(
                    "buy_price_gate_reject",
                    &diagnostics,
                    extras,
                ));
            }

            let slipped = apply_buy_slippage(orderbook.best_ask, config.entry_slippage_percent_buy);
            let final_price = round_to_tick_ceil(slipped, &tick_size);
            diagnostics.last_final_price = Some(final_price);

            if let Some(reject_reason) = buy_band_reject_reason(final_price, config) {
                diagnostics.gate_reject_count += 1;
                diagnostics.last_attempt_status = Some("GATE_REJECT_SUBMIT_BAND".to_owned());

                log_warn(
                    "Entry",
                    &format!(
                        "attempt #{} gate reject submitPrice={:.3} band={:.2}-{:.2} reason={}",
                        attempt,
                        final_price,
                        config.price_range_min,
                        config.price_range_max,
                        reject_reason
                    ),
                );

                let mut extras = Map::new();
                extras.insert("finalPrice".to_owned(), json!(final_price));
                extras.insert("slippedPrice".to_owned(), json!(slipped));
                extras.insert("gateSource".to_owned(), json!("submit_price"));
                extras.insert("gateReason".to_owned(), json!(reject_reason));
                extras.insert("gateMin".to_owned(), json!(config.price_range_min));
                extras.insert("gateMax".to_owned(), json!(config.price_range_max));

                return Ok(build_skipped_result(
                    "buy_price_gate_reject",
                    &diagnostics,
                    extras,
                ));
            }

            let min_by_shares = orderbook.min_order_size.max(0.0) * final_price;
            if min_by_shares > 0.0 && stake_usd < min_by_shares {
                log_warn(
                    "Entry",
                    &format!(
                        "attempt #{} requested stake ${:.2} below min_order_size {:.6} (~${:.2} at price {:.3}). Proceeding with requested stake.",
                        attempt,
                        stake_usd,
                        orderbook.min_order_size,
                        min_by_shares,
                        final_price
                    ),
                );
            }

            diagnostics.fok_attempts += 1;
            log_info(
                "Entry",
                &format!(
                    "attempt #{} FOK submit final={:.3} stake=${:.2}",
                    attempt, final_price, stake_usd
                ),
            );

            let result = match submit_fok_buy(SubmitFokParams {
                config,
                client: &client,
                token_id,
                stake_usd,
                final_price,
                close_time_sec,
                tick_size: &tick_size,
            })
            .await
            {
                Ok(value) => value,
                Err(error) => {
                    diagnostics.last_attempt_status = Some("FOK_SUBMIT_ERROR".to_owned());
                    log_warn(
                        "Entry",
                        &format!(
                            "attempt #{} FOK submit failed: {}",
                            attempt,
                            format_error_chain(&error)
                        ),
                    );
                    None
                }
            };

            if let Some(ref value) = result {
                diagnostics.last_attempt_status = Some(format!("FOK_{}", status_label(value.status)));
                log_info(
                    "Entry",
                    &format!(
                        "attempt #{} FOK result status={} filled={:.4} spent=${:.2} reqStake=${:.2}",
                        attempt,
                        status_label(value.status),
                        value.filled_size,
                        value.spent_usd,
                        stake_usd,
                    ),
                );
            } else {
                diagnostics.last_attempt_status = Some("FOK_NO_ORDER_ID".to_owned());
                log_warn(
                    "Entry",
                    &format!("attempt #{} FOK returned empty order id", attempt),
                );
            }

            if let Some(value) = result {
                if value.status == ExecutionStatus::Filled || value.status == ExecutionStatus::Partial {
                    return Ok(value);
                }
            }
        } else if config.enable_fallback_gtc_limit {
            diagnostics.no_asks_count += 1;
            diagnostics.last_attempt_status = Some("NO_ASKS_FALLBACK_GTC".to_owned());
            log_info(
                "Entry",
                &format!("attempt #{} no asks -> fallback GTC", attempt),
            );

            let fallback = match submit_limit_fallback(SubmitLimitFallbackParams {
                config,
                client: &client,
                token_id,
                requested_stake_usd: stake_usd,
                close_time_sec,
                tick_size: &tick_size,
            })
            .await
            {
                Ok(value) => value,
                Err(error) => {
                    diagnostics.last_attempt_status = Some("GTC_SUBMIT_ERROR".to_owned());
                    log_warn(
                        "Entry",
                        &format!(
                            "attempt #{} GTC fallback failed: {}",
                            attempt,
                            format_error_chain(&error)
                        ),
                    );
                    None
                }
            };

            if let Some(ref value) = fallback {
                diagnostics.last_attempt_status = Some(format!("GTC_{}", status_label(value.status)));
                log_info(
                    "Entry",
                    &format!(
                        "attempt #{} GTC result status={} filled={:.4} spent=${:.2}",
                        attempt,
                        status_label(value.status),
                        value.filled_size,
                        value.spent_usd,
                    ),
                );
            } else {
                diagnostics.last_attempt_status = Some("GTC_NO_ORDER_ID".to_owned());
                log_warn(
                    "Entry",
                    &format!("attempt #{} GTC fallback returned empty order id", attempt),
                );
            }

            if let Some(value) = fallback {
                if value.status == ExecutionStatus::Filled || value.status == ExecutionStatus::Partial {
                    return Ok(value);
                }
            }
        } else {
            diagnostics.no_asks_count += 1;
            diagnostics.last_attempt_status = Some("NO_ASKS_FALLBACK_DISABLED".to_owned());
            log_warn(
                "Entry",
                &format!("attempt #{} no asks and fallback disabled", attempt),
            );
        }

        if attempt >= config.order_max_attempts {
            return Ok(stop_for_attempt_limit(&mut diagnostics, config, attempt));
        }

        let remaining_ms = close_time_sec.saturating_sub(now_sec()) * 1_000;
        if remaining_ms <= config.order_retry_interval_ms {
            diagnostics.last_attempt_status = Some("TIME_BUDGET_AFTER_ATTEMPT".to_owned());
            log_warn(
                "Entry",
                &format!(
                    "attempt #{} stop retries: remaining={}ms retry={}ms",
                    attempt, remaining_ms, config.order_retry_interval_ms
                ),
            );

            let mut extras = Map::new();
            extras.insert("remainingMs".to_owned(), json!(remaining_ms));
            extras.insert(
                "retryIntervalMs".to_owned(),
                json!(config.order_retry_interval_ms),
            );

            return Ok(build_skipped_result(
                "entry_time_budget_exhausted_after_attempt",
                &diagnostics,
                extras,
            ));
        }

        sleep_ms(config.order_retry_interval_ms).await;
    }

    if diagnostics.last_attempt_status.is_none() {
        diagnostics.last_attempt_status = Some("WINDOW_CLOSED_LOOP_EXIT".to_owned());
    }

    let mut extras = Map::new();
    extras.insert("exhaustedAtSec".to_owned(), json!(now_sec()));
    Ok(build_skipped_result(
        "window_closed_before_fill",
        &diagnostics,
        extras,
    ))
}

fn truncate_text(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }

    value.chars().take(max_chars).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rounds_tick_price_up() {
        let rounded = round_to_tick_ceil(0.5075, "0.01");
        assert!((rounded - 0.51).abs() < 0.000001);
    }

    #[test]
    fn computes_market_buy_spent_from_requested_stake() {
        let spent = compute_spent_usd(
            SpentComputationContext {
                mode: SpentComputationMode::MarketBuyRequestedStake,
                requested_stake_usd: Some(1.0),
            },
            "MATCHED",
            2.5,
            0.45,
        );

        assert!((spent.spent_usd - 1.0).abs() < 0.000001);
        assert_eq!(spent.spent_source, "requested_stake_market_buy");
    }

    #[test]
    fn computes_partial_market_buy_spent_capped() {
        let spent = compute_spent_usd(
            SpentComputationContext {
                mode: SpentComputationMode::MarketBuyRequestedStake,
                requested_stake_usd: Some(1.0),
            },
            "CANCELLED",
            0.5,
            0.7,
        );

        assert!((spent.spent_usd - 0.35).abs() < 0.000001);
        assert_eq!(spent.spent_source, "requested_stake_market_buy_capped_partial");
    }

    #[test]
    fn extracts_order_id_from_nested_data_payload() {
        let payload = json!({
            "success": true,
            "data": {
                "order": {
                    "orderId": "0xabc123"
                }
            }
        });

        assert_eq!(extract_order_id(&payload), "0xabc123");
    }

    #[test]
    fn extracts_order_id_from_plain_string_payload() {
        let payload = json!("0xdef456");
        assert_eq!(extract_order_id(&payload), "0xdef456");
    }

    #[test]
    fn extracts_status_record_from_wrapped_array_payload() {
        let payload = json!([
            {
                "data": {
                    "order": {
                        "status": "MATCHED",
                        "size_matched": "10",
                        "price": "0.42"
                    }
                }
            }
        ]);

        let record = extract_status_record(&payload).expect("status record should be detected");
        assert_eq!(record.get("status").and_then(Value::as_str), Some("MATCHED"));
        assert_eq!(record.get("size_matched").and_then(Value::as_str), Some("10"));
    }
}
