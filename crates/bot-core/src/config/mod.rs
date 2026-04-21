use std::env;
use std::path::Path;

use anyhow::{bail, Result};

use crate::types::{Config, LivePriceSource, SettlementTxMode, SignatureType};

fn legacy_prefixed_env_keys() -> Vec<String> {
    let mut found: Vec<String> = env::vars_os()
        .filter_map(|(key, _)| key.into_string().ok())
        .filter(|key| {
            let bytes = key.as_bytes();
            bytes.len() > 3 && bytes[0] == b'V' && bytes[1] == b'3' && bytes[2] == b'_'
        })
        .collect();

    found.sort();
    found
}

fn ensure_no_legacy_prefixed_keys() -> Result<()> {
    let found = legacy_prefixed_env_keys();

    if found.is_empty() {
        return Ok(());
    }

    let migration_hints = found
        .iter()
        .map(|key| format!("{key} -> {}", &key[3..]))
        .collect::<Vec<_>>()
        .join(", ");

    bail!(
        "Legacy env keys are no longer supported: {}. Rename them to unprefixed keys: {}",
        found.join(", "),
        migration_hints
    )
}

fn optional_env(key: &str, fallback: &str) -> String {
    match env::var(key) {
        Ok(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                fallback.to_owned()
            } else {
                trimmed.to_owned()
            }
        }
        Err(_) => fallback.to_owned(),
    }
}

fn optional_any_env(keys: &[&str], fallback: &str) -> String {
    for key in keys {
        if let Ok(value) = env::var(key) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return trimmed.to_owned();
            }
        }
    }

    fallback.to_owned()
}

fn boolean_env(key: &str, fallback: bool) -> Result<bool> {
    let raw = optional_env(key, if fallback { "true" } else { "false" }).to_lowercase();
    match raw.as_str() {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        _ => bail!("{key} must be boolean (true/false/1/0), got: {raw}"),
    }
}

fn number_env(key: &str, fallback: f64) -> Result<f64> {
    let raw = optional_env(key, &fallback.to_string());
    let parsed: f64 = raw
        .parse()
        .map_err(|_| anyhow::anyhow!("{key} must be a valid number, got: {raw}"))?;
    if !parsed.is_finite() {
        bail!("{key} must be a finite number, got: {raw}");
    }

    Ok(parsed)
}

fn number_env_in_range(key: &str, fallback: f64, min: f64, max: f64) -> Result<f64> {
    let value = number_env(key, fallback)?;
    if value < min || value > max {
        bail!("{key} must be between {min} and {max}, got: {value}");
    }

    Ok(value)
}

fn floor_u64_env(key: &str, fallback: f64) -> Result<u64> {
    let value = number_env(key, fallback)?;
    if value < 0.0 {
        bail!("{key} must be >= 0, got: {value}");
    }

    Ok(value.floor() as u64)
}

fn live_price_source_env() -> Result<LivePriceSource> {
    let raw = optional_env("LIVE_PRICE_SOURCE", "CHAINLINK_PUBLIC").to_uppercase();
    match raw.as_str() {
        "BINANCE" => Ok(LivePriceSource::Binance),
        "CHAINLINK_PUBLIC" => Ok(LivePriceSource::ChainlinkPublic),
        _ => bail!("LIVE_PRICE_SOURCE must be BINANCE or CHAINLINK_PUBLIC, got: {raw}"),
    }
}

fn settlement_tx_mode_env() -> Result<SettlementTxMode> {
    let raw = optional_env("SETTLEMENT_TX_MODE", "DIRECT_ETHERS").to_uppercase();
    match raw.as_str() {
        "DIRECT_ETHERS" => Ok(SettlementTxMode::DirectEthers),
        "RELAYER_SAFE" => Ok(SettlementTxMode::RelayerSafe),
        _ => bail!("SETTLEMENT_TX_MODE must be DIRECT_ETHERS or RELAYER_SAFE, got: {raw}"),
    }
}

fn signature_type_env() -> Result<SignatureType> {
    let raw = optional_env("POLYMARKET_SIGNATURE_TYPE", "2");
    match raw.as_str() {
        "0" => Ok(SignatureType::Eoa),
        "1" => Ok(SignatureType::Safe),
        "2" => Ok(SignatureType::SmartContractWallet),
        _ => bail!("POLYMARKET_SIGNATURE_TYPE must be 0/1/2, got: {raw}"),
    }
}

fn stop_loss_order_type_env() -> Result<String> {
    let raw = optional_env("STOP_LOSS_ORDER_TYPE", "GTC").to_uppercase();
    match raw.as_str() {
        "GTC" | "FOK" => Ok(raw),
        _ => bail!("STOP_LOSS_ORDER_TYPE must be GTC or FOK, got: {raw}"),
    }
}

fn normalize_private_key(raw: &str) -> String {
    let trimmed = raw.trim();
    if trimmed.starts_with("0x") {
        trimmed.to_owned()
    } else {
        format!("0x{trimmed}")
    }
}

fn is_address(value: &str) -> bool {
    let trimmed = value.trim();
    if trimmed.len() != 42 || !trimmed.starts_with("0x") {
        return false;
    }

    trimmed.chars().skip(2).all(|c| c.is_ascii_hexdigit())
}

fn normalize_optional_address(raw: &str) -> Result<Option<String>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    if !is_address(trimmed) {
        bail!("Expected address format 0x + 40 hex chars, got: {trimmed}");
    }

    Ok(Some(trimmed.to_owned()))
}

pub fn load_config(argv: &[String], root_dir: &Path) -> Result<Config> {
    ensure_no_legacy_prefixed_keys()?;

    let once = if argv.iter().any(|arg| arg == "--once") {
        true
    } else {
        boolean_env("ONCE", false)?
    };
    let debug = boolean_env("DEBUG", false)?;

    let heartbeat_interval_sec = floor_u64_env("HEARTBEAT_INTERVAL_SECONDS", 15.0)?;
    let silent_watchdog_sec = floor_u64_env("SILENT_WATCHDOG_SECONDS", 60.0)?;

    let enable_live_trading = boolean_env("ENABLE_LIVE_TRADING", false)?;
    let stake_usd = number_env("STAKE_USD", 1.0)?;

    let price_range_min = number_env("PRICE_RANGE_MIN", 0.75)?;
    let price_range_max = number_env("PRICE_RANGE_MAX", 0.95)?;
    let entry_price_gate_enabled = boolean_env("ENABLE_ENTRY_PRICE_GATE", true)?;
    let entry_price_retry_interval_ms = floor_u64_env("ENTRY_PRICE_RETRY_INTERVAL_MS", 300.0)?;
    let entry_price_max_retries = floor_u64_env("ENTRY_PRICE_MAX_RETRIES", 0.0)?;
    let entry_slippage_percent_buy = number_env("ENTRY_SLIPPAGE_PERCENT_BUY", 1.5)?;
    let enable_fallback_gtc_limit = boolean_env("ENABLE_FALLBACK_GTC_LIMIT", false)?;
    let enable_stop_loss = boolean_env("ENABLE_STOP_LOSS", false)?;
    let stop_loss_price_trigger = number_env("STOP_LOSS_PRICE_TRIGGER", 0.35)?;
    let interval_check_price_trigger_ms = floor_u64_env("INTERVAL_CHECK_PRICE_TRIGGER", 500.0)?;
    let retry_sell = floor_u64_env("RETRY_SELL", 4.0)?;
    let stop_loss_timeout_sec = floor_u64_env("STOP_LOSS_TIMEOUT_SECONDS", 120.0)?;
    let stop_loss_order_type = stop_loss_order_type_env()?;
    let stop_loss_submit_retry_interval_ms =
        floor_u64_env("STOP_LOSS_SUBMIT_RETRY_INTERVAL_MS", 1000.0)?;
    let stop_loss_deadline_before_close_sec =
        floor_u64_env("STOP_LOSS_DEADLINE_BEFORE_CLOSE_SECONDS", 5.0)?;

    let check_before_close_sec = floor_u64_env("CHECK_BEFORE_CLOSE_SECONDS", 10.0)?;
    let resolve_delay_sec = floor_u64_env("RESOLVE_DELAY_SECONDS", 2.0)?;
    let idle_poll_interval_ms = floor_u64_env("IDLE_POLL_INTERVAL_MS", 1000.0)?;
    let market_poll_interval_ms = floor_u64_env("MARKET_POLL_INTERVAL_MS", 500.0)?;
    let market_lookup_max_wait_ms = floor_u64_env("MARKET_LOOKUP_MAX_WAIT_MS", 12000.0)?;
    let order_retry_interval_ms = floor_u64_env("ORDER_RETRY_INTERVAL_MS", 1000.0)?;
    let order_max_attempts = floor_u64_env("ORDER_MAX_ATTEMPTS", 4.0)?;
    let status_poll_interval_ms = floor_u64_env("STATUS_POLL_INTERVAL_MS", 500.0)?;
    let status_poll_grace_sec = floor_u64_env("STATUS_POLL_GRACE_SECONDS", 6.0)?;

    let loss_cooldown_minutes = floor_u64_env("LOSS_COOLDOWN_MINUTES", 0.0)?;
    let total_loss_trades = floor_u64_env("TOTAL_LOSS_TRADES", 0.0)?;

    let polymarket_clob_url = optional_env("POLYMARKET_CLOB_URL", "https://clob.polymarket.com");
    let polymarket_gamma_url =
        optional_env("POLYMARKET_GAMMA_URL", "https://gamma-api.polymarket.com");
    let binance_base_url =
        optional_env("BINANCE_BASE_URL", "https://data-api.binance.vision/api/v3");

    let live_price_source = live_price_source_env()?;
    let chainlink_btc_usd_feed_address = optional_env(
        "CHAINLINK_BTC_USD_FEED_ADDRESS",
        "0xc907E116054Ad103354f2D350FD2514433D57F6f",
    );
    let live_price_max_staleness_ms = floor_u64_env("LIVE_PRICE_MAX_STALENESS_MS", 300000.0)?;

    let private_key_raw = optional_any_env(&["PRIVATE_KEY", "POLYMARKET_PRIVATE_KEY"], "");
    let funder_address_raw = optional_any_env(
        &[
            "FUNDER_ADDRESS",
            "POLYMARKET_FUNDER_ADDRESS",
            "POLYMARKET_BALANCE_ADDRESS",
        ],
        "",
    );

    let private_key = if private_key_raw.is_empty() {
        String::new()
    } else {
        normalize_private_key(&private_key_raw)
    };
    let funder_address = funder_address_raw.trim().to_owned();

    let signature_type = signature_type_env()?;
    let api_key = optional_env("POLYMARKET_API_KEY", "");
    let api_secret = optional_env("POLYMARKET_API_SECRET", "");
    let api_passphrase = optional_env("POLYMARKET_API_PASSPHRASE", "");

    let telegram_bot_token = {
        let raw = optional_env("TELEGRAM_BOT_TOKEN", "");
        if raw.trim().is_empty() {
            None
        } else {
            Some(raw)
        }
    };

    let telegram_chat_id = {
        let raw = optional_env("TELEGRAM_CHAT_ID", "");
        if raw.trim().is_empty() {
            None
        } else {
            Some(raw)
        }
    };

    let on_chain_auto_claim_enabled = boolean_env("ENABLE_ONCHAIN_AUTO_CLAIM", true)?;
    let settlement_tx_mode = settlement_tx_mode_env()?;
    let relayer_base_url = optional_env("RELAYER_BASE_URL", "https://relayer-v2.polymarket.com");
    let relayer_api_key = {
        let raw = optional_env("RELAYER_API_KEY", "");
        if raw.trim().is_empty() {
            None
        } else {
            Some(raw)
        }
    };

    let relayer_api_key_address =
        normalize_optional_address(&optional_env("RELAYER_API_KEY_ADDRESS", ""))?;
    let relayer_request_timeout_ms = floor_u64_env("RELAYER_REQUEST_TIMEOUT_MS", 30000.0)?;
    let relayer_poll_interval_ms = floor_u64_env("RELAYER_POLL_INTERVAL_MS", 2000.0)?;
    let relayer_max_polls = floor_u64_env("RELAYER_MAX_POLLS", 120.0)?;
    let relayer_allow_fallback_to_direct = boolean_env("RELAYER_ALLOW_FALLBACK_TO_DIRECT", true)?;

    let settlement_max_attempts = floor_u64_env("SETTLEMENT_MAX_ATTEMPTS", 3.0)?;
    let settlement_retry_delay_ms = floor_u64_env("SETTLEMENT_RETRY_DELAY_MS", 5000.0)?;
    let enable_gamma_resolution_fallback = boolean_env("ENABLE_GAMMA_RESOLUTION_FALLBACK", true)?;

    let redeem_gas_limit_multiplier =
        number_env_in_range("REDEEM_GAS_LIMIT_MULTIPLIER", 1.3, 1.0, 5.0)?;
    let redeem_min_gas_limit = floor_u64_env("REDEEM_MIN_GAS_LIMIT", 300000.0)?;
    let redeem_max_fee_per_gas_gwei =
        number_env_in_range("REDEEM_MAX_FEE_PER_GAS_GWEI", 100.0, 0.0, 1000.0)?;
    let redeem_max_priority_fee_per_gas_gwei =
        number_env_in_range("REDEEM_MAX_PRIORITY_FEE_PER_GAS_GWEI", 30.0, 0.0, 1000.0)?;
    let redeem_internal_max_attempts = floor_u64_env("REDEEM_INTERNAL_MAX_ATTEMPTS", 3.0)?;
    let redeem_internal_retry_base_delay_ms =
        floor_u64_env("REDEEM_INTERNAL_RETRY_BASE_DELAY_MS", 2000.0)?;
    let redeem_internal_retry_backoff_multiplier =
        number_env_in_range("REDEEM_INTERNAL_RETRY_BACKOFF_MULTIPLIER", 2.0, 1.0, 5.0)?;
    let redeem_tx_confirm_timeout_ms = floor_u64_env("REDEEM_TX_CONFIRM_TIMEOUT_MS", 120000.0)?;

    let polygon_rpc_url = optional_env("POLYGON_RPC_URL", "https://polygon-rpc.com");
    let ctf_contract = optional_env("CTF_CONTRACT", "0x4D97DCd97eC945f40cF65F87097ACe5EA0476045");
    let usdc_address = optional_env("USDC_E", "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174");

    let output_file = optional_env("OUTPUT_FILE", "result.jsonl");
    let trades_output_file = optional_env("TRADES_OUTPUT_FILE", "trades.jsonl");
    let output_path = root_dir.join(output_file);
    let trades_output_path = root_dir.join(trades_output_file);

    if heartbeat_interval_sec == 0 {
        bail!("HEARTBEAT_INTERVAL_SECONDS must be > 0");
    }
    if silent_watchdog_sec == 0 {
        bail!("SILENT_WATCHDOG_SECONDS must be > 0");
    }
    if stake_usd <= 0.0 {
        bail!("STAKE_USD must be > 0");
    }
    if !(0.0..1.0).contains(&price_range_min) {
        bail!("PRICE_RANGE_MIN must be between 0 and 1");
    }
    if !(0.0..1.0).contains(&price_range_max) {
        bail!("PRICE_RANGE_MAX must be between 0 and 1");
    }
    if price_range_min >= price_range_max {
        bail!("PRICE_RANGE_MIN must be < PRICE_RANGE_MAX");
    }
    if entry_slippage_percent_buy < 0.0 {
        bail!("ENTRY_SLIPPAGE_PERCENT_BUY must be >= 0");
    }
    if entry_price_max_retries > 0 && entry_price_retry_interval_ms == 0 {
        bail!("ENTRY_PRICE_RETRY_INTERVAL_MS must be > 0 when ENTRY_PRICE_MAX_RETRIES > 0");
    }
    if !(0.01..=0.99).contains(&stop_loss_price_trigger) {
        bail!("STOP_LOSS_PRICE_TRIGGER must be between 0.01 and 0.99");
    }
    if interval_check_price_trigger_ms == 0 {
        bail!("INTERVAL_CHECK_PRICE_TRIGGER must be > 0");
    }
    if retry_sell == 0 {
        bail!("RETRY_SELL must be > 0");
    }
    if stop_loss_timeout_sec == 0 {
        bail!("STOP_LOSS_TIMEOUT_SECONDS must be > 0");
    }
    if stop_loss_submit_retry_interval_ms == 0 {
        bail!("STOP_LOSS_SUBMIT_RETRY_INTERVAL_MS must be > 0");
    }
    if stop_loss_deadline_before_close_sec == 0 {
        bail!("STOP_LOSS_DEADLINE_BEFORE_CLOSE_SECONDS must be > 0");
    }
    if check_before_close_sec == 0 {
        bail!("CHECK_BEFORE_CLOSE_SECONDS must be > 0");
    }
    if idle_poll_interval_ms == 0 {
        bail!("IDLE_POLL_INTERVAL_MS must be > 0");
    }
    if market_poll_interval_ms == 0 {
        bail!("MARKET_POLL_INTERVAL_MS must be > 0");
    }
    if market_lookup_max_wait_ms == 0 {
        bail!("MARKET_LOOKUP_MAX_WAIT_MS must be > 0");
    }
    if order_retry_interval_ms == 0 {
        bail!("ORDER_RETRY_INTERVAL_MS must be > 0");
    }
    if order_max_attempts == 0 {
        bail!("ORDER_MAX_ATTEMPTS must be > 0");
    }
    if status_poll_interval_ms == 0 {
        bail!("STATUS_POLL_INTERVAL_MS must be > 0");
    }
    if live_price_max_staleness_ms == 0 {
        bail!("LIVE_PRICE_MAX_STALENESS_MS must be > 0");
    }
    if polygon_rpc_url.trim().is_empty() {
        bail!("POLYGON_RPC_URL must not be empty");
    }
    if !is_address(&chainlink_btc_usd_feed_address) {
        bail!("CHAINLINK_BTC_USD_FEED_ADDRESS must be a valid address");
    }
    if !is_address(&ctf_contract) {
        bail!("CTF_CONTRACT must be a valid address");
    }
    if !is_address(&usdc_address) {
        bail!("USDC_E must be a valid address");
    }
    if redeem_min_gas_limit < 21_000 {
        bail!("REDEEM_MIN_GAS_LIMIT must be >= 21000");
    }
    if redeem_max_priority_fee_per_gas_gwei > redeem_max_fee_per_gas_gwei {
        bail!("REDEEM_MAX_PRIORITY_FEE_PER_GAS_GWEI must be <= REDEEM_MAX_FEE_PER_GAS_GWEI");
    }
    if relayer_max_polls == 0 {
        bail!("RELAYER_MAX_POLLS must be > 0");
    }
    if relayer_request_timeout_ms == 0 {
        bail!("RELAYER_REQUEST_TIMEOUT_MS must be > 0");
    }

    if enable_live_trading {
        if private_key.is_empty() {
            bail!("Missing PRIVATE_KEY/POLYMARKET_PRIVATE_KEY when ENABLE_LIVE_TRADING=true");
        }
        if funder_address.is_empty() {
            bail!(
                "Missing FUNDER_ADDRESS/POLYMARKET_FUNDER_ADDRESS/POLYMARKET_BALANCE_ADDRESS when ENABLE_LIVE_TRADING=true"
            );
        }
        if !is_address(&funder_address) {
            bail!("FUNDER_ADDRESS must be a valid 0x address");
        }
        if api_key.trim().is_empty() {
            bail!("POLYMARKET_API_KEY is required when ENABLE_LIVE_TRADING=true");
        }
        if api_secret.trim().is_empty() {
            bail!("POLYMARKET_API_SECRET is required when ENABLE_LIVE_TRADING=true");
        }
        if api_passphrase.trim().is_empty() {
            bail!("POLYMARKET_API_PASSPHRASE is required when ENABLE_LIVE_TRADING=true");
        }
    } else if !funder_address.is_empty() && !is_address(&funder_address) {
        bail!("FUNDER_ADDRESS must be a valid 0x address");
    }

    if settlement_tx_mode == SettlementTxMode::RelayerSafe && on_chain_auto_claim_enabled {
        if relayer_api_key.is_none() {
            bail!("RELAYER_API_KEY is required when SETTLEMENT_TX_MODE=RELAYER_SAFE");
        }
        if relayer_api_key_address.is_none() {
            bail!("RELAYER_API_KEY_ADDRESS is required when SETTLEMENT_TX_MODE=RELAYER_SAFE");
        }
    }

    Ok(Config {
        once,
        debug,
        heartbeat_interval_sec,
        silent_watchdog_sec,
        enable_live_trading,
        stake_usd,
        price_range_min,
        price_range_max,
        entry_price_gate_enabled,
        entry_price_retry_interval_ms,
        entry_price_max_retries,
        entry_slippage_percent_buy,
        enable_fallback_gtc_limit,
        enable_stop_loss,
        stop_loss_price_trigger,
        interval_check_price_trigger_ms,
        retry_sell,
        stop_loss_timeout_sec,
        stop_loss_order_type,
        stop_loss_submit_retry_interval_ms,
        stop_loss_deadline_before_close_sec,
        check_before_close_sec,
        resolve_delay_sec,
        idle_poll_interval_ms,
        market_poll_interval_ms,
        market_lookup_max_wait_ms,
        order_retry_interval_ms,
        order_max_attempts,
        status_poll_interval_ms,
        status_poll_grace_sec,
        polymarket_clob_url,
        polymarket_gamma_url,
        binance_base_url,
        live_price_source,
        chainlink_btc_usd_feed_address,
        live_price_max_staleness_ms,
        private_key,
        funder_address,
        signature_type,
        api_key,
        api_secret,
        api_passphrase,
        telegram_bot_token,
        telegram_chat_id,
        on_chain_auto_claim_enabled,
        settlement_tx_mode,
        relayer_base_url,
        relayer_api_key,
        relayer_api_key_address,
        relayer_request_timeout_ms,
        relayer_poll_interval_ms,
        relayer_max_polls,
        relayer_allow_fallback_to_direct,
        settlement_max_attempts,
        settlement_retry_delay_ms,
        enable_gamma_resolution_fallback,
        redeem_gas_limit_multiplier,
        redeem_min_gas_limit,
        redeem_max_fee_per_gas_gwei,
        redeem_max_priority_fee_per_gas_gwei,
        redeem_internal_max_attempts,
        redeem_internal_retry_base_delay_ms,
        redeem_internal_retry_backoff_multiplier,
        redeem_tx_confirm_timeout_ms,
        polygon_rpc_url,
        ctf_contract,
        usdc_address,
        output_path,
        trades_output_path,
        loss_cooldown_minutes,
        total_loss_trades,
    })
}
