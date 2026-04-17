use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Result;
use reqwest::Client;
use serde_json::json;

use crate::types::V3Config;
use crate::utils::logger::log_warn;

#[derive(Clone)]
pub struct TelegramNotifier {
    pub enabled: bool,
    client: Client,
    bot_token: Option<String>,
    chat_id: Option<String>,
    paused: Arc<AtomicBool>,
}

impl TelegramNotifier {
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
        self.client
            .post(url)
            .json(&json!({
                "chat_id": chat_id,
                "text": message,
            }))
            .timeout(std::time::Duration::from_millis(8000))
            .send()
            .await?;

        Ok(())
    }

    pub fn start_command_listener(&self) {
        if self.enabled {
            log_warn(
                "Telegram",
                "Command listener scaffold exists but polling loop is not implemented yet.",
            );
        }
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    pub async fn set_paused(&self, paused: bool, reason: Option<&str>) -> Result<bool> {
        let previous = self.paused.swap(paused, Ordering::SeqCst);
        let changed = previous != paused;

        if changed {
            let reason_text = reason.unwrap_or("manual command");
            let state = if paused { "PAUSED" } else { "RESUMED" };
            let _ = self
                .send(&format!("[BOT-RUST] trading {state}. reason={reason_text}"))
                .await;
        }

        Ok(changed)
    }
}

pub fn create_telegram_notifier(config: &V3Config) -> TelegramNotifier {
    let enabled = config.telegram_bot_token.is_some() && config.telegram_chat_id.is_some();

    TelegramNotifier {
        enabled,
        client: Client::new(),
        bot_token: config.telegram_bot_token.clone(),
        chat_id: config.telegram_chat_id.clone(),
        paused: Arc::new(AtomicBool::new(false)),
    }
}
