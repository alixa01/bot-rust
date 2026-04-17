use anyhow::Result;

use crate::types::V3Config;
use crate::utils::logger::{log_cycle_separator, log_info, log_warn};
use crate::utils::time::{build_window, sleep_ms};

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

pub async fn run_orchestrator(config: V3Config) -> Result<()> {
    log_info(
        "Boot",
        &format!(
            "BOT-RUST standalone start mode={} once={}",
            config.mode(),
            config.once
        ),
    );

    if config.once {
        run_cycle(&config).await?;
        return Ok(());
    }

    loop {
        if let Err(error) = run_cycle(&config).await {
            log_warn("Cycle", &format!("cycle ended with error: {error}"));
        }

        sleep_ms(config.idle_poll_interval_ms).await;
    }
}

async fn run_cycle(config: &V3Config) -> Result<()> {
    let window = build_window(None);

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

    log_info(
        "Cycle",
        "Implementation in progress: full execution/settlement wiring will be added incrementally.",
    );

    if config.enable_live_trading {
        log_warn(
            "Live",
            "Live switch is enabled, but order execution is not yet wired in this initial Rust bootstrap.",
        );
    }

    Ok(())
}
