use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum MarketSide {
    #[serde(rename = "UP")]
    Up,
    #[serde(rename = "DOWN")]
    Down,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum TradeResult {
    #[serde(rename = "WIN")]
    Win,
    #[serde(rename = "LOSS")]
    Loss,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum LivePriceSource {
    #[serde(rename = "BINANCE")]
    Binance,
    #[serde(rename = "CHAINLINK_PUBLIC")]
    ChainlinkPublic,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum SettlementTxMode {
    #[serde(rename = "DIRECT_ETHERS")]
    DirectEthers,
    #[serde(rename = "RELAYER_SAFE")]
    RelayerSafe,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ClaimStatus {
    #[serde(rename = "PENDING")]
    Pending,
    #[serde(rename = "SUCCESS")]
    Success,
    #[serde(rename = "FAILED")]
    Failed,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MarketResolutionSource {
    Polling,
    Cached,
    GammaFallback,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ExecutionStatus {
    #[serde(rename = "PENDING")]
    Pending,
    #[serde(rename = "FILLED")]
    Filled,
    #[serde(rename = "PARTIAL")]
    Partial,
    #[serde(rename = "CANCELLED")]
    Cancelled,
    #[serde(rename = "FAILED")]
    Failed,
    #[serde(rename = "SKIPPED")]
    Skipped,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum SettlementSource {
    #[serde(rename = "BINANCE")]
    Binance,
    #[serde(rename = "POLYMARKET")]
    Polymarket,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ResultOrderStatus {
    #[serde(rename = "PENDING")]
    Pending,
    #[serde(rename = "FILLED")]
    Filled,
    #[serde(rename = "PARTIAL")]
    Partial,
    #[serde(rename = "CANCELLED")]
    Cancelled,
    #[serde(rename = "FAILED")]
    Failed,
    #[serde(rename = "SKIPPED")]
    Skipped,
    #[serde(rename = "NOT_EXECUTED")]
    NotExecuted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureType {
    Eoa,
    Safe,
    SmartContractWallet,
}

impl SignatureType {
    pub fn as_u8(self) -> u8 {
        match self {
            Self::Eoa => 0,
            Self::Safe => 1,
            Self::SmartContractWallet => 2,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketWindow {
    pub window_start_sec: u64,
    pub close_time_sec: u64,
    pub slug: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredMarket {
    pub slug: String,
    pub condition_id: String,
    pub yes_token_id: String,
    pub no_token_id: String,
    pub question: String,
    pub yes_price: Option<f64>,
    pub no_price: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderBookSnapshot {
    pub best_bid: f64,
    pub best_ask: f64,
    pub asks_present: bool,
    pub ask_levels: usize,
    pub parsed_ask_levels: usize,
    pub min_order_size: f64,
    pub tick_size: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SettlementOutcome {
    pub outcome: MarketSide,
    pub open_price: f64,
    pub close_price: f64,
    pub source: SettlementSource,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionResult {
    pub status: ExecutionStatus,
    pub order_id: String,
    pub filled_price: f64,
    pub filled_size: f64,
    pub spent_usd: f64,
    pub used_fallback_limit: bool,
    pub raw_response: HashMap<String, Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaimProcessingResult {
    pub completed: bool,
    pub tx_hash: Option<String>,
    pub market_resolved: bool,
    pub resolution_source: Option<MarketResolutionSource>,
    pub market_resolved_at_ms: Option<u64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingClaim {
    pub trade_id: String,
    pub window_slug: String,
    pub condition_id: String,
    pub yes_token_id: String,
    pub no_token_id: String,
    pub created_at_ms: u64,
    pub poll_count: u64,
    pub claim_attempts: u64,
    pub last_update_ms: u64,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TradeRecord {
    pub id: String,
    pub timestamp_ms: u64,
    pub mode: String,
    pub window: MarketWindow,
    pub market: DiscoveredMarket,
    pub side: MarketSide,
    pub selected_ask_price: f64,
    pub selected_bid_price: f64,
    pub stake_usd: f64,
    pub execution: ExecutionResult,
    pub settlement: SettlementOutcome,
    pub outcome: TradeResult,
    pub redeemed_usd: f64,
    pub pnl_usd: f64,
    pub claim_status: Option<ClaimStatus>,
    pub claim_attempts: Option<u64>,
    pub claim_tx_hash: Option<String>,
    pub claim_last_error: Option<String>,
    pub claim_updated_at_ms: Option<u64>,
    pub market_resolved: Option<bool>,
    pub market_resolution_source: Option<MarketResolutionSource>,
    pub market_resolved_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResultRow {
    pub market: String,
    pub time: String,
    pub side: MarketSide,
    pub result: TradeResult,
    pub bid_price: f64,
    pub mode: Option<String>,
    pub ask_price: Option<f64>,
    pub order_status: Option<ResultOrderStatus>,
    pub order_id: Option<String>,
    pub filled_price: Option<f64>,
    pub filled_size: Option<f64>,
    pub spent_usd: Option<f64>,
    pub redeemed_usd: Option<f64>,
    pub pnl_usd: Option<f64>,
    pub settlement_source: Option<SettlementSource>,
    pub claim_status: Option<ClaimStatus>,
    pub claim_tx_hash: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectedSideSnapshot {
    pub side: MarketSide,
    pub time: String,
    pub ask_price: f64,
    pub bid_price: f64,
}

#[derive(Debug, Clone)]
pub struct Config {
    pub once: bool,
    pub debug: bool,
    pub heartbeat_interval_sec: u64,
    pub silent_watchdog_sec: u64,
    pub enable_live_trading: bool,
    pub stake_usd: f64,
    pub price_range_min: f64,
    pub price_range_max: f64,
    pub entry_price_gate_enabled: bool,
    pub entry_price_retry_interval_ms: u64,
    pub entry_price_max_retries: u64,
    pub entry_slippage_percent_buy: f64,
    pub enable_fallback_gtc_limit: bool,
    pub enable_stop_loss: bool,
    pub stop_loss_price_trigger: f64,
    pub interval_check_price_trigger_ms: u64,
    pub retry_sell: u64,
    pub stop_loss_timeout_sec: u64,
    pub stop_loss_order_type: String,
    pub stop_loss_submit_retry_interval_ms: u64,
    pub stop_loss_deadline_before_close_sec: u64,
    pub check_before_close_sec: u64,
    pub resolve_delay_sec: u64,
    pub idle_poll_interval_ms: u64,
    pub market_poll_interval_ms: u64,
    pub market_lookup_max_wait_ms: u64,
    pub order_retry_interval_ms: u64,
    pub order_max_attempts: u64,
    pub status_poll_interval_ms: u64,
    pub status_poll_grace_sec: u64,
    pub polymarket_clob_url: String,
    pub polymarket_gamma_url: String,
    pub binance_base_url: String,
    pub live_price_source: LivePriceSource,
    pub chainlink_btc_usd_feed_address: String,
    pub live_price_max_staleness_ms: u64,
    pub private_key: String,
    pub funder_address: String,
    pub signature_type: SignatureType,
    pub api_key: String,
    pub api_secret: String,
    pub api_passphrase: String,
    pub telegram_bot_token: Option<String>,
    pub telegram_chat_id: Option<String>,
    pub on_chain_auto_claim_enabled: bool,
    pub settlement_tx_mode: SettlementTxMode,
    pub relayer_base_url: String,
    pub relayer_api_key: Option<String>,
    pub relayer_api_key_address: Option<String>,
    pub relayer_request_timeout_ms: u64,
    pub relayer_poll_interval_ms: u64,
    pub relayer_max_polls: u64,
    pub relayer_allow_fallback_to_direct: bool,
    pub settlement_max_attempts: u64,
    pub settlement_retry_delay_ms: u64,
    pub enable_gamma_resolution_fallback: bool,
    pub redeem_gas_limit_multiplier: f64,
    pub redeem_min_gas_limit: u64,
    pub redeem_max_fee_per_gas_gwei: f64,
    pub redeem_max_priority_fee_per_gas_gwei: f64,
    pub redeem_internal_max_attempts: u64,
    pub redeem_internal_retry_base_delay_ms: u64,
    pub redeem_internal_retry_backoff_multiplier: f64,
    pub redeem_tx_confirm_timeout_ms: u64,
    pub polygon_rpc_url: String,
    pub ctf_contract: String,
    pub usdc_address: String,
    pub output_path: PathBuf,
    pub trades_output_path: PathBuf,
    pub loss_cooldown_minutes: u64,
    pub total_loss_trades: u64,
}

impl Config {
    pub fn mode(&self) -> &'static str {
        if self.enable_live_trading {
            "LIVE"
        } else {
            "PAPER"
        }
    }
}
