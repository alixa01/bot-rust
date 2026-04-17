use std::collections::HashSet;

use anyhow::Result;
use serde_json::Value;

use crate::data::market_discovery::fetch_market_by_slug;
use crate::data::orderbook::fetch_orderbook_snapshot;
use crate::execution::order_executor::execute_live_entry;
use crate::notifications::telegram_notifier::{create_telegram_notifier, TelegramNotifier};
use crate::types::{DiscoveredMarket, MarketSide, Config};
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

pub async fn run_orchestrator(config: Config) -> Result<()> {
    let mut processed_windows: HashSet<u64> = HashSet::new();
    let telegram = create_telegram_notifier(&config);
    let mut pause_notice_sent = false;

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

    if config.once {
        if telegram.is_paused() {
            log_warn("BOT", "trading paused (use /resume to continue)");
            return Ok(());
        }

        run_cycle(&config, &mut processed_windows, &telegram).await?;
        return Ok(());
    }

    loop {
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
        }

        if let Err(error) = run_cycle(&config, &mut processed_windows, &telegram).await {
            log_warn("Cycle", &format!("cycle ended with error: {error}"));
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

async fn select_side_candidate(config: &Config, market: &DiscoveredMarket) -> Option<SideCandidate> {
    let up = fetch_side_candidate(config, MarketSide::Up, &market.yes_token_id).await;
    if up.is_some() {
        return up;
    }

    fetch_side_candidate(config, MarketSide::Down, &market.no_token_id).await
}

async fn run_cycle(
    config: &Config,
    processed_windows: &mut HashSet<u64>,
    telegram: &TelegramNotifier,
) -> Result<()> {
    let window = build_window(None);

    cleanup_processed_windows(processed_windows, window.window_start_sec);

    if processed_windows.contains(&window.window_start_sec) {
        return Ok(());
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
        return Ok(());
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
        return Ok(());
    }

    log_info(
        "Market",
        &format!("stage={:?} lookup slug={}", RuntimeStage::LookupMarket, window.slug),
    );

    let market = lookup_market_until_deadline(config, &window.slug, window.close_time_sec).await?;
    let Some(market) = market else {
        log_warn("Market", &format!("market not found for slug={}", window.slug));
        processed_windows.insert(window.window_start_sec);
        return Ok(());
    };

    let candidate = select_side_candidate(config, &market).await;
    let Some(candidate) = candidate else {
        log_warn(
            "Signal",
            &format!("no side candidate in configured range for {}", window.slug),
        );
        processed_windows.insert(window.window_start_sec);
        return Ok(());
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
                match execution.status {
                    crate::types::ExecutionStatus::Pending => "PENDING",
                    crate::types::ExecutionStatus::Filled => "FILLED",
                    crate::types::ExecutionStatus::Partial => "PARTIAL",
                    crate::types::ExecutionStatus::Cancelled => "CANCELLED",
                    crate::types::ExecutionStatus::Failed => "FAILED",
                    crate::types::ExecutionStatus::Skipped => "SKIPPED",
                },
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
                    match execution.status {
                        crate::types::ExecutionStatus::Pending => "PENDING",
                        crate::types::ExecutionStatus::Filled => "FILLED",
                        crate::types::ExecutionStatus::Partial => "PARTIAL",
                        crate::types::ExecutionStatus::Cancelled => "CANCELLED",
                        crate::types::ExecutionStatus::Failed => "FAILED",
                        crate::types::ExecutionStatus::Skipped => "SKIPPED",
                    },
                    execution.order_id,
                    execution.spent_usd,
                    execution.filled_size,
                ))
                .await;
        }
    } else {
        log_warn(
            "Entry",
            &format!(
                "not filled status={}{}",
                match execution.status {
                    crate::types::ExecutionStatus::Pending => "PENDING",
                    crate::types::ExecutionStatus::Filled => "FILLED",
                    crate::types::ExecutionStatus::Partial => "PARTIAL",
                    crate::types::ExecutionStatus::Cancelled => "CANCELLED",
                    crate::types::ExecutionStatus::Failed => "FAILED",
                    crate::types::ExecutionStatus::Skipped => "SKIPPED",
                },
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
                    match execution.status {
                        crate::types::ExecutionStatus::Pending => "PENDING",
                        crate::types::ExecutionStatus::Filled => "FILLED",
                        crate::types::ExecutionStatus::Partial => "PARTIAL",
                        crate::types::ExecutionStatus::Cancelled => "CANCELLED",
                        crate::types::ExecutionStatus::Failed => "FAILED",
                        crate::types::ExecutionStatus::Skipped => "SKIPPED",
                    },
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

    Ok(())
}
