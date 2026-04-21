use std::collections::HashMap;

use anyhow::{Error, Result};
use serde_json::{json, Map, Value};

use crate::data::orderbook::fetch_orderbook_snapshot;
use crate::execution::client::{get_clob_client, ClobClient};
use crate::types::{Config, ExecutionResult, ExecutionStatus};
use crate::utils::logger::{log_error, log_info, log_warn};
use crate::utils::time::{now_sec, sleep_ms};

const STATUS_POLL_WARN_BURST: u64 = 3;
const STATUS_POLL_UNAVAILABLE_ABORT_COUNT: u64 = 8;

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
    Timeout(PollTimeoutDiagnostics),
}

#[derive(Debug, Clone, Copy)]
struct PollTimeoutDiagnostics {
    empty_status_count: u64,
    unsupported_payload_count: u64,
    aborted_due_to_unavailable: bool,
}

#[derive(Debug, Clone)]
struct ClassifiedSubmitError {
    message: String,
    code: String,
    retryable: bool,
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

fn apply_buy_slippage(base_price: f64, slippage_percent: f64, max_price: f64) -> f64 {
    const FIXED_MARKUP: f64 = 0.17;

    let percent_candidate = base_price * (1.0 + ((slippage_percent * 1.5) / 100.0));
    let fixed_candidate = base_price + FIXED_MARKUP;
    let slipped = percent_candidate.max(fixed_candidate);

    slipped.min(max_price)
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

fn compact_error_message(message: &str, max_chars: usize) -> String {
    let collapsed = message.split_whitespace().collect::<Vec<_>>().join(" ");

    truncate_text(&collapsed, max_chars)
}

fn classify_submit_error_message(message: &str) -> ClassifiedSubmitError {
    let compact = compact_error_message(message, 260);
    let lower = compact.to_lowercase();

    let retryable_hints = [
        "timeout",
        "timed out",
        "network",
        "socket",
        "econnreset",
        "enotfound",
        "eai_again",
        "temporarily unavailable",
        "service unavailable",
        "rate limit",
        "too many requests",
        "429",
    ];

    let non_retryable_hints = [
        "unauthorized",
        "forbidden",
        "signature",
        "invalid api",
        "api key",
        "passphrase",
        "insufficient",
        "not enough balance",
        "invalid order",
        "min size",
        "min_order_size",
    ];

    if retryable_hints.iter().any(|hint| lower.contains(hint)) {
        return ClassifiedSubmitError {
            message: compact,
            code: "RETRYABLE_NETWORK".to_owned(),
            retryable: true,
        };
    }

    if lower.contains("http 5") || lower.contains("status 5") || lower.contains("code=5") {
        return ClassifiedSubmitError {
            message: compact,
            code: "RETRYABLE_HTTP_5XX".to_owned(),
            retryable: true,
        };
    }

    if non_retryable_hints.iter().any(|hint| lower.contains(hint)) {
        return ClassifiedSubmitError {
            message: compact,
            code: "NON_RETRYABLE_REQUEST".to_owned(),
            retryable: false,
        };
    }

    if lower.contains("400") || lower.contains("401") || lower.contains("403") {
        return ClassifiedSubmitError {
            message: compact,
            code: "NON_RETRYABLE_HTTP_4XX".to_owned(),
            retryable: false,
        };
    }

    ClassifiedSubmitError {
        message: compact,
        code: "UNKNOWN_SUBMIT_ERROR".to_owned(),
        retryable: false,
    }
}

fn parse_balance_amount_from_rejection(message: &str) -> Option<f64> {
    let lower = message.to_lowercase();
    let marker = "balance:";
    let start = lower.find(marker)? + marker.len();
    let tail = &lower[start..];

    let digits: String = tail
        .chars()
        .skip_while(|ch| ch.is_whitespace())
        .take_while(|ch| ch.is_ascii_digit())
        .collect();

    if digits.is_empty() {
        return None;
    }

    let units = digits.parse::<f64>().ok()?;
    Some((units / 1_000_000.0).max(0.0))
}

fn floor_to_sell_size_step(value: f64) -> f64 {
    if !value.is_finite() || value <= 0.0 {
        return 0.0;
    }

    let step = 0.01_f64;
    let floored = (value / step).floor() * step;
    (floored * 100.0).round() / 100.0
}

fn apply_balance_limited_sell_size(
    current_size: f64,
    rejection_message: &str,
) -> Option<(f64, f64)> {
    if !rejection_message
        .to_lowercase()
        .contains("not enough balance")
    {
        return None;
    }

    let parsed_balance = parse_balance_amount_from_rejection(rejection_message)?;
    let safe_size = floor_to_sell_size_step(parsed_balance - 0.01);

    if safe_size <= 0.0 {
        return None;
    }
    if safe_size >= current_size {
        return None;
    }

    Some((parsed_balance, safe_size))
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
    raw.insert("fillBandViolationReason".to_owned(), json!(violation));
    raw.insert(
        "fillBandObservedPrice".to_owned(),
        json!(result.filled_price),
    );
    raw.insert("fillBandMin".to_owned(), json!(config.price_range_min));
    raw.insert("fillBandMax".to_owned(), json!(config.price_range_max));

    ExecutionResult {
        raw_response: raw,
        ..result
    }
}

async fn poll_final_order_status(
    config: &Config,
    client: &ClobClient,
    order_id: &str,
    poll_until_sec: u64,
    spent_context: SpentComputationContext,
    used_fallback_limit: bool,
) -> Result<PollStatusOutcome> {
    let mut empty_status_count = 0_u64;
    let mut unsupported_payload_count = 0_u64;

    while now_sec() <= poll_until_sec {
        let status = client.get_order(order_id).await?;
        let Some(status_value) = status else {
            empty_status_count += 1;
            if should_log_poll_warning(empty_status_count) {
                log_warn(
                    "Entry",
                    &format!(
                        "order={} get_order returned empty status (count={}); treat as transient and continue polling",
                        order_id, empty_status_count
                    ),
                );
            }

            if empty_status_count + unsupported_payload_count >= STATUS_POLL_UNAVAILABLE_ABORT_COUNT
            {
                log_warn(
                    "Entry",
                    &format!(
                        "order={} stop polling due to repeated unavailable status responses (empty={}, unsupported={}, abortAt={})",
                        order_id,
                        empty_status_count,
                        unsupported_payload_count,
                        STATUS_POLL_UNAVAILABLE_ABORT_COUNT,
                    ),
                );

                return Ok(PollStatusOutcome::Timeout(PollTimeoutDiagnostics {
                    empty_status_count,
                    unsupported_payload_count,
                    aborted_due_to_unavailable: true,
                }));
            }

            sleep_ms(config.status_poll_interval_ms).await;
            continue;
        };

        let Some(status_record) = extract_status_record(&status_value) else {
            let shape = value_shape(&status_value);
            unsupported_payload_count += 1;
            if should_log_poll_warning(unsupported_payload_count) {
                log_warn(
                    "Entry",
                    &format!(
                        "order={} get_order returned unsupported status payload shape={} payload={} (count={}); treat as transient and continue polling",
                        order_id,
                        shape,
                        truncate_text(&status_value.to_string(), 220),
                        unsupported_payload_count,
                    ),
                );
            }

            if empty_status_count + unsupported_payload_count >= STATUS_POLL_UNAVAILABLE_ABORT_COUNT
            {
                log_warn(
                    "Entry",
                    &format!(
                        "order={} stop polling due to repeated unavailable status responses (empty={}, unsupported={}, abortAt={})",
                        order_id,
                        empty_status_count,
                        unsupported_payload_count,
                        STATUS_POLL_UNAVAILABLE_ABORT_COUNT,
                    ),
                );

                return Ok(PollStatusOutcome::Timeout(PollTimeoutDiagnostics {
                    empty_status_count,
                    unsupported_payload_count,
                    aborted_due_to_unavailable: true,
                }));
            }

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

    Ok(PollStatusOutcome::Timeout(PollTimeoutDiagnostics {
        empty_status_count,
        unsupported_payload_count,
        aborted_due_to_unavailable: false,
    }))
}

fn string_field(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(|v| v.trim())
        .filter(|v| !v.is_empty())
        .map(ToOwned::to_owned)
}

fn bool_field(value: Option<&Value>) -> Option<bool> {
    match value {
        Some(Value::Bool(value)) => Some(*value),
        Some(Value::String(value)) => {
            let normalized = value.trim().to_lowercase();
            match normalized.as_str() {
                "true" | "1" => Some(true),
                "false" | "0" => Some(false),
                _ => None,
            }
        }
        _ => None,
    }
}

fn object_string_field(record: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(value) = string_field(record.get(*key)) {
            return Some(value);
        }
    }

    None
}

fn extract_submit_success(post_result: &Value) -> Option<bool> {
    let Some(record) = post_result.as_object() else {
        return None;
    };

    bool_field(record.get("success")).or_else(|| {
        record
            .get("data")
            .and_then(Value::as_object)
            .and_then(|data| bool_field(data.get("success")))
    })
}

fn extract_submit_status(post_result: &Value) -> Option<String> {
    let Some(record) = post_result.as_object() else {
        return None;
    };

    object_string_field(record, &["status"]).or_else(|| {
        record
            .get("data")
            .and_then(Value::as_object)
            .and_then(|data| object_string_field(data, &["status"]))
    })
}

fn extract_submit_error_message(post_result: &Value) -> Option<String> {
    let Some(record) = post_result.as_object() else {
        return None;
    };

    object_string_field(
        record,
        &[
            "errorMsg",
            "error_message",
            "errorMessage",
            "error",
            "message",
            "reason",
        ],
    )
    .or_else(|| {
        record
            .get("error")
            .and_then(Value::as_object)
            .and_then(|error_record| {
                object_string_field(error_record, &["message", "errorMsg", "reason", "code"])
            })
    })
    .or_else(|| {
        record
            .get("data")
            .and_then(Value::as_object)
            .and_then(|data| {
                object_string_field(
                    data,
                    &[
                        "errorMsg",
                        "error_message",
                        "errorMessage",
                        "error",
                        "message",
                        "reason",
                    ],
                )
            })
    })
}

fn looks_like_order_hash(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.len() != 66 || !trimmed.starts_with("0x") {
        return false;
    }

    trimmed.chars().skip(2).all(|ch| ch.is_ascii_hexdigit())
}

fn has_submit_shape(record: &Map<String, Value>) -> bool {
    record.contains_key("success")
        || record.contains_key("status")
        || record.contains_key("orderID")
        || record.contains_key("orderId")
        || record.contains_key("order_id")
        || record.contains_key("transactionsHashes")
        || record.contains_key("tradeIDs")
}

fn infer_submit_error_code(error_message: &str) -> Option<String> {
    let upper = error_message.to_uppercase();
    const KNOWN_CODES: [&str; 12] = [
        "INVALID_ORDER_MIN_SIZE",
        "INVALID_ORDER_MIN_TICK_SIZE",
        "INVALID_ORDER_DUPLICATED",
        "INVALID_ORDER_NOT_ENOUGH_BALANCE",
        "INVALID_ORDER_EXPIRATION",
        "INVALID_ORDER_ERROR",
        "INVALID_POST_ONLY_ORDER_TYPE",
        "INVALID_POST_ONLY_ORDER",
        "FOK_ORDER_NOT_FILLED_ERROR",
        "EXECUTION_ERROR",
        "ORDER_DELAYED",
        "MARKET_NOT_READY",
    ];

    for code in KNOWN_CODES {
        if upper.contains(code) {
            return Some(code.to_owned());
        }
    }

    None
}

fn is_non_retryable_submit_error(error_code: Option<&str>) -> bool {
    matches!(
        error_code,
        Some(
            "INVALID_ORDER_MIN_SIZE"
                | "INVALID_ORDER_MIN_TICK_SIZE"
                | "INVALID_ORDER_NOT_ENOUGH_BALANCE"
                | "INVALID_ORDER_EXPIRATION"
                | "INVALID_POST_ONLY_ORDER_TYPE"
                | "INVALID_POST_ONLY_ORDER"
        )
    )
}

fn should_log_poll_warning(count: u64) -> bool {
    count <= STATUS_POLL_WARN_BURST || count % 5 == 0
}

fn extract_order_id_from_object(record: &Map<String, Value>) -> Option<String> {
    string_field(record.get("orderID"))
        .or_else(|| string_field(record.get("orderId")))
        .or_else(|| string_field(record.get("order_id")))
        .or_else(|| {
            if !has_submit_shape(record) {
                return None;
            }

            string_field(record.get("id")).filter(|value| looks_like_order_hash(value))
        })
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

    let submit_success = extract_submit_success(&post_result);
    let submit_status = extract_submit_status(&post_result);
    let submit_error_message = extract_submit_error_message(&post_result);
    let submit_error_code = submit_error_message
        .as_deref()
        .and_then(infer_submit_error_code);
    let submit_rejected = submit_success == Some(false) || submit_error_message.is_some();

    if submit_rejected {
        let order_id = extract_order_id(&post_result);
        let non_retryable = is_non_retryable_submit_error(submit_error_code.as_deref());

        log_warn(
            "Entry",
            &format!(
                "FOK submit rejected order={} success={:?} status={} code={} message={}",
                if order_id.is_empty() {
                    "<empty>"
                } else {
                    order_id.as_str()
                },
                submit_success,
                submit_status.as_deref().unwrap_or("<none>"),
                submit_error_code.as_deref().unwrap_or("<none>"),
                submit_error_message.as_deref().unwrap_or("<none>"),
            ),
        );

        let mut raw = to_raw_map(post_result);
        raw.insert("reason".to_owned(), json!("order_submit_rejected"));
        raw.insert(
            "submitSuccess".to_owned(),
            submit_success
                .map(|value| json!(value))
                .unwrap_or(Value::Null),
        );
        raw.insert(
            "submitStatus".to_owned(),
            submit_status
                .as_ref()
                .map(|value| json!(value))
                .unwrap_or(Value::Null),
        );
        raw.insert(
            "submitErrorMsg".to_owned(),
            submit_error_message
                .as_ref()
                .map(|value| json!(value))
                .unwrap_or(Value::Null),
        );
        raw.insert(
            "submitErrorCode".to_owned(),
            submit_error_code
                .as_ref()
                .map(|value| json!(value))
                .unwrap_or(Value::Null),
        );
        raw.insert("submitTerminalNoRetry".to_owned(), json!(non_retryable));

        return Ok(Some(ExecutionResult {
            status: ExecutionStatus::Failed,
            order_id,
            filled_price: params.final_price,
            filled_size: 0.0,
            spent_usd: 0.0,
            used_fallback_limit: false,
            raw_response: raw,
        }));
    }

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

    let poll_until_sec =
        status_poll_deadline_sec(params.close_time_sec, params.config.status_poll_grace_sec);

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
        PollStatusOutcome::Timeout(diagnostics) => {
            let mut raw = to_raw_map(post_result);
            raw.insert(
                "reason".to_owned(),
                json!(if diagnostics.aborted_due_to_unavailable {
                    "order_status_unavailable"
                } else {
                    "order_status_poll_timeout"
                }),
            );
            raw.insert(
                "statusPollIntervalMs".to_owned(),
                json!(params.config.status_poll_interval_ms),
            );
            raw.insert(
                "statusPollGraceSec".to_owned(),
                json!(params.config.status_poll_grace_sec),
            );
            raw.insert(
                "statusPollEmptyCount".to_owned(),
                json!(diagnostics.empty_status_count),
            );
            raw.insert(
                "statusPollUnsupportedCount".to_owned(),
                json!(diagnostics.unsupported_payload_count),
            );
            raw.insert(
                "statusPollAbortedUnavailable".to_owned(),
                json!(diagnostics.aborted_due_to_unavailable),
            );
            raw.insert(
                "statusPollAbortCount".to_owned(),
                json!(STATUS_POLL_UNAVAILABLE_ABORT_COUNT),
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

    let poll_until_sec =
        status_poll_deadline_sec(params.close_time_sec, params.config.status_poll_grace_sec);

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
        PollStatusOutcome::Timeout(diagnostics) => {
            let mut raw = to_raw_map(post_result);
            raw.insert(
                "reason".to_owned(),
                json!(if diagnostics.aborted_due_to_unavailable {
                    "order_status_unavailable"
                } else {
                    "order_status_poll_timeout"
                }),
            );
            raw.insert(
                "statusPollIntervalMs".to_owned(),
                json!(params.config.status_poll_interval_ms),
            );
            raw.insert(
                "statusPollGraceSec".to_owned(),
                json!(params.config.status_poll_grace_sec),
            );
            raw.insert(
                "statusPollEmptyCount".to_owned(),
                json!(diagnostics.empty_status_count),
            );
            raw.insert(
                "statusPollUnsupportedCount".to_owned(),
                json!(diagnostics.unsupported_payload_count),
            );
            raw.insert(
                "statusPollAbortedUnavailable".to_owned(),
                json!(diagnostics.aborted_due_to_unavailable),
            );
            raw.insert(
                "statusPollAbortCount".to_owned(),
                json!(STATUS_POLL_UNAVAILABLE_ABORT_COUNT),
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

fn round_size_to_6_decimals(value: f64) -> f64 {
    if !value.is_finite() {
        return 0.0;
    }

    let factor = 1_000_000.0;
    (value * factor).round() / factor
}

fn build_sell_submit_retry_payload(
    interval_ms: u64,
    max_retries: u64,
    attempts_used: u64,
    exhausted: bool,
) -> Value {
    let mut payload = Map::new();
    payload.insert("intervalMs".to_owned(), json!(interval_ms));
    payload.insert("maxRetries".to_owned(), json!(max_retries));
    payload.insert("attemptsUsed".to_owned(), json!(attempts_used));
    payload.insert("exhausted".to_owned(), json!(exhausted));

    Value::Object(payload)
}

fn build_post_fill_sell_limit_payload(
    token_id: &str,
    requested_price: f64,
    size: f64,
) -> Map<String, Value> {
    let mut payload = Map::new();
    payload.insert("enabled".to_owned(), json!(true));
    payload.insert("attempted".to_owned(), json!(false));
    payload.insert("orderType".to_owned(), json!("GTC"));
    payload.insert("tokenId".to_owned(), json!(token_id));
    payload.insert("requestedPrice".to_owned(), json!(requested_price));
    payload.insert("finalPrice".to_owned(), Value::Null);
    payload.insert("tickSize".to_owned(), Value::Null);
    payload.insert("size".to_owned(), json!(size));
    payload.insert("success".to_owned(), json!(false));
    payload.insert("orderId".to_owned(), Value::Null);
    payload.insert("status".to_owned(), Value::Null);
    payload.insert("errorMsg".to_owned(), Value::Null);
    payload.insert("errorCode".to_owned(), Value::Null);
    payload.insert("retryable".to_owned(), Value::Null);
    payload.insert("errorPhase".to_owned(), Value::Null);

    payload
}

async fn submit_post_fill_sell_limit(
    config: &Config,
    client: &ClobClient,
    token_id: &str,
    size: f64,
) -> Map<String, Value> {
    let requested_price = config.post_fill_sell_limit_price;
    let normalized_size = round_size_to_6_decimals(size);
    let mut payload =
        build_post_fill_sell_limit_payload(token_id, requested_price, normalized_size);

    if normalized_size <= 0.0 {
        payload.insert("status".to_owned(), json!("skipped_no_filled_size"));
        payload.insert(
            "errorMsg".to_owned(),
            json!("filled size must be > 0 to place post-fill sell limit"),
        );
        payload.insert("errorCode".to_owned(), json!("SELL_SIZE_INVALID"));
        payload.insert("retryable".to_owned(), json!(false));
        return payload;
    }

    let retry_interval_ms = config.post_fill_sell_retry_interval_ms.max(1);
    let max_retries = config.post_fill_sell_max_retries.max(1);

    let tick_size = match client.get_tick_size(token_id).await {
        Ok(value) => value,
        Err(error) => {
            let message = compact_error_message(&format_error_chain(&error), 180);
            log_warn(
                "Exit",
                &format!(
                    "post-fill SELL tick size fetch failed, fallback to 0.01: {}",
                    message
                ),
            );
            "0.01".to_owned()
        }
    };

    payload.insert("tickSize".to_owned(), json!(tick_size.clone()));

    let final_price = clamp_price(round_to_tick_ceil(clamp_price(requested_price), &tick_size));
    payload.insert("finalPrice".to_owned(), json!(final_price));

    let mut current_size = normalized_size;
    payload.insert("sizeAdjustedForBalance".to_owned(), json!(false));

    let mut attempts_used = 0_u64;
    let mut last_status = "order_submit_failed".to_owned();
    let mut last_error_message: Option<String> = None;
    let mut last_error_code: Option<String> = None;
    let mut last_retryable = true;
    let mut last_error_phase = "order_submission".to_owned();
    let mut last_post_result: Option<Value> = None;

    for attempt_idx in 1..=max_retries {
        attempts_used = attempt_idx;

        if attempt_idx > 1 {
            sleep_ms(retry_interval_ms).await;
        }

        let signed_order = match client
            .create_limit_order_sell(token_id, current_size, final_price, &tick_size)
            .await
        {
            Ok(value) => value,
            Err(error) => {
                let classified = classify_submit_error_message(&format_error_chain(&error));
                last_status = "order_creation_failed".to_owned();
                last_error_message = Some(classified.message.clone());
                last_error_code = Some(classified.code.clone());
                last_retryable = classified.retryable;
                last_error_phase = "order_creation".to_owned();

                log_warn(
                    "Exit",
                    &format!(
                        "post-fill SELL submit retry {}/{} failed phase=order_creation code={} msg={}",
                        attempt_idx, max_retries, classified.code, classified.message
                    ),
                );

                continue;
            }
        };

        let post_result = match client
            .post_order(&signed_order, "GTC", false, Some(false))
            .await
        {
            Ok(value) => value,
            Err(error) => {
                let classified = classify_submit_error_message(&format_error_chain(&error));
                last_status = "order_submit_failed".to_owned();
                last_error_message = Some(classified.message.clone());
                last_error_code = Some(classified.code.clone());
                last_retryable = classified.retryable;
                last_error_phase = "order_submission".to_owned();

                log_warn(
                    "Exit",
                    &format!(
                        "post-fill SELL submit retry {}/{} failed phase=order_submission code={} msg={}",
                        attempt_idx, max_retries, classified.code, classified.message
                    ),
                );

                continue;
            }
        };

        let submit_success = extract_submit_success(&post_result);
        let submit_status = extract_submit_status(&post_result);
        let submit_error_message = extract_submit_error_message(&post_result);
        let submit_error_code = submit_error_message
            .as_deref()
            .and_then(infer_submit_error_code);
        let submit_rejected = submit_success == Some(false) || submit_error_message.is_some();

        if submit_rejected {
            let fallback_classified = classify_submit_error_message(
                submit_error_message
                    .as_deref()
                    .unwrap_or("order submission rejected"),
            );

            let error_code = submit_error_code
                .clone()
                .unwrap_or_else(|| fallback_classified.code.clone());

            let retryable = if submit_error_code.is_some() {
                !is_non_retryable_submit_error(Some(error_code.as_str()))
            } else {
                fallback_classified.retryable
            };

            let status_text = submit_status
                .as_deref()
                .unwrap_or("order_submit_rejected")
                .to_owned();
            let message_text = submit_error_message.unwrap_or(fallback_classified.message);

            if let Some((parsed_balance, reduced_size)) =
                apply_balance_limited_sell_size(current_size, &message_text)
            {
                current_size = reduced_size;
                payload.insert("sizeAdjustedForBalance".to_owned(), json!(true));
                payload.insert("adjustedSize".to_owned(), json!(current_size));
                payload.insert("balanceAtReject".to_owned(), json!(parsed_balance));

                log_warn(
                    "Exit",
                    &format!(
                        "post-fill SELL submit retry {}/{} balance-limited resize parsedBalance={:.6} nextSize={:.2}",
                        attempt_idx, max_retries, parsed_balance, current_size
                    ),
                );
            }

            last_status = status_text.clone();
            last_error_message = Some(message_text.clone());
            last_error_code = Some(error_code.clone());
            last_retryable = retryable;
            last_error_phase = "order_submission".to_owned();
            last_post_result = Some(post_result);

            log_warn(
                "Exit",
                &format!(
                    "post-fill SELL submit retry {}/{} rejected status={} code={} retryable={} msg={}",
                    attempt_idx, max_retries, status_text, error_code, retryable, message_text
                ),
            );

            continue;
        }

        let order_id = extract_order_id(&post_result);
        if order_id.is_empty() {
            let status_text = submit_status
                .as_deref()
                .unwrap_or("order_submit_missing_order_id")
                .to_owned();
            let message_text = submit_error_message
                .unwrap_or_else(|| "postOrder response does not contain orderID/id".to_owned());

            last_status = status_text.clone();
            last_error_message = Some(message_text.clone());
            last_error_code = Some("ORDER_ID_MISSING".to_owned());
            last_retryable = true;
            last_error_phase = "response_parse".to_owned();
            last_post_result = Some(post_result);

            log_warn(
                "Exit",
                &format!(
                    "post-fill SELL submit retry {}/{} failed phase=response_parse status={} code=ORDER_ID_MISSING msg={}",
                    attempt_idx, max_retries, status_text, message_text
                ),
            );

            continue;
        }

        payload.insert("attempted".to_owned(), json!(true));
        payload.insert("success".to_owned(), json!(true));
        payload.insert("orderId".to_owned(), json!(order_id));
        payload.insert("size".to_owned(), json!(current_size));
        payload.insert(
            "status".to_owned(),
            json!(submit_status.unwrap_or_else(|| "submitted".to_owned())),
        );
        payload.insert("postResult".to_owned(), post_result);
        payload.insert("submitAttempt".to_owned(), json!(attempts_used));
        payload.insert(
            "submitRetry".to_owned(),
            build_sell_submit_retry_payload(retry_interval_ms, max_retries, attempts_used, false),
        );

        return payload;
    }

    payload.insert("attempted".to_owned(), json!(attempts_used > 0));
    payload.insert(
        "status".to_owned(),
        json!(format!("{}_retries_exhausted", last_status)),
    );
    payload.insert(
        "errorMsg".to_owned(),
        json!(last_error_message.unwrap_or_else(|| {
            format!(
                "post-fill SELL retries exhausted after {} attempts",
                attempts_used
            )
        })),
    );
    payload.insert(
        "errorCode".to_owned(),
        json!(last_error_code.unwrap_or_else(|| "SELL_RETRY_EXHAUSTED".to_owned())),
    );
    payload.insert("retryable".to_owned(), json!(last_retryable));
    payload.insert("errorPhase".to_owned(), json!(last_error_phase));
    payload.insert("submitAttempt".to_owned(), json!(attempts_used));
    payload.insert(
        "submitRetry".to_owned(),
        build_sell_submit_retry_payload(retry_interval_ms, max_retries, attempts_used, true),
    );

    if let Some(post_result) = last_post_result {
        payload.insert("postResult".to_owned(), post_result);
    }

    payload
}

async fn attach_post_fill_sell_limit(
    config: &Config,
    client: &ClobClient,
    token_id: &str,
    result: ExecutionResult,
    attempt: u64,
    close_time_sec: u64,
) -> ExecutionResult {
    if !config.enable_post_fill_sell_limit {
        return result;
    }

    let trigger_before_close_sec = config.post_fill_sell_trigger_before_close_sec;
    if trigger_before_close_sec > 0 {
        let trigger_at_sec = close_time_sec.saturating_sub(trigger_before_close_sec);
        let now = now_sec();
        if now < trigger_at_sec {
            let wait_sec = trigger_at_sec.saturating_sub(now);
            log_info(
                "Exit",
                &format!(
                    "attempt #{} post-fill SELL scheduled at t-{}s; waiting {}s",
                    attempt, trigger_before_close_sec, wait_sec
                ),
            );
            sleep_ms(wait_sec.saturating_mul(1_000)).await;
        }
    }

    let placement = submit_post_fill_sell_limit(config, client, token_id, result.filled_size).await;

    let success = placement
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let attempted = placement
        .get("attempted")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    if success {
        let order_id = placement
            .get("orderId")
            .and_then(Value::as_str)
            .unwrap_or("");
        let size = placement.get("size").and_then(Value::as_f64).unwrap_or(0.0);
        let price = placement
            .get("finalPrice")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let status = placement
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("submitted");

        log_info(
            "Exit",
            &format!(
                "attempt #{} post-fill SELL accepted order={} size={:.6} price={:.3} status={}",
                attempt, order_id, size, price, status
            ),
        );
    } else if attempted {
        let phase = placement
            .get("errorPhase")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let code = placement
            .get("errorCode")
            .and_then(Value::as_str)
            .unwrap_or("UNKNOWN");
        let status = placement
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let message = placement
            .get("errorMsg")
            .and_then(Value::as_str)
            .unwrap_or("unknown");

        log_warn(
            "Exit",
            &format!(
                "attempt #{} post-fill SELL failed phase={} code={} status={} msg={}",
                attempt, phase, code, status, message
            ),
        );
    } else {
        let message = placement
            .get("errorMsg")
            .and_then(Value::as_str)
            .unwrap_or("unknown");

        log_warn(
            "Exit",
            &format!("attempt #{} post-fill SELL skipped: {}", attempt, message),
        );
    }

    let mut raw_response = result.raw_response.clone();
    raw_response.insert("postFillSellLimit".to_owned(), Value::Object(placement));

    ExecutionResult {
        raw_response,
        ..result
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
    extras.insert(
        "retryIntervalMs".to_owned(),
        json!(config.order_retry_interval_ms),
    );

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
            extras.insert(
                "timeToCloseSec".to_owned(),
                json!(time_to_close_before_attempt),
            );

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
                    diagnostics.last_attempt_status =
                        Some("ORDERBOOK_ERROR_UNTIL_CLOSE".to_owned());

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

            let max_buy_price = if config.entry_price_gate_enabled {
                config.price_range_max
            } else {
                0.98
            };
            let slipped = apply_buy_slippage(
                orderbook.best_ask,
                config.entry_slippage_percent_buy,
                max_buy_price,
            );
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
                diagnostics.last_attempt_status =
                    Some(format!("FOK_{}", status_label(value.status)));
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
                let should_stop = value
                    .raw_response
                    .get("submitTerminalNoRetry")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);

                if value.status == ExecutionStatus::Filled
                    || value.status == ExecutionStatus::Partial
                {
                    return Ok(attach_post_fill_sell_limit(
                        config,
                        &client,
                        token_id,
                        value,
                        attempt,
                        close_time_sec,
                    )
                    .await);
                }

                if should_stop {
                    diagnostics.last_attempt_status = Some("FOK_TERMINAL_NO_RETRY".to_owned());
                    log_warn(
                        "Entry",
                        &format!(
                            "attempt #{} stop retries due to terminal submit error reason={} code={}",
                            attempt,
                            value
                                .raw_response
                                .get("reason")
                                .and_then(Value::as_str)
                                .unwrap_or("<none>"),
                            value
                                .raw_response
                                .get("submitErrorCode")
                                .and_then(Value::as_str)
                                .unwrap_or("<none>"),
                        ),
                    );
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
                diagnostics.last_attempt_status =
                    Some(format!("GTC_{}", status_label(value.status)));
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
                if value.status == ExecutionStatus::Filled
                    || value.status == ExecutionStatus::Partial
                {
                    return Ok(attach_post_fill_sell_limit(
                        config,
                        &client,
                        token_id,
                        value,
                        attempt,
                        close_time_sec,
                    )
                    .await);
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
    fn applies_aggressive_buy_slippage_with_fixed_markup_floor() {
        let slipped = apply_buy_slippage(0.70, 2.5, 0.95);
        assert!((slipped - 0.87).abs() < 0.000001);
    }

    #[test]
    fn applies_aggressive_buy_slippage_with_cap() {
        let slipped = apply_buy_slippage(0.90, 14.0, 0.95);
        assert!((slipped - 0.95).abs() < 0.000001);
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
        assert_eq!(
            spent.spent_source,
            "requested_stake_market_buy_capped_partial"
        );
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
        assert_eq!(
            record.get("status").and_then(Value::as_str),
            Some("MATCHED")
        );
        assert_eq!(
            record.get("size_matched").and_then(Value::as_str),
            Some("10")
        );
    }

    #[test]
    fn detects_submit_error_and_code() {
        let payload = json!({
            "success": false,
            "errorMsg": "INVALID_ORDER_MIN_SIZE: order too small"
        });

        assert_eq!(extract_submit_success(&payload), Some(false));
        assert_eq!(
            extract_submit_error_message(&payload),
            Some("INVALID_ORDER_MIN_SIZE: order too small".to_owned())
        );

        let error_code = extract_submit_error_message(&payload)
            .as_deref()
            .and_then(infer_submit_error_code);
        assert_eq!(error_code.as_deref(), Some("INVALID_ORDER_MIN_SIZE"));
        assert!(is_non_retryable_submit_error(error_code.as_deref()));
    }

    #[test]
    fn keeps_non_retryable_code_for_balance_digits_without_http_status() {
        let message = "not enough balance / allowance. balance: 1048840, order amount: 1050000";
        let classified = classify_submit_error_message(message);

        assert_eq!(classified.code, "NON_RETRYABLE_REQUEST");
        assert!(!classified.retryable);
    }

    #[test]
    fn shrinks_post_fill_sell_size_from_balance_rejection_message() {
        let message = "not enough balance / allowance. balance: 1048840, order amount: 1050000";
        let adjusted = apply_balance_limited_sell_size(1.05, message)
            .expect("balance-limited adjustment should produce reduced size");

        assert!((adjusted.0 - 1.04884).abs() < 0.000001);
        assert!((adjusted.1 - 1.03).abs() < 0.000001);
    }

    #[test]
    fn ignores_non_hash_generic_id_for_submit_payload() {
        let payload = json!({
            "success": false,
            "id": "12345",
            "errorMsg": "INVALID_ORDER_MIN_SIZE"
        });

        assert_eq!(extract_order_id(&payload), "");
    }
}
