use std::collections::{HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde_json::Value;
use uuid::Uuid;

use crate::data::market_discovery::fetch_market_by_slug;
use crate::data::orderbook::fetch_orderbook_snapshot;
use crate::execution::order_executor::execute_live_entry;
use crate::notifications::telegram_notifier::{create_telegram_notifier, TelegramNotifier};
use crate::settlement::result_resolver::resolve_window_outcome;
use crate::settlement::settlement_service::{compute_trade_pnl, process_pending_claim};
use crate::storage::trade_logger::{
    log_trade_record, read_trade_records, update_trade_claim_status, ClaimStatusUpdate,
};
use crate::types::{
    ClaimStatus, Config, DiscoveredMarket, ExecutionStatus, MarketSide, PendingClaim, TradeRecord,
};
use crate::utils::logger::{log_cycle_separator, log_info, log_warn};
use crate::utils::time::{build_window, now_sec, sleep_ms, sleep_until};

#[derive(Debug, Clone, Copy)]
pub enum RuntimeStage {
    Boot,
    Idle,
    Cycle,
    WaitT10,
    LookupMarket,
    WaitResolve,
    Resolving,
    ClaimSweep,
}

#[derive(Debug, Clone)]
struct SideCandidate {
    side: MarketSide,
    token_id: String,
    ask_price: f64,
    bid_price: f64,
}

#[derive(Debug, Clone)]
struct PostFillSellLimitOutcome {
    attempted: bool,
    success: bool,
    order_type: String,
    size: f64,
    final_price: Option<f64>,
    status: Option<String>,
    order_id: Option<String>,
    retryable: Option<bool>,
    error_code: Option<String>,
    error_phase: Option<String>,
    error_msg: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CycleOutcome {
    None,
    Win,
    Loss,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn trade_id(window_start_sec: u64) -> String {
    format!("v2_{}_{}", window_start_sec, Uuid::new_v4().simple())
}

fn clip_text(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }

    value.chars().take(max_chars).collect()
}

fn execution_status_label(status: ExecutionStatus) -> &'static str {
    match status {
        ExecutionStatus::Pending => "PENDING",
        ExecutionStatus::Filled => "FILLED",
        ExecutionStatus::Partial => "PARTIAL",
        ExecutionStatus::Cancelled => "CANCELLED",
        ExecutionStatus::Failed => "FAILED",
        ExecutionStatus::Skipped => "SKIPPED",
    }
}

fn trade_result_label(won: bool) -> &'static str {
    if won {
        "WIN"
    } else {
        "LOSS"
    }
}

fn parse_value_f64(value: Option<&Value>) -> Option<f64> {
    match value {
        Some(Value::Number(number)) => number.as_f64(),
        Some(Value::String(text)) => text.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn parse_post_fill_sell_limit_outcome(
    raw_response: &HashMap<String, Value>,
) -> Option<PostFillSellLimitOutcome> {
    let placement = raw_response.get("postFillSellLimit")?.as_object()?;
    if placement.get("enabled").and_then(Value::as_bool) != Some(true) {
        return None;
    }

    let attempted = placement
        .get("attempted")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let success = placement
        .get("success")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    Some(PostFillSellLimitOutcome {
        attempted,
        success,
        order_type: placement
            .get("orderType")
            .and_then(Value::as_str)
            .unwrap_or("GTC")
            .to_owned(),
        size: parse_value_f64(placement.get("size")).unwrap_or(0.0),
        final_price: parse_value_f64(placement.get("finalPrice")),
        status: placement
            .get("status")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        order_id: placement
            .get("orderId")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        retryable: placement.get("retryable").and_then(Value::as_bool),
        error_code: placement
            .get("errorCode")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        error_phase: placement
            .get("errorPhase")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        error_msg: placement
            .get("errorMsg")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
    })
}

fn should_manage_claims(config: &Config) -> bool {
    config.enable_live_trading && config.on_chain_auto_claim_enabled
}

fn claim_status_from_record(record: &TradeRecord) -> ClaimStatus {
    if let Some(status) = record.claim_status {
        return status;
    }

    if record.claim_tx_hash.is_some() {
        ClaimStatus::Success
    } else {
        ClaimStatus::Pending
    }
}

fn to_pending_claim(record: &TradeRecord) -> Option<PendingClaim> {
    if record.market.condition_id.trim().is_empty() {
        return None;
    }

    if record.market.yes_token_id.trim().is_empty() || record.market.no_token_id.trim().is_empty() {
        return None;
    }

    if matches!(
        claim_status_from_record(record),
        ClaimStatus::Success | ClaimStatus::Failed
    ) {
        return None;
    }

    Some(PendingClaim {
        trade_id: record.id.clone(),
        window_slug: record.window.slug.clone(),
        condition_id: record.market.condition_id.clone(),
        yes_token_id: record.market.yes_token_id.clone(),
        no_token_id: record.market.no_token_id.clone(),
        created_at_ms: record.timestamp_ms,
        poll_count: 0,
        claim_attempts: record.claim_attempts.unwrap_or(0),
        last_update_ms: now_ms(),
        last_error: record.claim_last_error.clone(),
    })
}

async fn restore_pending_claims_from_history(
    config: &Config,
    pending_claims: &mut HashMap<String, PendingClaim>,
) -> Result<()> {
    let records = read_trade_records(&config.trades_output_path).await?;
    let mut restored = 0_u64;

    for record in records {
        let Some(pending) = to_pending_claim(&record) else {
            continue;
        };

        if pending_claims.contains_key(&pending.trade_id) {
            continue;
        }

        pending_claims.insert(pending.trade_id.clone(), pending);
        restored += 1;
    }

    if restored > 0 {
        log_info(
            "Settlement",
            &format!("restored {restored} pending claim(s) from history"),
        );
    }

    Ok(())
}

async fn sweep_pending_claims(
    config: &Config,
    pending_claims: &mut HashMap<String, PendingClaim>,
    telegram: &TelegramNotifier,
) -> Result<()> {
    if pending_claims.is_empty() {
        return Ok(());
    }

    let entries: Vec<(String, PendingClaim)> = pending_claims
        .iter()
        .map(|(trade_id, pending)| (trade_id.clone(), pending.clone()))
        .collect();
    let checked_total = entries.len();

    let mut settled = 0_u64;

    for (trade_id, pending) in entries {
        let next_poll_count = pending.poll_count + 1;

        match process_pending_claim(
            config,
            &trade_id,
            &pending.window_slug,
            &pending.condition_id,
        )
        .await
        {
            Ok(claim_result) => {
                if claim_result.completed {
                    let next_claim_attempts = pending.claim_attempts + 1;
                    pending_claims.remove(&trade_id);
                    settled += 1;
                    let tx_label = claim_result.tx_hash.as_deref().unwrap_or("n/a");

                    let updated_at_ms = now_ms();
                    let _ = update_trade_claim_status(
                        &config.trades_output_path,
                        &trade_id,
                        ClaimStatusUpdate {
                            claim_status: Some(ClaimStatus::Success),
                            claim_attempts: Some(next_claim_attempts),
                            claim_tx_hash: claim_result.tx_hash.clone(),
                            claim_last_error: Some(String::new()),
                            claim_updated_at_ms: Some(updated_at_ms),
                            market_resolved: Some(true),
                            market_resolution_source: claim_result.resolution_source,
                            market_resolved_at_ms: claim_result.market_resolved_at_ms,
                        },
                    )
                    .await;

                    log_info(
                        "Settlement",
                        &format!("claim completed trade={} tx={}", trade_id, tx_label),
                    );

                    if telegram.enabled {
                        let _ = telegram
                            .send(&format!(
                                "[POLYMARKET BOT CLAIM OK]\ntrade : {}\nwindow: {}\ntx    : {}",
                                trade_id, pending.window_slug, tx_label
                            ))
                            .await;
                    }

                    continue;
                }

                let next_claim_attempts = if claim_result.market_resolved {
                    pending.claim_attempts + 1
                } else {
                    pending.claim_attempts
                };

                let reached_failure_threshold = claim_result.market_resolved
                    && next_claim_attempts >= config.settlement_max_attempts;

                let next_status = if reached_failure_threshold {
                    ClaimStatus::Failed
                } else {
                    ClaimStatus::Pending
                };

                let claim_error = claim_result.error.clone();
                if reached_failure_threshold {
                    pending_claims.remove(&trade_id);
                } else {
                    pending_claims.insert(
                        trade_id.clone(),
                        PendingClaim {
                            trade_id: trade_id.clone(),
                            window_slug: pending.window_slug.clone(),
                            condition_id: pending.condition_id.clone(),
                            yes_token_id: pending.yes_token_id.clone(),
                            no_token_id: pending.no_token_id.clone(),
                            created_at_ms: pending.created_at_ms,
                            poll_count: next_poll_count,
                            claim_attempts: next_claim_attempts,
                            last_update_ms: now_ms(),
                            last_error: claim_error.clone(),
                        },
                    );
                }

                let _ = update_trade_claim_status(
                    &config.trades_output_path,
                    &trade_id,
                    ClaimStatusUpdate {
                        claim_status: Some(next_status),
                        claim_attempts: Some(next_claim_attempts),
                        claim_tx_hash: None,
                        claim_last_error: claim_error.clone(),
                        claim_updated_at_ms: Some(now_ms()),
                        market_resolved: Some(claim_result.market_resolved),
                        market_resolution_source: claim_result.resolution_source,
                        market_resolved_at_ms: claim_result.market_resolved_at_ms,
                    },
                )
                .await;

                if !claim_result.market_resolved {
                    log_info(
                        "Settlement",
                        &format!(
                            "claim pending trade={} unresolved poll={} source={} reason={}",
                            trade_id,
                            next_poll_count,
                            match claim_result.resolution_source {
                                Some(value) => format!("{:?}", value),
                                None => "none".to_owned(),
                            },
                            clip_text(claim_error.as_deref().unwrap_or("unknown"), 220)
                        ),
                    );
                } else {
                    log_warn(
                        "Settlement",
                        &format!(
                            "claim attempt failed trade={} attempt={} reason={}",
                            trade_id,
                            next_claim_attempts,
                            claim_error.as_deref().unwrap_or("unknown")
                        ),
                    );
                }

                if reached_failure_threshold
                    && pending.claim_attempts < config.settlement_max_attempts
                {
                    if telegram.enabled {
                        let _ = telegram
                            .send(&format!(
                                "[POLYMARKET BOT CLAIM FAIL]\ntrade : {}\nwindow: {}\nreason: {}",
                                trade_id,
                                pending.window_slug,
                                clip_text(claim_error.as_deref().unwrap_or("unknown"), 220)
                            ))
                            .await;
                    }
                }
            }
            Err(error) => {
                let message = error.to_string();
                let next_claim_attempts = pending.claim_attempts + 1;
                let next_status = if next_claim_attempts >= config.settlement_max_attempts {
                    ClaimStatus::Failed
                } else {
                    ClaimStatus::Pending
                };

                if matches!(next_status, ClaimStatus::Failed) {
                    pending_claims.remove(&trade_id);
                } else {
                    pending_claims.insert(
                        trade_id.clone(),
                        PendingClaim {
                            trade_id: trade_id.clone(),
                            window_slug: pending.window_slug.clone(),
                            condition_id: pending.condition_id.clone(),
                            yes_token_id: pending.yes_token_id.clone(),
                            no_token_id: pending.no_token_id.clone(),
                            created_at_ms: pending.created_at_ms,
                            poll_count: next_poll_count,
                            claim_attempts: next_claim_attempts,
                            last_update_ms: now_ms(),
                            last_error: Some(message.clone()),
                        },
                    );
                }

                let _ = update_trade_claim_status(
                    &config.trades_output_path,
                    &trade_id,
                    ClaimStatusUpdate {
                        claim_status: Some(next_status),
                        claim_attempts: Some(next_claim_attempts),
                        claim_tx_hash: None,
                        claim_last_error: Some(message.clone()),
                        claim_updated_at_ms: Some(now_ms()),
                        market_resolved: None,
                        market_resolution_source: None,
                        market_resolved_at_ms: None,
                    },
                )
                .await;

                log_warn(
                    "Settlement",
                    &format!(
                        "claim sweep error trade={} attempt={} error={}",
                        trade_id, next_claim_attempts, message
                    ),
                );
            }
        }
    }

    log_info(
        "Settlement",
        &format!(
            "claim sweep checked={} settled={} pending={}",
            checked_total,
            settled,
            pending_claims.len()
        ),
    );

    Ok(())
}

pub async fn run_orchestrator(config: Config) -> Result<()> {
    let mut processed_windows: HashSet<u64> = HashSet::new();
    let mut pending_claims: HashMap<String, PendingClaim> = HashMap::new();
    let telegram = create_telegram_notifier(&config);
    let mut pause_notice_sent = false;
    let mut losses_since_resume = 0_u64;
    let mut paused_by_loss_limit = false;
    let mut loss_cooldown_until_sec: Option<u64> = None;

    if should_manage_claims(&config) {
        if let Err(error) = restore_pending_claims_from_history(&config, &mut pending_claims).await
        {
            log_warn(
                "Settlement",
                &format!("failed to restore pending claims: {error}"),
            );
        }
    }

    log_info(
        "Boot",
        &format!(
            "BOT-RUST standalone start mode={} once={}",
            config.mode(),
            config.once
        ),
    );

    telegram.start_command_listener();

    if telegram.enabled {
        let startup_message = format!(
            "[POLYMARKET BOT RUST START]\nmode      : {}\nstake     : ${:.2}\ncommands  : /pause /resume /status",
            config.mode(),
            config.stake_usd
        );
        let _ = telegram.send(&startup_message).await;
    }

    if config.total_loss_trades > 0 && !telegram.enabled {
        log_warn(
            "Risk",
            "TOTAL_LOSS_TRADES is enabled but Telegram is disabled; /resume command is unavailable until restart",
        );
    }

    if config.once {
        if telegram.is_paused() {
            log_warn("BOT", "trading paused (use /resume to continue)");
            return Ok(());
        }

        let _ = run_cycle(
            &config,
            &mut processed_windows,
            &mut pending_claims,
            &telegram,
        )
        .await?;

        if should_manage_claims(&config) {
            if let Err(error) = sweep_pending_claims(&config, &mut pending_claims, &telegram).await
            {
                log_warn(
                    "Settlement",
                    &format!("claim sweep ended with error: {error}"),
                );
            }
        }

        return Ok(());
    }

    loop {
        if should_manage_claims(&config) {
            if let Err(error) = sweep_pending_claims(&config, &mut pending_claims, &telegram).await
            {
                log_warn(
                    "Settlement",
                    &format!("claim sweep ended with error: {error}"),
                );
            }
        }

        if telegram.is_paused() {
            if !pause_notice_sent {
                pause_notice_sent = true;
                log_warn("BOT", "trading paused (use /resume to continue)");
            }

            sleep_ms(2_000).await;
            continue;
        }

        if pause_notice_sent {
            pause_notice_sent = false;
            log_info("BOT", "trading resumed");

            if paused_by_loss_limit {
                paused_by_loss_limit = false;
                losses_since_resume = 0;
                log_info("Risk", "loss counter reset after /resume");

                if telegram.enabled {
                    let _ = telegram
                        .send("[POLYMARKET BOT LOSS GUARD RESET]\nlosses   : 0\nstate    : RUNNING")
                        .await;
                }
            }
        }

        if let Some(cooldown_until_sec) = loss_cooldown_until_sec {
            let now = now_sec();
            if now < cooldown_until_sec {
                sleep_ms(2_000).await;
                continue;
            }

            loss_cooldown_until_sec = None;
            log_info("Risk", "loss cooldown finished, trading resumed");

            if telegram.enabled {
                let _ = telegram
                    .send("[POLYMARKET BOT LOSS COOLDOWN END]\nstate    : RUNNING")
                    .await;
            }
        }

        match run_cycle(
            &config,
            &mut processed_windows,
            &mut pending_claims,
            &telegram,
        )
        .await
        {
            Ok(CycleOutcome::Loss) => {
                losses_since_resume = losses_since_resume.saturating_add(1);
                let mut loss_guard_triggered = false;

                if config.total_loss_trades > 0
                    && losses_since_resume >= config.total_loss_trades
                    && !telegram.is_paused()
                {
                    loss_guard_triggered = true;
                    paused_by_loss_limit = true;

                    let reason = format!(
                        "loss trades reached {}/{}",
                        losses_since_resume, config.total_loss_trades
                    );
                    let _ = telegram.set_paused(true, Some(&reason)).await;

                    log_warn(
                        "Risk",
                        &format!(
                            "loss-trade guard reached ({}/{}), trading paused until /resume",
                            losses_since_resume, config.total_loss_trades
                        ),
                    );

                    if telegram.enabled {
                        let _ = telegram
                            .send(&format!(
                                "[POLYMARKET BOT LOSS GUARD]\nlosses   : {}/{}\nnext step: send /resume to continue",
                                losses_since_resume, config.total_loss_trades
                            ))
                            .await;
                    }
                }

                if !loss_guard_triggered && config.loss_cooldown_minutes > 0 {
                    let cooldown_sec = config.loss_cooldown_minutes.saturating_mul(60);
                    if cooldown_sec > 0 {
                        loss_cooldown_until_sec = Some(now_sec().saturating_add(cooldown_sec));

                        log_info(
                            "Risk",
                            &format!(
                                "loss cooldown started for {} minute(s) after losing trade",
                                config.loss_cooldown_minutes
                            ),
                        );

                        if telegram.enabled {
                            let _ = telegram
                                .send(&format!(
                                    "[POLYMARKET BOT LOSS COOLDOWN]\nminutes  : {}\nnext step: waiting before next entry",
                                    config.loss_cooldown_minutes
                                ))
                                .await;
                        }
                    }
                }
            }
            Ok(CycleOutcome::Win) | Ok(CycleOutcome::None) => {}
            Err(error) => {
                log_warn("Cycle", &format!("cycle ended with error: {error}"));
            }
        }

        sleep_ms(config.idle_poll_interval_ms).await;
    }
}

fn cleanup_processed_windows(processed_windows: &mut HashSet<u64>, current_window_start_sec: u64) {
    if processed_windows.len() < 512 {
        return;
    }

    let min_keep = current_window_start_sec.saturating_sub(3_600);
    processed_windows.retain(|window_start| *window_start >= min_keep);
}

fn side_label(side: MarketSide) -> &'static str {
    match side {
        MarketSide::Up => "UP",
        MarketSide::Down => "DOWN",
    }
}

fn buy_band_reject_reason(config: &Config, price: f64) -> Option<&'static str> {
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

async fn lookup_market_until_deadline(
    config: &Config,
    slug: &str,
    close_time_sec: u64,
) -> Result<Option<DiscoveredMarket>> {
    let started_at_ms = now_sec().saturating_mul(1_000);

    loop {
        match fetch_market_by_slug(config, slug).await {
            Ok(Some(market)) => return Ok(Some(market)),
            Ok(None) => {}
            Err(error) => {
                log_warn("Market", &format!("lookup error slug={slug}: {error}"));
            }
        }

        let elapsed_ms = now_sec()
            .saturating_mul(1_000)
            .saturating_sub(started_at_ms);

        if elapsed_ms >= config.market_lookup_max_wait_ms || now_sec() >= close_time_sec {
            return Ok(None);
        }

        sleep_ms(config.market_poll_interval_ms).await;
    }
}

async fn fetch_side_candidate(
    config: &Config,
    side: MarketSide,
    token_id: &str,
) -> Option<SideCandidate> {
    if token_id.trim().is_empty() {
        log_warn(
            "Signal",
            &format!("side={} candidate missing token id", side_label(side)),
        );
        return None;
    }

    match fetch_orderbook_snapshot(config, token_id).await {
        Ok(orderbook) => {
            if !orderbook.asks_present || orderbook.best_ask <= 0.0 {
                return None;
            }

            if let Some(reason) = buy_band_reject_reason(config, orderbook.best_ask) {
                log_warn(
                    "Signal",
                    &format!(
                        "side={} gate reject bestAsk={:.3} band={:.2}-{:.2} reason={}",
                        side_label(side),
                        orderbook.best_ask,
                        config.price_range_min,
                        config.price_range_max,
                        reason
                    ),
                );
                return None;
            }

            Some(SideCandidate {
                side,
                token_id: token_id.to_owned(),
                ask_price: orderbook.best_ask,
                bid_price: orderbook.best_bid,
            })
        }
        Err(error) => {
            log_warn(
                "Signal",
                &format!(
                    "side={} orderbook fetch failed token={} err={}",
                    side_label(side),
                    token_id,
                    error
                ),
            );
            None
        }
    }
}

async fn select_side_candidate(
    config: &Config,
    market: &DiscoveredMarket,
) -> Option<SideCandidate> {
    let up = fetch_side_candidate(config, MarketSide::Up, &market.yes_token_id).await;
    if up.is_some() {
        return up;
    }

    fetch_side_candidate(config, MarketSide::Down, &market.no_token_id).await
}

async fn run_cycle(
    config: &Config,
    processed_windows: &mut HashSet<u64>,
    pending_claims: &mut HashMap<String, PendingClaim>,
    telegram: &TelegramNotifier,
) -> Result<CycleOutcome> {
    let window = build_window(None);

    cleanup_processed_windows(processed_windows, window.window_start_sec);

    if processed_windows.contains(&window.window_start_sec) {
        return Ok(CycleOutcome::None);
    }

    log_cycle_separator(&window.slug);
    log_info(
        "Cycle",
        &format!(
            "windowStart={} close={} stage={:?}",
            window.window_start_sec,
            window.close_time_sec,
            RuntimeStage::Cycle
        ),
    );

    if !config.enable_live_trading {
        log_info(
            "Live",
            "Live trading is disabled. Execution wiring is active and waiting for LIVE mode.",
        );
        processed_windows.insert(window.window_start_sec);
        return Ok(CycleOutcome::None);
    }

    let entry_target_sec = window
        .close_time_sec
        .saturating_sub(config.check_before_close_sec);
    let now = now_sec();
    if now < entry_target_sec {
        let wait_sec = entry_target_sec.saturating_sub(now);
        log_info(
            "Cycle",
            &format!(
                "stage={:?} waiting {}s until pre-close entry checkpoint",
                RuntimeStage::WaitT10,
                wait_sec
            ),
        );
        sleep_until(entry_target_sec).await;
    }

    if now_sec() >= window.close_time_sec {
        log_warn("Cycle", "window closed before entry step");
        processed_windows.insert(window.window_start_sec);
        return Ok(CycleOutcome::None);
    }

    log_info(
        "Market",
        &format!(
            "stage={:?} lookup slug={}",
            RuntimeStage::LookupMarket,
            window.slug
        ),
    );

    let market = lookup_market_until_deadline(config, &window.slug, window.close_time_sec).await?;
    let Some(market) = market else {
        log_warn(
            "Market",
            &format!("market not found for slug={}", window.slug),
        );
        processed_windows.insert(window.window_start_sec);
        return Ok(CycleOutcome::None);
    };

    let candidate = select_side_candidate(config, &market).await;
    let Some(candidate) = candidate else {
        log_warn(
            "Signal",
            &format!("no side candidate in configured range for {}", window.slug),
        );
        processed_windows.insert(window.window_start_sec);
        return Ok(CycleOutcome::None);
    };

    log_info(
        "Signal",
        &format!(
            "selected side={} ask={:.4} bid={:.4}",
            side_label(candidate.side),
            candidate.ask_price,
            candidate.bid_price
        ),
    );

    let execution = execute_live_entry(
        config,
        &candidate.token_id,
        config.stake_usd,
        window.close_time_sec,
    )
    .await?;

    let reason = execution
        .raw_response
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("");

    if execution.filled_size > 0.0 && execution.spent_usd > 0.0 {
        log_info(
            "Entry",
            &format!(
                "status={} order={} filled={:.6} spent=${:.4}",
                execution_status_label(execution.status),
                execution.order_id,
                execution.filled_size,
                execution.spent_usd,
            ),
        );

        if telegram.enabled {
            let _ = telegram
                .send(&format!(
                    "[POLYMARKET BOT ENTRY]\nmarket: {}\nside  : {}\nstatus: {}\norder : {}\nspent : ${:.4}\nfilled: {:.6}",
                    window.slug,
                    side_label(candidate.side),
                    execution_status_label(execution.status),
                    execution.order_id,
                    execution.spent_usd,
                    execution.filled_size,
                ))
                .await;
        }

        if let Some(outcome) = parse_post_fill_sell_limit_outcome(&execution.raw_response) {
            if telegram.enabled {
                let title = if outcome.success {
                    "[POLYMARKET BOT EXIT ORDER OK]"
                } else {
                    "[POLYMARKET BOT EXIT ORDER FAIL]"
                };

                let sell_price = outcome
                    .final_price
                    .map(|value| format!("{value:.3}"))
                    .unwrap_or_else(|| "n/a".to_owned());
                let retryable = outcome
                    .retryable
                    .map(|value| if value { "yes" } else { "no" })
                    .unwrap_or("n/a");

                let _ = telegram
                    .send(&format!(
                        "{}\nwindow   : {}\nside     : {}\nbuyOrder : {}\nsellType : {}\nsellStat : {}\nsellId   : {}\nsellQty  : {:.6}\nsellPx   : {}\nattempted: {}\nretryable: {}\nerrCode  : {}\nerrPhase : {}\nerrMsg   : {}",
                        title,
                        window.slug,
                        side_label(candidate.side),
                        if execution.order_id.is_empty() {
                            "n/a"
                        } else {
                            execution.order_id.as_str()
                        },
                        outcome.order_type,
                        outcome.status.as_deref().unwrap_or("unknown"),
                        outcome.order_id.as_deref().unwrap_or("n/a"),
                        outcome.size,
                        sell_price,
                        if outcome.attempted { "yes" } else { "no" },
                        retryable,
                        outcome.error_code.as_deref().unwrap_or("-"),
                        outcome.error_phase.as_deref().unwrap_or("-"),
                        clip_text(outcome.error_msg.as_deref().unwrap_or("-"), 220),
                    ))
                    .await;
            }
        }

        let resolve_target_sec = window
            .close_time_sec
            .saturating_add(config.resolve_delay_sec);
        if now_sec() < resolve_target_sec {
            let wait_sec = resolve_target_sec.saturating_sub(now_sec());
            log_info(
                "Cycle",
                &format!(
                    "stage={:?} waiting {}s until settlement checkpoint",
                    RuntimeStage::WaitResolve,
                    wait_sec
                ),
            );
            sleep_until(resolve_target_sec).await;
        }

        log_info(
            "Settlement",
            &format!(
                "stage={:?} resolving window={}",
                RuntimeStage::Resolving,
                window.slug
            ),
        );

        let settlement =
            match resolve_window_outcome(config, window.window_start_sec, &window.slug).await {
                Ok(value) => value,
                Err(error) => {
                    log_warn(
                        "Settlement",
                        &format!(
                            "failed settlement resolution for {}: {}",
                            window.slug, error
                        ),
                    );
                    processed_windows.insert(window.window_start_sec);
                    return Ok(CycleOutcome::None);
                }
            };

        let pnl = compute_trade_pnl(candidate.side, &execution, settlement.outcome);
        let won = matches!(pnl.outcome, crate::types::TradeResult::Win);

        let claim_enabled = should_manage_claims(config);
        let record_id = trade_id(window.window_start_sec);
        let timestamp_ms = now_ms();

        let trade_record = TradeRecord {
            id: record_id.clone(),
            timestamp_ms,
            mode: config.mode().to_owned(),
            window: window.clone(),
            market: market.clone(),
            side: candidate.side,
            selected_ask_price: candidate.ask_price,
            selected_bid_price: candidate.bid_price,
            stake_usd: execution.spent_usd,
            execution: execution.clone(),
            settlement: settlement.clone(),
            outcome: pnl.outcome,
            redeemed_usd: pnl.redeemed_usd,
            pnl_usd: pnl.pnl_usd,
            claim_status: if claim_enabled {
                Some(ClaimStatus::Pending)
            } else {
                None
            },
            claim_attempts: if claim_enabled { Some(0) } else { None },
            claim_tx_hash: None,
            claim_last_error: None,
            claim_updated_at_ms: if claim_enabled {
                Some(timestamp_ms)
            } else {
                None
            },
            market_resolved: if claim_enabled { Some(false) } else { None },
            market_resolution_source: None,
            market_resolved_at_ms: None,
        };

        if let Err(error) = log_trade_record(&config.trades_output_path, &trade_record).await {
            log_warn(
                "Storage",
                &format!(
                    "failed to persist trade record on {}: {}",
                    window.slug, error
                ),
            );
        }

        if claim_enabled {
            pending_claims.insert(
                record_id.clone(),
                PendingClaim {
                    trade_id: record_id.clone(),
                    window_slug: window.slug.clone(),
                    condition_id: market.condition_id.clone(),
                    yes_token_id: market.yes_token_id.clone(),
                    no_token_id: market.no_token_id.clone(),
                    created_at_ms: timestamp_ms,
                    poll_count: 0,
                    claim_attempts: 0,
                    last_update_ms: timestamp_ms,
                    last_error: None,
                },
            );

            if let Err(error) = sweep_pending_claims(config, pending_claims, telegram).await {
                log_warn(
                    "Settlement",
                    &format!("claim sweep ended with error: {error}"),
                );
            }
        }

        let pnl_sign = if pnl.pnl_usd >= 0.0 { "+" } else { "" };
        log_info(
            "BOT",
            &format!(
                "{} {} {} | pnl={}${:.2} | source={:?}{}",
                window.slug,
                side_label(candidate.side),
                trade_result_label(won),
                pnl_sign,
                pnl.pnl_usd,
                settlement.source,
                if claim_enabled {
                    " | claim=pending"
                } else {
                    ""
                }
            ),
        );

        if telegram.enabled {
            let _ = telegram
                .send(&format!(
                    "[POLYMARKET BOT TRADE {}]\nwindow  : {}\nside    : {}\nspent   : ${:.2}\npnl     : {}${:.2}",
                    execution_status_label(execution.status),
                    window.slug,
                    side_label(candidate.side),
                    execution.spent_usd,
                    pnl_sign,
                    pnl.pnl_usd,
                ))
                .await;
        }

        processed_windows.insert(window.window_start_sec);
        return Ok(if won {
            CycleOutcome::Win
        } else {
            CycleOutcome::Loss
        });
    } else {
        log_warn(
            "Entry",
            &format!(
                "not filled status={}{}",
                execution_status_label(execution.status),
                if reason.is_empty() {
                    String::new()
                } else {
                    format!(", reason={reason}")
                }
            ),
        );

        if telegram.enabled {
            let _ = telegram
                .send(&format!(
                    "[POLYMARKET BOT ENTRY]\nmarket: {}\nside  : {}\nstatus: {}{}",
                    window.slug,
                    side_label(candidate.side),
                    execution_status_label(execution.status),
                    if reason.is_empty() {
                        String::new()
                    } else {
                        format!("\nreason: {}", reason)
                    }
                ))
                .await;
        }
    }

    processed_windows.insert(window.window_start_sec);

    Ok(CycleOutcome::None)
}
