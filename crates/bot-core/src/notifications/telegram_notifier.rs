use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use reqwest::Client;
use serde::Deserialize;
use serde_json::json;
use tokio::time::sleep;

use crate::types::Config;
use crate::utils::logger::{log_info, log_warn};

const TELEGRAM_SEND_TIMEOUT_MS: u64 = 10_000;
const TELEGRAM_UPDATE_TIMEOUT_MS: u64 = 35_000;
const TELEGRAM_POLL_BACKOFF_MS: u64 = 2_000;
const TELEGRAM_MAX_MESSAGE_LENGTH: usize = 3_800;

#[derive(Debug, Deserialize)]
struct TelegramChat {
    id: i64,
}

#[derive(Debug, Deserialize)]
struct TelegramMessage {
    chat: TelegramChat,
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TelegramUpdate {
    update_id: i64,
    message: Option<TelegramMessage>,
}

#[derive(Debug, Deserialize)]
struct TelegramGetUpdatesPayload {
    ok: bool,
    result: Option<Vec<TelegramUpdate>>,
}

fn clip(text: &str, max_length: usize) -> String {
    if text.chars().count() <= max_length {
        return text.to_owned();
    }

    let clipped: String = text.chars().take(max_length.saturating_sub(3)).collect();
    format!("{clipped}...")
}

fn parse_telegram_command(text: Option<&str>) -> Option<String> {
    let raw = text?.trim();
    if raw.is_empty() {
        return None;
    }

    raw.split_whitespace().next().map(|value| value.to_lowercase())
}

#[derive(Clone)]
pub struct TelegramNotifier {
    pub enabled: bool,
    client: Client,
    bot_token: Option<String>,
    chat_id: Option<String>,
    paused: Arc<AtomicBool>,
    listener_started: Arc<AtomicBool>,
}

impl TelegramNotifier {
    fn set_paused_state(&self, paused: bool, source: &str, reason: Option<&str>) -> bool {
        let previous = self.paused.swap(paused, Ordering::SeqCst);
        let changed = previous != paused;
        if !changed {
            return false;
        }

        let suffix = reason
            .filter(|value| !value.is_empty())
            .map(|value| format!(" reason={}", clip(value, 220)))
            .unwrap_or_default();

        log_info(
            "Telegram",
            &format!(
                "trading {} by {}{}",
                if paused { "paused" } else { "resumed" },
                source,
                suffix
            ),
        );

        true
    }

    pub async fn send(&self, message: &str) -> Result<()> {
        if !self.enabled {
            return Ok(());
        }

        let Some(token) = &self.bot_token else {
            return Ok(());
        };
        let Some(chat_id) = &self.chat_id else {
            return Ok(());
        };

        let url = format!("https://api.telegram.org/bot{token}/sendMessage");
        let response = self
            .client
            .post(url)
            .json(&json!({
                "chat_id": chat_id,
                "text": clip(message, TELEGRAM_MAX_MESSAGE_LENGTH),
                "disable_web_page_preview": true,
            }))
            .timeout(Duration::from_millis(TELEGRAM_SEND_TIMEOUT_MS))
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            log_warn(
                "Telegram",
                &format!(
                    "send failed http={} body={}",
                    status,
                    clip(&body, 200)
                ),
            );
        }

        Ok(())
    }

    async fn run_command_poll_loop(self) {
        if !self.enabled {
            return;
        }

        let Some(token) = self.bot_token.clone() else {
            return;
        };
        let Some(chat_id) = self.chat_id.clone() else {
            return;
        };

        let mut next_update_offset: Option<i64> = None;
        let mut update_offset_primed = false;

        loop {
            let base_url = format!("https://api.telegram.org/bot{token}/getUpdates");
            let mut request = self.client.get(&base_url).query(&[("timeout", "25")]);

            if let Some(offset) = next_update_offset {
                request = request.query(&[("offset", offset)]);
            }

            let response = match request
                .timeout(Duration::from_millis(TELEGRAM_UPDATE_TIMEOUT_MS))
                .send()
                .await
            {
                Ok(value) => value,
                Err(error) => {
                    log_warn(
                        "Telegram",
                        &format!("getUpdates failed: {}", clip(&error.to_string(), 200)),
                    );
                    sleep(Duration::from_millis(TELEGRAM_POLL_BACKOFF_MS)).await;
                    continue;
                }
            };

            if !response.status().is_success() {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                log_warn(
                    "Telegram",
                    &format!(
                        "getUpdates failed http={} body={}",
                        status,
                        clip(&body, 200)
                    ),
                );
                sleep(Duration::from_millis(TELEGRAM_POLL_BACKOFF_MS)).await;
                continue;
            }

            let payload: TelegramGetUpdatesPayload = match response.json().await {
                Ok(value) => value,
                Err(error) => {
                    log_warn(
                        "Telegram",
                        &format!(
                            "getUpdates decode failed: {}",
                            clip(&error.to_string(), 200)
                        ),
                    );
                    sleep(Duration::from_millis(TELEGRAM_POLL_BACKOFF_MS)).await;
                    continue;
                }
            };

            if !payload.ok {
                sleep(Duration::from_millis(TELEGRAM_POLL_BACKOFF_MS)).await;
                continue;
            }

            let updates = payload.result.unwrap_or_default();

            if !update_offset_primed {
                update_offset_primed = true;
                if let Some(last) = updates.last() {
                    next_update_offset = Some(last.update_id + 1);
                    continue;
                }
            }

            for update in updates {
                next_update_offset = Some(update.update_id + 1);

                let Some(message) = update.message else {
                    continue;
                };

                if message.chat.id.to_string() != chat_id {
                    continue;
                }

                let command = parse_telegram_command(message.text.as_deref());

                if command.as_deref() == Some("/pause") {
                    if self.set_paused_state(true, "command", None) {
                        let _ = self
                            .send(
                                "[POLYMARKET BOT CONTROL]\nTrading paused (use /resume to continue).",
                            )
                            .await;
                    } else {
                        let _ = self
                            .send("[POLYMARKET BOT CONTROL]\nTrading is already paused.")
                            .await;
                    }
                    continue;
                }

                if command.as_deref() == Some("/resume") {
                    if self.set_paused_state(false, "command", None) {
                        let _ = self.send("[POLYMARKET BOT CONTROL]\nTrading resumed.").await;
                    } else {
                        let _ = self
                            .send("[POLYMARKET BOT CONTROL]\nTrading is already running.")
                            .await;
                    }
                    continue;
                }

                if command.as_deref() == Some("/status") {
                    let _ = self
                        .send(&format!(
                            "[POLYMARKET BOT CONTROL]\nTrading status: {}.\nCommands: /pause /resume /status",
                            if self.is_paused() { "PAUSED" } else { "RUNNING" }
                        ))
                        .await;
                }
            }
        }
    }

    pub fn start_command_listener(&self) {
        if !self.enabled {
            return;
        }

        if self.listener_started.swap(true, Ordering::SeqCst) {
            return;
        }

        log_info(
            "Telegram",
            "command listener started (supports /pause, /resume, /status)",
        );

        let notifier = self.clone();
        tokio::spawn(async move {
            notifier.run_command_poll_loop().await;
        });
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    pub async fn set_paused(&self, paused: bool, reason: Option<&str>) -> Result<bool> {
        let changed = self.set_paused_state(paused, "system", reason);
        if !changed {
            return Ok(false);
        }

        let reason_line = reason
            .filter(|value| !value.trim().is_empty())
            .map(|value| format!("\nReason: {}", clip(value, 220)))
            .unwrap_or_default();

        if paused {
            let _ = self
                .send(&format!(
                    "[POLYMARKET BOT CONTROL]\nTrading paused (use /resume to continue).{}",
                    reason_line
                ))
                .await;
        } else {
            let _ = self
                .send(&format!(
                    "[POLYMARKET BOT CONTROL]\nTrading resumed.{}",
                    reason_line
                ))
                .await;
        }

        Ok(changed)
    }
}

pub fn create_telegram_notifier(config: &Config) -> TelegramNotifier {
    let enabled = config.telegram_bot_token.is_some() && config.telegram_chat_id.is_some();

    TelegramNotifier {
        enabled,
        client: Client::new(),
        bot_token: config.telegram_bot_token.clone(),
        chat_id: config.telegram_chat_id.clone(),
        paused: Arc::new(AtomicBool::new(false)),
        listener_started: Arc::new(AtomicBool::new(false)),
    }
}
