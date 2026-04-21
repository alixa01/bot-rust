use std::str::FromStr;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine as _;
use chrono::Utc;
use ethers::signers::{LocalWallet, Signer};
use ethers::types::transaction::eip712::TypedData;
use ethers::types::{Address, U256};
use ethers::utils::parse_units;
use hmac::{Hmac, Mac};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::Client;
use serde_json::{json, Map, Value};
use sha2::Sha256;

use crate::types::{Config, SignatureType};

type HmacSha256 = Hmac<Sha256>;

const CHAIN_ID_POLYGON: u64 = 137;
const ZERO_ADDRESS: &str = "0x0000000000000000000000000000000000000000";
const EXCHANGE_POLYGON: &str = "0x4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E";
const NEG_RISK_EXCHANGE_POLYGON: &str = "0xC5d563A36AE78145C45a50134d48A1215220f80a";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderSide {
    Buy,
    Sell,
}

impl OrderSide {
    fn as_u8(self) -> u8 {
        match self {
            Self::Buy => 0,
            Self::Sell => 1,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Buy => "BUY",
            Self::Sell => "SELL",
        }
    }
}

#[derive(Debug, Clone)]
pub struct SignedOrder {
    pub salt: String,
    pub maker: String,
    pub signer: String,
    pub taker: String,
    pub token_id: String,
    pub maker_amount: String,
    pub taker_amount: String,
    pub expiration: String,
    pub nonce: String,
    pub fee_rate_bps: String,
    pub side: OrderSide,
    pub signature_type: u8,
    pub signature: String,
}

#[derive(Debug, Clone)]
pub struct ConditionalBalanceAllowance {
    pub balance_raw: String,
    pub allowance_raw: String,
    pub balance_units: U256,
    pub allowance_units: U256,
}

impl ConditionalBalanceAllowance {
    pub fn available_units(&self) -> U256 {
        if self.balance_units < self.allowance_units {
            self.balance_units
        } else {
            self.allowance_units
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct RoundingConfig {
    price_decimals: u32,
    size_decimals: u32,
    amount_decimals: u32,
}

#[derive(Debug, Clone)]
pub struct ClobClient {
    host: String,
    wallet: LocalWallet,
    signer_address: String,
    funder_address: String,
    signature_type: u8,
    api_key: String,
    api_secret: String,
    api_passphrase: String,
    http_client: Client,
}

impl ClobClient {
    pub async fn get_tick_size(&self, token_id: &str) -> Result<String> {
        let endpoint = "/tick-size";
        let response = self
            .http_client
            .get(format!("{}{}", self.host, endpoint))
            .query(&[("token_id", token_id)])
            .send()
            .await
            .context("failed to fetch tick size")?;

        let payload: Value = response
            .json()
            .await
            .context("failed to parse tick size payload")?;

        if let Some(message) = payload.get("error").and_then(Value::as_str) {
            bail!("tick size endpoint returned error: {message}");
        }

        let tick = payload
            .get("minimum_tick_size")
            .and_then(value_to_f64)
            .filter(|value| value.is_finite() && *value > 0.0)
            .ok_or_else(|| anyhow!("tick size payload missing minimum_tick_size"))?;

        Ok(normalize_tick_size_string(tick))
    }

    pub async fn get_fee_rate_bps(&self, token_id: &str) -> Result<u64> {
        let endpoint = "/fee-rate";
        let response = self
            .http_client
            .get(format!("{}{}", self.host, endpoint))
            .query(&[("token_id", token_id)])
            .send()
            .await
            .context("failed to fetch fee rate")?;

        let payload: Value = response
            .json()
            .await
            .context("failed to parse fee rate payload")?;

        if let Some(message) = payload.get("error").and_then(Value::as_str) {
            bail!("fee rate endpoint returned error: {message}");
        }

        let fee_rate = payload
            .get("base_fee")
            .and_then(value_to_u64)
            .ok_or_else(|| anyhow!("fee rate payload missing base_fee"))?;

        Ok(fee_rate)
    }

    pub async fn get_neg_risk(&self, token_id: &str) -> Result<bool> {
        let endpoint = "/neg-risk";
        let response = self
            .http_client
            .get(format!("{}{}", self.host, endpoint))
            .query(&[("token_id", token_id)])
            .send()
            .await
            .context("failed to fetch neg-risk flag")?;

        let payload: Value = response
            .json()
            .await
            .context("failed to parse neg-risk payload")?;

        if let Some(message) = payload.get("error").and_then(Value::as_str) {
            bail!("neg-risk endpoint returned error: {message}");
        }

        payload
            .get("neg_risk")
            .and_then(Value::as_bool)
            .ok_or_else(|| anyhow!("neg-risk payload missing neg_risk"))
    }

    pub async fn get_order(&self, order_id: &str) -> Result<Option<Value>> {
        let request_path = format!("/data/order/{order_id}");
        let headers = self.build_l2_headers("GET", &request_path, None)?;

        let response = self
            .http_client
            .get(format!("{}{}", self.host, request_path))
            .headers(headers)
            .send()
            .await
            .with_context(|| format!("failed to fetch order status for {order_id}"))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_else(|_| String::new());

            return Ok(Some(json!({
                "source": "get_order_http_error",
                "orderId": order_id,
                "httpStatus": status.as_u16(),
                "body": truncate_text(&body, 220),
            })));
        }

        let payload: Value = response
            .json()
            .await
            .with_context(|| format!("failed to parse order status response for {order_id}"))?;

        Ok(Some(payload))
    }

    pub async fn get_conditional_balance_allowance(
        &self,
        token_id: &str,
    ) -> Result<ConditionalBalanceAllowance> {
        let request_path = "/balance-allowance";
        let headers = self.build_l2_headers("GET", request_path, None)?;
        let signature_type = self.signature_type.to_string();

        let response = self
            .http_client
            .get(format!("{}{}", self.host, request_path))
            .headers(headers)
            .query(&[
                ("asset_type", "CONDITIONAL"),
                ("token_id", token_id),
                ("signature_type", signature_type.as_str()),
            ])
            .send()
            .await
            .with_context(|| {
                format!(
                    "failed to fetch balance-allowance for conditional token {}",
                    token_id
                )
            })?;

        let status = response.status();
        let text = response
            .text()
            .await
            .context("failed to read balance-allowance response body")?;

        if !status.is_success() {
            bail!(
                "balance-allowance failed with HTTP {} body={} tokenId={}",
                status.as_u16(),
                truncate_text(&text, 220),
                token_id
            );
        }

        let payload: Value = serde_json::from_str(&text).with_context(|| {
            format!(
                "failed to parse balance-allowance payload for token {} body={}",
                token_id,
                truncate_text(&text, 220)
            )
        })?;

        if let Some(message) = payload.get("error").and_then(Value::as_str) {
            bail!(
                "balance-allowance endpoint returned error for token {}: {}",
                token_id,
                message
            );
        }

        let (balance_raw, allowance_raw) =
            parse_balance_allowance_payload(&payload).with_context(|| {
                format!(
                    "unexpected balance-allowance payload shape token={} payload={}",
                    token_id,
                    truncate_text(&payload.to_string(), 220)
                )
            })?;

        let balance_units = parse_balance_allowance_units("balance", &balance_raw)?;
        let allowance_units = parse_balance_allowance_units("allowance", &allowance_raw)?;

        Ok(ConditionalBalanceAllowance {
            balance_raw,
            allowance_raw,
            balance_units,
            allowance_units,
        })
    }

    pub async fn post_order(
        &self,
        order: &SignedOrder,
        order_type: &str,
        defer_exec: bool,
        post_only: Option<bool>,
    ) -> Result<Value> {
        let mut payload = json!({
            "deferExec": defer_exec,
            "order": {
                "salt": order.salt.parse::<u128>().unwrap_or(0),
                "maker": order.maker,
                "signer": order.signer,
                "taker": order.taker,
                "tokenId": order.token_id,
                "makerAmount": order.maker_amount,
                "takerAmount": order.taker_amount,
                "side": order.side.as_str(),
                "expiration": order.expiration,
                "nonce": order.nonce,
                "feeRateBps": order.fee_rate_bps,
                "signatureType": order.signature_type,
                "signature": order.signature,
            },
            "owner": self.api_key,
            "orderType": order_type,
        });

        if let Some(flag) = post_only {
            payload["postOnly"] = json!(flag);
        }

        let request_path = "/order";
        let body = serde_json::to_string(&payload).context("failed to serialize order payload")?;
        let headers = self.build_l2_headers("POST", request_path, Some(&body))?;

        let response = self
            .http_client
            .post(format!("{}{}", self.host, request_path))
            .headers(headers)
            .header("Content-Type", "application/json")
            .body(body)
            .send()
            .await
            .context("failed to post order")?;

        let status = response.status();
        let text = response
            .text()
            .await
            .context("failed to read post order response body")?;

        if text.trim().is_empty() {
            if status.is_success() {
                return Ok(Value::Null);
            }

            bail!("post order failed with HTTP {status} and empty response body");
        }

        let parsed: Value = serde_json::from_str(&text).with_context(|| {
            format!("failed to parse post order response (status={status}, body={text})")
        })?;

        Ok(parsed)
    }

    pub async fn create_market_order_buy(
        &self,
        token_id: &str,
        amount_usd: f64,
        price: f64,
        tick_size: &str,
    ) -> Result<SignedOrder> {
        let round = round_config_for_tick(tick_size)?;
        let fee_rate_bps = self.get_fee_rate_bps(token_id).await?;
        let neg_risk = self.get_neg_risk(token_id).await?;

        let (side, raw_maker_amount, raw_taker_amount) =
            get_market_order_raw_amounts(OrderSide::Buy, amount_usd, price, round)?;

        self.create_signed_order(SignOrderParams {
            token_id,
            side,
            raw_maker_amount,
            raw_taker_amount,
            fee_rate_bps,
            expiration: 0,
            nonce: 0,
            neg_risk,
        })
        .await
    }

    pub async fn create_market_order_sell(
        &self,
        token_id: &str,
        size_shares: f64,
        price: f64,
        tick_size: &str,
    ) -> Result<SignedOrder> {
        let round = round_config_for_tick(tick_size)?;
        let fee_rate_bps = self.get_fee_rate_bps(token_id).await?;
        let neg_risk = self.get_neg_risk(token_id).await?;

        let (side, raw_maker_amount, raw_taker_amount) =
            get_market_order_raw_amounts(OrderSide::Sell, size_shares, price, round)?;

        self.create_signed_order(SignOrderParams {
            token_id,
            side,
            raw_maker_amount,
            raw_taker_amount,
            fee_rate_bps,
            expiration: 0,
            nonce: 0,
            neg_risk,
        })
        .await
    }

    pub async fn create_limit_order_buy(
        &self,
        token_id: &str,
        size_shares: f64,
        price: f64,
        tick_size: &str,
    ) -> Result<SignedOrder> {
        let round = round_config_for_tick(tick_size)?;
        let fee_rate_bps = self.get_fee_rate_bps(token_id).await?;
        let neg_risk = self.get_neg_risk(token_id).await?;

        let (side, raw_maker_amount, raw_taker_amount) =
            get_limit_order_raw_amounts(OrderSide::Buy, size_shares, price, round)?;

        self.create_signed_order(SignOrderParams {
            token_id,
            side,
            raw_maker_amount,
            raw_taker_amount,
            fee_rate_bps,
            expiration: 0,
            nonce: 0,
            neg_risk,
        })
        .await
    }

    pub async fn create_limit_order_sell(
        &self,
        token_id: &str,
        size_shares: f64,
        price: f64,
        tick_size: &str,
    ) -> Result<SignedOrder> {
        let round = round_config_for_tick(tick_size)?;
        let fee_rate_bps = self.get_fee_rate_bps(token_id).await?;
        let neg_risk = self.get_neg_risk(token_id).await?;

        let (side, raw_maker_amount, raw_taker_amount) =
            get_limit_order_raw_amounts(OrderSide::Sell, size_shares, price, round)?;

        self.create_signed_order(SignOrderParams {
            token_id,
            side,
            raw_maker_amount,
            raw_taker_amount,
            fee_rate_bps,
            expiration: 0,
            nonce: 0,
            neg_risk,
        })
        .await
    }

    async fn create_signed_order(&self, params: SignOrderParams<'_>) -> Result<SignedOrder> {
        if !params.raw_maker_amount.is_finite() || params.raw_maker_amount <= 0.0 {
            bail!("invalid maker amount for signed order");
        }
        if !params.raw_taker_amount.is_finite() || params.raw_taker_amount <= 0.0 {
            bail!("invalid taker amount for signed order");
        }

        let maker_amount_raw = parse_units(decimal_to_string(params.raw_maker_amount, 8), 6)
            .context("failed to encode maker amount to base units")?
            .to_string();
        let taker_amount_raw = parse_units(decimal_to_string(params.raw_taker_amount, 8), 6)
            .context("failed to encode taker amount to base units")?
            .to_string();
        let maker_amount = normalize_uint256_string("makerAmount", &maker_amount_raw)?;
        let taker_amount = normalize_uint256_string("takerAmount", &taker_amount_raw)?;

        let salt = normalize_uint256_string("salt", &generate_order_salt())?;
        let maker = normalize_address_string("maker", &self.funder_address)?;
        let signer = normalize_address_string("signer", &self.signer_address)?;
        let taker = normalize_address_string("taker", ZERO_ADDRESS)?;
        let token_id = normalize_uint256_string("tokenId", params.token_id)?;
        let fee_rate_bps =
            normalize_uint256_string("feeRateBps", &params.fee_rate_bps.to_string())?;
        let expiration = normalize_uint256_string("expiration", &params.expiration.to_string())?;
        let nonce = normalize_uint256_string("nonce", &params.nonce.to_string())?;

        let verifying_contract = normalize_address_string(
            "verifyingContract",
            if params.neg_risk {
                NEG_RISK_EXCHANGE_POLYGON
            } else {
                EXCHANGE_POLYGON
            },
        )?;

        let typed_data_value = json!({
            "types": {
                "EIP712Domain": [
                    { "name": "name", "type": "string" },
                    { "name": "version", "type": "string" },
                    { "name": "chainId", "type": "uint256" },
                    { "name": "verifyingContract", "type": "address" }
                ],
                "Order": [
                    { "name": "salt", "type": "uint256" },
                    { "name": "maker", "type": "address" },
                    { "name": "signer", "type": "address" },
                    { "name": "taker", "type": "address" },
                    { "name": "tokenId", "type": "uint256" },
                    { "name": "makerAmount", "type": "uint256" },
                    { "name": "takerAmount", "type": "uint256" },
                    { "name": "expiration", "type": "uint256" },
                    { "name": "nonce", "type": "uint256" },
                    { "name": "feeRateBps", "type": "uint256" },
                    { "name": "side", "type": "uint8" },
                    { "name": "signatureType", "type": "uint8" }
                ]
            },
            "primaryType": "Order",
            "domain": {
                "name": "Polymarket CTF Exchange",
                "version": "1",
                "chainId": CHAIN_ID_POLYGON,
                "verifyingContract": verifying_contract,
            },
            "message": {
                "salt": salt,
                "maker": maker,
                "signer": signer,
                "taker": taker,
                "tokenId": token_id,
                "makerAmount": maker_amount,
                "takerAmount": taker_amount,
                "expiration": expiration,
                "nonce": nonce,
                "feeRateBps": fee_rate_bps,
                "side": params.side.as_u8(),
                "signatureType": self.signature_type,
            }
        });

        let typed_data: TypedData = serde_json::from_value(typed_data_value)
            .with_context(|| {
                format!(
                    "failed to build typed-data payload for order signature (tokenId={}, maker={}, signer={}, side={}, signatureType={})",
                    token_id,
                    maker,
                    signer,
                    params.side.as_u8(),
                    self.signature_type,
                )
            })?;

        let signature = self
            .wallet
            .sign_typed_data(&typed_data)
            .await
            .with_context(|| {
                format!(
                    "failed to sign order typed-data (tokenId={}, maker={}, signer={}, side={}, signatureType={})",
                    token_id,
                    maker,
                    signer,
                    params.side.as_u8(),
                    self.signature_type,
                )
            })?
            .to_string();

        Ok(SignedOrder {
            salt,
            maker,
            signer,
            taker,
            token_id,
            maker_amount,
            taker_amount,
            expiration,
            nonce,
            fee_rate_bps,
            side: params.side,
            signature_type: self.signature_type,
            signature,
        })
    }

    fn build_l2_headers(
        &self,
        method: &str,
        request_path: &str,
        body: Option<&str>,
    ) -> Result<HeaderMap> {
        let timestamp = Utc::now().timestamp();
        let timestamp_str = timestamp.to_string();

        let mut message = format!("{}{}{}", timestamp_str, method, request_path);
        if let Some(body_value) = body {
            message.push_str(body_value);
        }

        let secret_key = decode_api_secret(&self.api_secret)?;
        let signature = build_poly_hmac_signature(&secret_key, &message)?;

        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_bytes(b"POLY_ADDRESS")?,
            HeaderValue::from_str(&self.signer_address)
                .context("invalid POLY_ADDRESS header value")?,
        );
        headers.insert(
            HeaderName::from_bytes(b"POLY_SIGNATURE")?,
            HeaderValue::from_str(&signature).context("invalid POLY_SIGNATURE header value")?,
        );
        headers.insert(
            HeaderName::from_bytes(b"POLY_TIMESTAMP")?,
            HeaderValue::from_str(&timestamp_str).context("invalid POLY_TIMESTAMP header value")?,
        );
        headers.insert(
            HeaderName::from_bytes(b"POLY_API_KEY")?,
            HeaderValue::from_str(&self.api_key).context("invalid POLY_API_KEY header value")?,
        );
        headers.insert(
            HeaderName::from_bytes(b"POLY_PASSPHRASE")?,
            HeaderValue::from_str(&self.api_passphrase)
                .context("invalid POLY_PASSPHRASE header value")?,
        );

        Ok(headers)
    }
}

struct SignOrderParams<'a> {
    token_id: &'a str,
    side: OrderSide,
    raw_maker_amount: f64,
    raw_taker_amount: f64,
    fee_rate_bps: u64,
    expiration: u64,
    nonce: u64,
    neg_risk: bool,
}

fn round_config_for_tick(tick_size: &str) -> Result<RoundingConfig> {
    match tick_size.trim() {
        "0.1" => Ok(RoundingConfig {
            price_decimals: 1,
            size_decimals: 2,
            amount_decimals: 3,
        }),
        "0.01" => Ok(RoundingConfig {
            price_decimals: 2,
            size_decimals: 2,
            amount_decimals: 4,
        }),
        "0.001" => Ok(RoundingConfig {
            price_decimals: 3,
            size_decimals: 2,
            amount_decimals: 5,
        }),
        "0.0001" => Ok(RoundingConfig {
            price_decimals: 4,
            size_decimals: 2,
            amount_decimals: 6,
        }),
        other => bail!("unsupported tick size for signing: {other}"),
    }
}

fn get_market_order_raw_amounts(
    side: OrderSide,
    amount: f64,
    price: f64,
    round: RoundingConfig,
) -> Result<(OrderSide, f64, f64)> {
    if !amount.is_finite() || amount <= 0.0 {
        bail!("invalid market order amount");
    }
    if !price.is_finite() || price <= 0.0 {
        bail!("invalid market order price");
    }

    let raw_price = round_down(price, round.price_decimals);

    if side == OrderSide::Buy {
        let raw_maker = round_down(amount, round.size_decimals);
        let mut raw_taker = raw_maker / raw_price;

        if decimal_places(raw_taker) > round.amount_decimals {
            raw_taker = round_up(raw_taker, round.amount_decimals + 4);
            if decimal_places(raw_taker) > round.amount_decimals {
                raw_taker = round_down(raw_taker, round.amount_decimals);
            }
        }

        return Ok((OrderSide::Buy, raw_maker, raw_taker));
    }

    let raw_maker = round_down(amount, round.size_decimals);
    let mut raw_taker = raw_maker * raw_price;
    if decimal_places(raw_taker) > round.amount_decimals {
        raw_taker = round_up(raw_taker, round.amount_decimals + 4);
        if decimal_places(raw_taker) > round.amount_decimals {
            raw_taker = round_down(raw_taker, round.amount_decimals);
        }
    }

    Ok((OrderSide::Sell, raw_maker, raw_taker))
}

fn get_limit_order_raw_amounts(
    side: OrderSide,
    size: f64,
    price: f64,
    round: RoundingConfig,
) -> Result<(OrderSide, f64, f64)> {
    if !size.is_finite() || size <= 0.0 {
        bail!("invalid limit order size");
    }
    if !price.is_finite() || price <= 0.0 {
        bail!("invalid limit order price");
    }

    let raw_price = round_normal(price, round.price_decimals);

    if side == OrderSide::Buy {
        let raw_taker = round_down(size, round.size_decimals);
        let mut raw_maker = raw_taker * raw_price;

        if decimal_places(raw_maker) > round.amount_decimals {
            raw_maker = round_up(raw_maker, round.amount_decimals + 4);
            if decimal_places(raw_maker) > round.amount_decimals {
                raw_maker = round_down(raw_maker, round.amount_decimals);
            }
        }

        return Ok((OrderSide::Buy, raw_maker, raw_taker));
    }

    let raw_maker = round_down(size, round.size_decimals);
    let mut raw_taker = raw_maker * raw_price;
    if decimal_places(raw_taker) > round.amount_decimals {
        raw_taker = round_up(raw_taker, round.amount_decimals + 4);
        if decimal_places(raw_taker) > round.amount_decimals {
            raw_taker = round_down(raw_taker, round.amount_decimals);
        }
    }

    Ok((OrderSide::Sell, raw_maker, raw_taker))
}

fn round_normal(value: f64, decimals: u32) -> f64 {
    let factor = 10_f64.powi(decimals as i32);
    (value * factor).round() / factor
}

fn round_down(value: f64, decimals: u32) -> f64 {
    let factor = 10_f64.powi(decimals as i32);
    (value * factor).floor() / factor
}

fn round_up(value: f64, decimals: u32) -> f64 {
    let factor = 10_f64.powi(decimals as i32);
    (value * factor).ceil() / factor
}

fn decimal_places(value: f64) -> u32 {
    if !value.is_finite() {
        return 0;
    }

    let mut text = format!("{value:.12}");
    while text.ends_with('0') {
        text.pop();
    }
    if text.ends_with('.') {
        text.pop();
    }

    if let Some((_, frac)) = text.split_once('.') {
        frac.len() as u32
    } else {
        0
    }
}

fn decimal_to_string(value: f64, decimals: usize) -> String {
    let mut text = format!("{value:.decimals$}");
    while text.ends_with('0') {
        text.pop();
    }
    if text.ends_with('.') {
        text.pop();
    }

    if text.is_empty() {
        "0".to_owned()
    } else {
        text
    }
}

fn normalize_tick_size_string(tick: f64) -> String {
    let mut text = format!("{tick:.8}");
    while text.ends_with('0') {
        text.pop();
    }
    if text.ends_with('.') {
        text.pop();
    }
    if text.is_empty() {
        "0.01".to_owned()
    } else {
        text
    }
}

fn normalize_uint256_string(field_name: &str, raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("{field_name} must not be empty");
    }

    let value = if let Some(hex) = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
    {
        U256::from_str_radix(hex, 16)
            .with_context(|| format!("{field_name} must be valid hex uint256, got: {trimmed}"))?
    } else {
        U256::from_dec_str(trimmed).with_context(|| {
            format!("{field_name} must be valid decimal uint256, got: {trimmed}")
        })?
    };

    Ok(value.to_string())
}

fn normalize_address_string(field_name: &str, raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("{field_name} must not be empty");
    }

    let address = Address::from_str(trimmed)
        .with_context(|| format!("{field_name} must be a valid 0x address, got: {trimmed}"))?;

    Ok(format!("{:#x}", address))
}

fn generate_order_salt() -> String {
    let now_ms = Utc::now().timestamp_millis().max(0) as u128;
    let pid = std::process::id() as u128;
    (now_ms * 1_000 + (pid % 1_000)).to_string()
}

fn decode_api_secret(secret: &str) -> Result<Vec<u8>> {
    let sanitized: String = secret
        .trim()
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(*ch, '+' | '/' | '=' | '-' | '_'))
        .collect();

    if sanitized.is_empty() {
        bail!("POLYMARKET_API_SECRET is empty");
    }

    if let Ok(decoded) = STANDARD.decode(&sanitized) {
        return Ok(decoded);
    }
    if let Ok(decoded) = STANDARD_NO_PAD.decode(&sanitized) {
        return Ok(decoded);
    }
    if let Ok(decoded) = URL_SAFE.decode(&sanitized) {
        return Ok(decoded);
    }
    if let Ok(decoded) = URL_SAFE_NO_PAD.decode(&sanitized) {
        return Ok(decoded);
    }

    let normalized = sanitized.replace('-', "+").replace('_', "/");
    if let Ok(decoded) = STANDARD.decode(&normalized) {
        return Ok(decoded);
    }
    if let Ok(decoded) = STANDARD_NO_PAD.decode(&normalized) {
        return Ok(decoded);
    }

    bail!("POLYMARKET_API_SECRET is not valid base64/base64url")
}

fn build_poly_hmac_signature(secret_bytes: &[u8], message: &str) -> Result<String> {
    let mut mac = HmacSha256::new_from_slice(secret_bytes)
        .map_err(|_| anyhow!("failed to initialize HMAC-SHA256"))?;
    mac.update(message.as_bytes());
    let sig = mac.finalize().into_bytes();

    let base64 = STANDARD.encode(sig);
    Ok(base64.replace('+', "-").replace('/', "_"))
}

fn value_to_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn first_amount_string(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => {
            let trimmed = text.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_owned())
            }
        }
        Value::Number(number) => Some(number.to_string()),
        Value::Array(items) => items.iter().find_map(first_amount_string),
        Value::Object(map) => {
            let preferred_keys = [
                "amount",
                "value",
                "raw",
                "balance",
                "allowance",
                "available",
                "approved",
            ];

            for key in preferred_keys {
                if let Some(found) = map.get(key).and_then(first_amount_string) {
                    return Some(found);
                }
            }

            map.values().find_map(first_amount_string)
        }
        _ => None,
    }
}

fn read_amount_from_object(object: &Map<String, Value>, keys: &[&str]) -> Option<String> {
    for key in keys {
        if let Some(found) = object.get(*key).and_then(first_amount_string) {
            return Some(found);
        }
    }

    None
}

fn read_amount_from_payload(payload: &Value, keys: &[&str]) -> Option<String> {
    if let Some(object) = payload.as_object() {
        if let Some(found) = read_amount_from_object(object, keys) {
            return Some(found);
        }

        if let Some(data) = object.get("data").and_then(Value::as_object) {
            if let Some(found) = read_amount_from_object(data, keys) {
                return Some(found);
            }
        }

        if let Some(result) = object.get("result").and_then(Value::as_object) {
            if let Some(found) = read_amount_from_object(result, keys) {
                return Some(found);
            }
        }
    }

    None
}

fn parse_balance_allowance_payload(payload: &Value) -> Result<(String, String)> {
    let balance_keys = ["balance", "available", "available_balance", "balance_raw"];
    let allowance_keys = [
        "allowance",
        "allowances",
        "approved",
        "approval",
        "available_allowance",
    ];

    let balance_raw = read_amount_from_payload(payload, &balance_keys)
        .ok_or_else(|| anyhow!("balance-allowance payload missing balance"))?;

    let allowance_raw =
        read_amount_from_payload(payload, &allowance_keys).unwrap_or_else(|| balance_raw.clone());

    Ok((balance_raw, allowance_raw))
}

fn parse_balance_allowance_units(field_name: &str, raw: &str) -> Result<U256> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("{field_name} must not be empty");
    }

    if trimmed.contains('.') {
        let parsed = parse_units(trimmed, 6)
            .with_context(|| format!("{field_name} must be valid decimal amount, got: {raw}"))?;
        return Ok(parsed.into());
    }

    if let Ok(parsed) = U256::from_dec_str(trimmed) {
        return Ok(parsed);
    }

    let parsed = parse_units(trimmed, 6)
        .with_context(|| format!("{field_name} must be valid amount, got: {raw}"))?;
    Ok(parsed.into())
}

fn truncate_text(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }

    value.chars().take(max_chars).collect()
}

fn value_to_u64(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number
            .as_u64()
            .or_else(|| number.as_i64().filter(|v| *v >= 0).map(|v| v as u64)),
        Value::String(text) => text.trim().parse::<u64>().ok(),
        _ => None,
    }
}

pub fn create_clob_client(config: &Config) -> Result<ClobClient> {
    if config.private_key.trim().is_empty() {
        bail!("private key is required for live trading client");
    }
    if config.funder_address.trim().is_empty() {
        bail!("funder address is required for live trading client");
    }
    if config.api_key.trim().is_empty() {
        bail!("POLYMARKET_API_KEY is required for live trading client");
    }
    if config.api_secret.trim().is_empty() {
        bail!("POLYMARKET_API_SECRET is required for live trading client");
    }
    if config.api_passphrase.trim().is_empty() {
        bail!("POLYMARKET_API_PASSPHRASE is required for live trading client");
    }

    let wallet = config
        .private_key
        .parse::<LocalWallet>()
        .context("invalid private key format for LocalWallet")?
        .with_chain_id(CHAIN_ID_POLYGON);

    let signer_address = format!("{:#x}", wallet.address());
    let signature_type = match config.signature_type {
        SignatureType::Eoa => 0,
        SignatureType::Safe => 1,
        SignatureType::SmartContractWallet => 2,
    };

    let http_client = Client::builder()
        .timeout(Duration::from_millis(12_000))
        .build()
        .context("failed to create reqwest client")?;

    Ok(ClobClient {
        host: config.polymarket_clob_url.trim_end_matches('/').to_owned(),
        wallet,
        signer_address,
        funder_address: normalize_address_string("FUNDER_ADDRESS", &config.funder_address)?,
        signature_type,
        api_key: config.api_key.trim().to_owned(),
        api_secret: config.api_secret.trim().to_owned(),
        api_passphrase: config.api_passphrase.trim().to_owned(),
        http_client,
    })
}

pub fn get_clob_client(config: &Config) -> Result<ClobClient> {
    create_clob_client(config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_uint256_decimal() {
        let value = normalize_uint256_string("tokenId", "123456789").unwrap();
        assert_eq!(value, "123456789");
    }

    #[test]
    fn normalizes_uint256_hex() {
        let value = normalize_uint256_string("tokenId", "0x10").unwrap();
        assert_eq!(value, "16");
    }

    #[test]
    fn rejects_invalid_uint256() {
        let error = normalize_uint256_string("tokenId", "not-a-number").unwrap_err();
        let message = error.to_string();
        assert!(message.contains("tokenId must be valid"));
    }

    #[test]
    fn normalizes_address() {
        let value = normalize_address_string("maker", "0x0000000000000000000000000000000000000001")
            .unwrap();
        assert_eq!(
            value.to_lowercase(),
            "0x0000000000000000000000000000000000000001"
        );
    }

    #[test]
    fn parses_balance_allowance_units_from_fixed_int() {
        let parsed = parse_balance_allowance_units("balance", "1050000").unwrap();
        assert_eq!(parsed.to_string(), "1050000");
    }

    #[test]
    fn parses_balance_allowance_units_from_decimal() {
        let parsed = parse_balance_allowance_units("balance", "1.05").unwrap();
        assert_eq!(parsed.to_string(), "1050000");
    }

    #[test]
    fn parses_balance_allowance_from_nested_data_shape() {
        let payload = json!({
            "data": {
                "balance": "1050000",
                "allowance": "1000000"
            }
        });

        let (balance, allowance) = parse_balance_allowance_payload(&payload).unwrap();
        assert_eq!(balance, "1050000");
        assert_eq!(allowance, "1000000");
    }

    #[test]
    fn falls_back_allowance_to_balance_when_allowance_missing() {
        let payload = json!({
            "balance": "1050000"
        });

        let (balance, allowance) = parse_balance_allowance_payload(&payload).unwrap();
        assert_eq!(balance, "1050000");
        assert_eq!(allowance, "1050000");
    }
}
