use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Mutex;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use ethers::abi::{Abi, AbiParser};
use ethers::contract::{Contract, ContractCall};
use ethers::middleware::SignerMiddleware;
use ethers::providers::{Http, Middleware, Provider};
use ethers::signers::{LocalWallet, Signer};
use ethers::types::{Address, H256, TxHash, U256};
use ethers::utils::parse_units;
use once_cell::sync::Lazy;
use tokio::time::{sleep, Instant};

use crate::account::polygon_provider::get_working_polygon_provider;
use crate::account::relayer_service::{relay_redeem_positions, should_use_relayer_for_settlement};
use crate::data::market_discovery::probe_gamma_resolution_by_slug;
use crate::types::{
    ClaimProcessingResult, Config, ExecutionResult, MarketResolutionSource, MarketSide, TradeResult,
};
use crate::utils::logger::{log_info, log_warn};

const RESOLUTION_CACHE_TTL_MS: u64 = 10 * 60 * 1000;
const CONFIRMATION_POLL_INTERVAL_MS: u64 = 1500;

static CTF_ABI: Lazy<Abi> = Lazy::new(|| {
    AbiParser::default()
        .parse(&[
            "function redeemPositions(address collateralToken, bytes32 parentCollectionId, bytes32 conditionId, uint256[] indexSets)",
            "function balanceOf(address account, uint256 id) view returns (uint256)",
        ])
        .expect("valid CTF ABI")
});

#[derive(Debug, Clone, Copy)]
struct ResolutionCacheEntry {
    timestamp_ms: u64,
    resolved: bool,
}

static RESOLUTION_CACHE: Lazy<Mutex<HashMap<String, ResolutionCacheEntry>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

#[derive(Debug, Clone)]
pub struct TradePnlResult {
    pub outcome: TradeResult,
    pub redeemed_usd: f64,
    pub pnl_usd: f64,
}

#[derive(Debug, Clone)]
struct RedeemAttemptResult {
    completed: bool,
    tx_hash: Option<String>,
    error: Option<String>,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn is_bytes32(value: &str) -> bool {
    let trimmed = value.trim();
    trimmed.len() == 66
        && trimmed.starts_with("0x")
        && trimmed.chars().skip(2).all(|c| c.is_ascii_hexdigit())
}

fn parse_address(label: &str, value: &str) -> Result<Address> {
    let trimmed = value.trim();
    Address::from_str(trimmed).with_context(|| format!("Invalid {label}: {trimmed}"))
}

fn parse_condition_id(condition_id: &str) -> Result<H256> {
    let trimmed = condition_id.trim();
    H256::from_str(trimmed).with_context(|| format!("Invalid conditionId: {trimmed}"))
}

fn parse_token_id(token_id: &str) -> Result<U256> {
    let trimmed = token_id.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("empty token id"));
    }

    if let Some(hex) = trimmed.strip_prefix("0x") {
        return U256::from_str_radix(hex, 16)
            .with_context(|| format!("Invalid hex token id: {trimmed}"));
    }

    U256::from_dec_str(trimmed).with_context(|| format!("Invalid decimal token id: {trimmed}"))
}

fn classify_redeem_error(message: &str) -> (String, bool) {
    let lower = message.to_lowercase();

    if lower.contains("relayer") {
        return (format!("RELAYER_ERROR: {message}"), true);
    }

    if lower.contains("out of gas") || lower.contains("gas") {
        return (format!("OUT_OF_GAS: {message}"), true);
    }

    if lower.contains("timeout") {
        return (format!("RPC_TIMEOUT: {message}"), true);
    }

    if lower.contains("network") || lower.contains("temporarily unavailable") {
        return (format!("NETWORK_ERROR: {message}"), true);
    }

    if lower.contains("nonce") {
        return (format!("NONCE_ERROR: {message}"), true);
    }

    if lower.contains("balance") || lower.contains("insufficient") {
        return (format!("INSUFFICIENT_BALANCE: {message}"), false);
    }

    if lower.contains("reverted") {
        return (format!("TX_REVERTED: {message}"), true);
    }

    (message.to_owned(), false)
}

fn gwei_to_wei(value_gwei: f64) -> Result<U256> {
    let normalized = format!("{value_gwei:.9}");
    let parsed = parse_units(normalized, 9).context("failed to parse gwei to wei")?;
    Ok(parsed.into())
}

async fn wait_for_tx_confirmation_with_timeout(
    provider: &Provider<Http>,
    tx_hash: TxHash,
    timeout_ms: u64,
) -> Result<String> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);

    loop {
        if let Some(receipt) = provider
            .get_transaction_receipt(tx_hash)
            .await
            .context("failed to poll transaction receipt")?
        {
            if receipt.status == Some(0_u64.into()) {
                return Err(anyhow!("TX_REVERTED: tx={:#x}", tx_hash));
            }

            return Ok(format!("{:#x}", receipt.transaction_hash));
        }

        if Instant::now() >= deadline {
            break;
        }

        sleep(Duration::from_millis(CONFIRMATION_POLL_INTERVAL_MS)).await;
    }

    if let Some(receipt) = provider
        .get_transaction_receipt(tx_hash)
        .await
        .context("failed to fetch transaction receipt after timeout")?
    {
        if receipt.status == Some(0_u64.into()) {
            return Err(anyhow!("TX_REVERTED: tx={:#x}", tx_hash));
        }

        return Ok(format!("{:#x}", receipt.transaction_hash));
    }

    Err(anyhow!(
        "TX_CONFIRMATION_TIMEOUT: tx={:#x}, timeout={}ms",
        tx_hash,
        timeout_ms
    ))
}

async fn build_redeem_call_with_overrides(
    ctf: &Contract<SignerMiddleware<Provider<Http>, LocalWallet>>,
    collateral_token: Address,
    condition_id: H256,
    config: &Config,
) -> Result<ContractCall<SignerMiddleware<Provider<Http>, LocalWallet>, ()>> {
    let params = (
        collateral_token,
        H256::zero(),
        condition_id,
        vec![U256::from(1_u64), U256::from(2_u64)],
    );

    let min_gas_limit = U256::from(config.redeem_min_gas_limit.max(21_000));
    let multiplier_percent = (config.redeem_gas_limit_multiplier * 100.0)
        .round()
        .max(100.0) as u64;

    let estimate_call = ctf
        .method::<_, ()>("redeemPositions", params.clone())
        .context("failed to build redeemPositions estimate call")?;

    let gas_limit = match estimate_call.estimate_gas().await {
        Ok(estimate) => {
            let buffered = ((estimate * U256::from(multiplier_percent)) + U256::from(99_u64))
                / U256::from(100_u64);
            let applied = if buffered < min_gas_limit {
                min_gas_limit
            } else {
                buffered
            };

            log_info(
                "Settlement",
                &format!(
                    "redeem gas estimate={} gasLimit={}",
                    estimate,
                    applied
                ),
            );

            applied
        }
        Err(error) => {
            log_warn(
                "Settlement",
                &format!(
                    "redeem gas estimation failed, fallback gas={} error={}",
                    min_gas_limit,
                    error
                ),
            );
            min_gas_limit
        }
    };

    let mut call = ctf
        .method::<_, ()>("redeemPositions", params)
        .context("failed to build redeemPositions call")?
        .gas(gas_limit);

    if config.redeem_max_fee_per_gas_gwei > 0.0 {
        call = call.gas_price(gwei_to_wei(config.redeem_max_fee_per_gas_gwei)?);
    }

    Ok(call)
}

async fn has_zero_redeemable_balance(
    config: &Config,
    yes_token_id: &str,
    no_token_id: &str,
) -> Result<bool> {
    let yes_trimmed = yes_token_id.trim();
    let no_trimmed = no_token_id.trim();
    if yes_trimmed.is_empty() || no_trimmed.is_empty() {
        return Ok(false);
    }

    let provider = get_working_polygon_provider(config).await?;
    let owner = parse_address("FUNDER_ADDRESS", &config.funder_address)?;
    let ctf_contract = parse_address("CTF_CONTRACT", &config.ctf_contract)?;
    let yes_id = parse_token_id(yes_trimmed)?;
    let no_id = parse_token_id(no_trimmed)?;
    let ctf = Contract::new(ctf_contract, CTF_ABI.clone(), Arc::new(provider));

    let yes_balance: U256 = ctf
        .method::<_, U256>("balanceOf", (owner, yes_id))
        .context("failed to build balanceOf call for YES token")?
        .call()
        .await
        .context("failed to query YES token balance")?;

    let no_balance: U256 = ctf
        .method::<_, U256>("balanceOf", (owner, no_id))
        .context("failed to build balanceOf call for NO token")?
        .call()
        .await
        .context("failed to query NO token balance")?;

    Ok(yes_balance.is_zero() && no_balance.is_zero())
}

async fn redeem_direct(config: &Config, condition_id: &str) -> Result<String> {
    let provider = get_working_polygon_provider(config).await?;
    let wallet = LocalWallet::from_str(config.private_key.trim())
        .context("invalid PRIVATE_KEY format")?
        .with_chain_id(137_u64);
    let signer_client = Arc::new(SignerMiddleware::new(provider.clone(), wallet));

    let ctf_contract = parse_address("CTF_CONTRACT", &config.ctf_contract)?;
    let collateral_token = parse_address("USDC_E", &config.usdc_address)?;
    let condition = parse_condition_id(condition_id)?;

    let ctf = Contract::new(ctf_contract, CTF_ABI.clone(), signer_client);
    let call = build_redeem_call_with_overrides(&ctf, collateral_token, condition, config).await?;
    let pending_tx = call.send().await.context("failed to submit redeem transaction")?;
    let tx_hash = pending_tx.tx_hash();

    wait_for_tx_confirmation_with_timeout(&provider, tx_hash, config.redeem_tx_confirm_timeout_ms)
        .await
}

async fn attempt_redeem_positions(
    config: &Config,
    condition_id: &str,
    yes_token_id: &str,
    no_token_id: &str,
) -> RedeemAttemptResult {
    if !is_bytes32(condition_id) {
        return RedeemAttemptResult {
            completed: false,
            tx_hash: None,
            error: Some(format!("Invalid conditionId for redeem: {condition_id}")),
        };
    }

    let max_attempts = config.redeem_internal_max_attempts.max(1);
    for attempt in 1..=max_attempts {
        let result: Result<RedeemAttemptResult> = async {
            if has_zero_redeemable_balance(config, yes_token_id, no_token_id).await? {
                log_info(
                    "Settlement",
                    &format!("no YES/NO balance to redeem for condition {condition_id}"),
                );

                return Ok(RedeemAttemptResult {
                    completed: true,
                    tx_hash: None,
                    error: None,
                });
            }

            if should_use_relayer_for_settlement(config) {
                match relay_redeem_positions(config, condition_id).await {
                    Ok(tx_hash) => {
                        return Ok(RedeemAttemptResult {
                            completed: true,
                            tx_hash: Some(tx_hash),
                            error: None,
                        });
                    }
                    Err(error) => {
                        if !config.relayer_allow_fallback_to_direct {
                            return Err(anyhow!("RELAYER_REDEEM_FAILED: {error}"));
                        }

                        log_warn(
                            "Settlement",
                            &format!(
                                "relayer redeem failed, fallback to direct for {}: {}",
                                condition_id, error
                            ),
                        );
                    }
                }
            }

            let tx_hash = redeem_direct(config, condition_id).await?;
            Ok(RedeemAttemptResult {
                completed: true,
                tx_hash: Some(tx_hash),
                error: None,
            })
        }
        .await;

        match result {
            Ok(success) => return success,
            Err(error) => {
                let message = error.to_string();
                let (classified_error, retryable) = classify_redeem_error(&message);
                let can_retry = retryable && attempt < max_attempts;

                log_warn(
                    "Settlement",
                    &format!(
                        "redeem attempt {}/{} failed: {}",
                        attempt, max_attempts, classified_error
                    ),
                );

                if can_retry {
                    let exponent = (attempt - 1) as i32;
                    let delay = (config.redeem_internal_retry_base_delay_ms as f64)
                        * config
                            .redeem_internal_retry_backoff_multiplier
                            .powi(exponent);
                    if delay.is_finite() && delay > 0.0 {
                        sleep(Duration::from_millis(delay.round() as u64)).await;
                    }
                    continue;
                }

                return RedeemAttemptResult {
                    completed: false,
                    tx_hash: None,
                    error: Some(classified_error),
                };
            }
        }
    }

    RedeemAttemptResult {
        completed: false,
        tx_hash: None,
        error: Some("redeem failed: exhausted internal attempts".to_owned()),
    }
}

pub fn cleanup_resolution_cache() {
    if let Ok(mut cache) = RESOLUTION_CACHE.lock() {
        let now = now_ms();
        cache.retain(|_, entry| now.saturating_sub(entry.timestamp_ms) <= RESOLUTION_CACHE_TTL_MS);
    }
}

pub fn compute_trade_pnl(
    side: MarketSide,
    execution: &ExecutionResult,
    resolved_outcome: MarketSide,
) -> TradePnlResult {
    let is_win = side == resolved_outcome;

    if is_win {
        let redeemed_usd = execution.filled_size.max(0.0);
        let pnl_usd = redeemed_usd - execution.spent_usd;
        TradePnlResult {
            outcome: TradeResult::Win,
            redeemed_usd,
            pnl_usd,
        }
    } else {
        TradePnlResult {
            outcome: TradeResult::Loss,
            redeemed_usd: 0.0,
            pnl_usd: -execution.spent_usd,
        }
    }
}

pub async fn process_pending_claim(
    config: &Config,
    _trade_id: &str,
    window_slug: &str,
    condition_id: &str,
    yes_token_id: &str,
    no_token_id: &str,
) -> Result<ClaimProcessingResult> {
    if !config.on_chain_auto_claim_enabled {
        return Ok(ClaimProcessingResult {
            completed: false,
            tx_hash: None,
            market_resolved: false,
            resolution_source: Some(MarketResolutionSource::Polling),
            market_resolved_at_ms: None,
            error: Some("auto-claim disabled".to_owned()),
        });
    }

    if config.private_key.trim().is_empty() {
        return Ok(ClaimProcessingResult {
            completed: false,
            tx_hash: None,
            market_resolved: false,
            resolution_source: Some(MarketResolutionSource::Polling),
            market_resolved_at_ms: None,
            error: Some("PRIVATE_KEY is required for on-chain auto claim".to_owned()),
        });
    }

    if !is_bytes32(condition_id) {
        return Ok(ClaimProcessingResult {
            completed: false,
            tx_hash: None,
            market_resolved: false,
            resolution_source: Some(MarketResolutionSource::Polling),
            market_resolved_at_ms: None,
            error: Some(format!("Invalid conditionId: {condition_id}")),
        });
    }

    let slug = window_slug.trim();
    if slug.is_empty() && config.enable_gamma_resolution_fallback {
        return Ok(ClaimProcessingResult {
            completed: false,
            tx_hash: None,
            market_resolved: false,
            resolution_source: Some(MarketResolutionSource::Polling),
            market_resolved_at_ms: None,
            error: Some("window slug is required for claim resolution".to_owned()),
        });
    }

    cleanup_resolution_cache();

    let condition_key = condition_id.trim().to_lowercase();
    let mut resolved = false;
    let mut resolution_source = Some(MarketResolutionSource::Polling);
    let mut resolved_at_ms: Option<u64> = None;

    if let Ok(cache) = RESOLUTION_CACHE.lock() {
        if let Some(entry) = cache.get(&condition_key) {
            resolved = entry.resolved;
            resolution_source = Some(MarketResolutionSource::Cached);
            resolved_at_ms = Some(entry.timestamp_ms);
        }
    }

    if !resolved {
        if !config.enable_gamma_resolution_fallback {
            return Ok(ClaimProcessingResult {
                completed: false,
                tx_hash: None,
                market_resolved: false,
                resolution_source,
                market_resolved_at_ms: None,
                error: Some(
                    "market resolution fallback disabled (ENABLE_GAMMA_RESOLUTION_FALLBACK=false)"
                        .to_owned(),
                ),
            });
        }

        let probe = probe_gamma_resolution_by_slug(config, slug, condition_id).await?;
        if !probe.resolved {
            return Ok(ClaimProcessingResult {
                completed: false,
                tx_hash: None,
                market_resolved: false,
                resolution_source: Some(MarketResolutionSource::GammaFallback),
                market_resolved_at_ms: None,
                error: probe.error.or_else(|| Some("market unresolved".to_owned())),
            });
        }

        let now = now_ms();
        if let Ok(mut cache) = RESOLUTION_CACHE.lock() {
            cache.insert(
                condition_key,
                ResolutionCacheEntry {
                    timestamp_ms: now,
                    resolved: true,
                },
            );
        }

        resolved = true;
        resolution_source = Some(MarketResolutionSource::GammaFallback);
        resolved_at_ms = Some(now);
    }

    if !resolved {
        return Ok(ClaimProcessingResult {
            completed: false,
            tx_hash: None,
            market_resolved: false,
            resolution_source,
            market_resolved_at_ms: resolved_at_ms,
            error: Some("market unresolved".to_owned()),
        });
    }

    let redeem = attempt_redeem_positions(config, condition_id, yes_token_id, no_token_id).await;
    if !redeem.completed {
        return Ok(ClaimProcessingResult {
            completed: false,
            tx_hash: None,
            market_resolved: true,
            resolution_source,
            market_resolved_at_ms: resolved_at_ms,
            error: redeem
                .error
                .or_else(|| Some("redeem failed: unknown error".to_owned())),
        });
    }

    Ok(ClaimProcessingResult {
        completed: true,
        tx_hash: redeem.tx_hash,
        market_resolved: true,
        resolution_source,
        market_resolved_at_ms: resolved_at_ms,
        error: None,
    })
}
